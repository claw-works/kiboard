// kiboard firmware v4：4x4 矩阵键盘 + SSD1306 OLED + 3 路 LED + Wi-Fi + JSON Lines
//
// 引脚以 docs/pinmap.md 的 v4 为准，全部经面包板实测。一句话铁律：
//   SuperMini 在三个 strapping 脚 GPIO2/8/9 上带板载上拉，压不掉，
//   这三个脚只能当输出。所以矩阵的 8 根线走 GPIO0/1/21/3 + GPIO6/7/10/20，
//   三个 LED 占 GPIO9/2/8。
#include <Arduino.h>

// 版本号：语义版本手工维护（表达"功能到哪了"），git 描述由 version.py 构建期注入
// （表达"板子上到底是哪份代码"）。两者用途不同，都要。
#define KIBOARD_FW_VERSION "0.5.0"
#ifndef KIBOARD_FW_GIT
#define KIBOARD_FW_GIT "nogit"
#endif
#ifndef KIBOARD_FW_DATE
#define KIBOARD_FW_DATE "unknown"
#endif
#include <ArduinoJson.h>
#include <WiFi.h>
#include <WiFiMulti.h>
#include "display.h"
#include "hublink.h"
#include "secrets.h"

// ---------- 4x4 矩阵键盘 ----------
// 扫描某行时：该行 OUTPUT 拉高，其余三行 INPUT_PULLDOWN，列脚 INPUT_PULLDOWN。
// 空闲行不能用纯 INPUT（带上拉的脚会悬空成高，把高电平灌进列线造成整列误报），
// 也不能 OUTPUT 拉低（同列双键按下会推挽对推挽短路）。
constexpr uint8_t ROW_PINS[4] = {0, 1, 21, 3};      // R1..R4
constexpr uint8_t COL_PINS[4] = {6, 7, 10, 20};     // C1..C4
constexpr size_t KEY_COUNT = 16;                    // id = row*4 + col
constexpr uint32_t SETTLE_US = 50;                  // 切换行后的电平稳定时间
constexpr uint32_t DEBOUNCE_MS = 30;                // 电平稳定确认时间
constexpr uint32_t EVENT_COOLDOWN_MS = 250;         // 一次状态切换后的冷却，滤掉触点弹跳成串
constexpr uint32_t LONG_PRESS_MS = 600;

struct KeyState {
    bool stable = false;
    bool lastRaw = false;
    uint32_t lastEdge = 0;
    uint32_t lastEvent = 0;
    uint32_t pressAt = 0;
    bool longSent = false;
    // 息屏唤醒的那一次按键要整段吞掉（press/long/release 都不发）
    bool swallow = false;
};
KeyState keyStates[KEY_COUNT];

static void rowsIdle() {
    for (uint8_t pin : ROW_PINS) pinMode(pin, INPUT_PULLDOWN);
}

// 读一行的四个列脚
static void readRow(size_t r, bool out[4]) {
    pinMode(ROW_PINS[r], OUTPUT);
    digitalWrite(ROW_PINS[r], HIGH);
    delayMicroseconds(SETTLE_US);
    for (size_t c = 0; c < 4; c++) out[c] = digitalRead(COL_PINS[c]) == HIGH;
    pinMode(ROW_PINS[r], INPUT_PULLDOWN);
}

// ---------- LED ----------
// id0: GPIO9  黄，需批准（反接，低电平点亮）
// id1: GPIO2  红，出错（正接，高电平点亮）
// id2: GPIO8  板载蓝，心跳（低电平点亮）
struct Led {
    uint8_t pin;
    bool activeLow;
    uint8_t mode;     // 0=off 1=on 2=blink
    uint32_t halfMs;  // blink 半周期
};
Led leds[] = {
    {9, true, 0, 250},
    {2, false, 0, 250},
    {8, true, 2, 500},  // 默认心跳，hub 可覆盖
};
constexpr size_t LED_COUNT = sizeof(leds) / sizeof(leds[0]);

// ---------- Wi-Fi ----------
WiFiMulti wifiMulti;
constexpr uint32_t WIFI_PER_TRY_MS = 8000;
constexpr uint32_t WIFI_CHECK_MS = 2000;
constexpr uint32_t WIFI_RETRY_MS = 15000;
static bool wifiWasConnected = false;
static uint32_t lastWifiCheck = 0;
static uint32_t lastWifiAttempt = 0;

// ---------- 屏幕消息区的归属 ----------
// 消息区有两个写入者：固件本地的按键回显，和 hub 下发的 msg。
// hub 在裁决后会写 ACCEPTED，而同一次按压随后的 release 又被固件回显成 R1C1 release，
// 把结果覆盖掉——实机上看到的就是 release 而不是 ACCEPTED。
// 规则：一旦确认有 hub 在驱动屏幕（近期收到过它的指令），消息区就交给 hub，
// 固件不再自行回显。脱机自测时（没有 hub）保留回显，那时它是有用的。
static uint32_t gLastHubCmdMs = 0;
constexpr uint32_t HUB_PRESENT_MS = 30000;

static bool hubDrivingScreen() {
    return gLastHubCmdMs != 0 && millis() - gLastHubCmdMs < HUB_PRESENT_MS;
}

// hub 临时占用正文区（msg / text）的截止时间。
//
// 和 hubDrivingScreen 分开是必须的：后者的语义是"hub 在线所以别本地回显按键"，
// 判定窗口 30 秒。要是拿它来挡首屏重绘，hub 发一条 op=clock（本意是"把屏交回首屏"）
// 也会算作 hub 指令，于是首屏 30 秒不画——屏幕直接黑着。
// 这个变量只被真正画正文的指令置位，被 clock/home/msg_clear 立刻清零。
static uint32_t gHubBodyUntilMs = 0;
constexpr uint32_t HUB_BODY_HOLD_MS = 5000;

static bool hubOwnsBody() {
    return gHubBodyUntilMs != 0 && millis() < gHubBodyUntilMs;
}

// ---------- 顶栏角标 ----------
// hub 用 {"t":"disp","op":"badge","text":"AUTO 9m"} 设置。
// 「自动接受中」这类状态必须常驻可见——一个静默放行一切的模式如果看不见，就是陷阱。
static char gBadge[16] = "";

// ---------- 首屏与息屏 ----------
//
// 待机时显示的不再是一屏大时钟，而是分页的首屏（logo / 任务 / 帮助两页）。
// 时钟缩到标题栏右侧、只到分钟——秒在待机屏上没有信息量，还害得整屏每秒重绘一次。
//
// 页码由固件持有而不是 hub：翻页在 hub 离线时也必须能用，那时恰恰最需要看帮助页。
constexpr uint32_t HOME_TICK_MS = 200;
static bool ntpSynced = false;
static uint32_t lastHomeDraw = 0;
static int lastDrawnMin = -1;
static uint8_t gHomePage = 0;
static bool gHomeDirty = true;  // 翻页、任务变化、唤醒后强制重绘
// 任务屏（按 4）：临时占屏，几秒后自动回轮播。
// 数据本来就在设备上，所以这一屏完全本地，按下即出、不等网络往返
static uint32_t gTasksViewUntilMs = 0;
constexpr uint32_t TASKS_VIEW_MS = 8000;
// 当前全屏视图是否可退。审批屏不可退——* 不能把一条等着裁决的请求顶掉。
// hub 在 status 指令里用 transient 标注；老 hub 不带这个字段时默认 false，
// 也就是当审批屏处理：宁可多留一屏，不能弄丢一条请求
static bool gFullscreenTransient = false;

// 息屏：固件侧计时。
// 放固件而不是 hub 的理由有两条：hub 离线也要能省屏；按键唤醒不该等一个网络往返。
constexpr uint32_t SCREEN_OFF_MS = 5UL * 60UL * 1000UL;
static uint32_t gLastActivityMs = 0;
static bool gAutoOff = false;  // 是否由自动息屏关的（区别于用户按 * 主动关）

// ---------- 任务列表 ----------
// hub 下发 {"t":"disp","op":"tasks","items":["...","..."],"total":n}
static char gTasks[display::MAX_TASKS][40];
static int gTaskCount = 0;
static int gTaskTotal = 0;

// hub 在 hello 的回执里告诉设备自己的版本，显示在 logo 页。
// 目的和 /health 带版本一样：一眼看出设备连的 hub 是哪一版，不用猜
static char gHubVersion[24] = "";

static void startNtp() {
    configTzTime("CST-8", "ntp.aliyun.com", "cn.pool.ntp.org", "pool.ntp.org");
}

// 点亮屏幕并回到首屏。
//
// 必须显式 leaveFullscreen：0/5/6 那几屏是 statusScreen 画的，会置 fullscreen 标志。
// 息屏再唤醒时 OLED 缓冲里还留着那一屏，而 fullscreen 标志又让首屏不重绘，
// 结果就是"按 * 灭、再按 * 又回到 INFO、再等一会儿才回 logo"——
// 用户实测撞到的正是这个，最后那一步其实是 hub 的 8 秒定时器把屏收走的，
// 不是第三次按键起了作用。
//
// 有待批请求时会不会被误清？不会：* 键的按下照样上报给 hub，
// 而 hub 的 toggle_screen 在点亮后会重画当前请求。这里清掉的只是过期的查询屏。
// 只点亮，不改变当前显示的是哪一层。给"审批请求到了必须看得见"用
static void lightOn() {
    display::backlight(true);
    gAutoOff = false;
    gLastActivityMs = millis();
    gHomeDirty = true;
}

static void wakeScreen() {
    display::backlight(true);
    display::leaveFullscreen();
    gFullscreenTransient = false;
    gAutoOff = false;
    gTasksViewUntilMs = 0;
    gHomePage = 0;  // 唤醒后总是回到 logo 页
    gLastActivityMs = millis();
    gHomeDirty = true;
    // 让 hub 补画真正该显示的东西。这里先画 logo 是为了给即时反馈，
    // 但如果此刻有待批请求，那才是该占屏的内容——由 hub 盖上来，
    // 固件不需要自己记住"刚才屏上是什么"
    JsonDocument doc;
    doc["t"] = "repaint";
    hublink::send(doc);
}

// * 键：退一层，退到顶就熄屏。
//
//   logo 页        -> 熄屏
//   帮助页/任务屏   -> 回 logo 页
//   查询屏(0/5/6)  -> 回 logo 页
//   审批屏         -> 不动。那是一条等着裁决的请求，退掉等于让它悄悄消失；
//                     真要暗屏可以等 hub 那边的超时，或者先裁决
//   息屏           -> 点亮（走通用的任意键唤醒，不到这里）
//
// 整个逻辑放固件：它依赖"现在显示的是哪一层"，只有固件知道；
// 而且 hub 离线时也该能用。
static void starKey() {
    if (display::fullscreenActive()) {
        if (!gFullscreenTransient) {
            return;  // 审批屏，不退
        }
        display::leaveFullscreen();
        gFullscreenTransient = false;
        gHomePage = 0;
        gHomeDirty = true;
        return;
    }
    if (gTasksViewUntilMs != 0 || gHomePage != 0) {
        gTasksViewUntilMs = 0;
        gHomePage = 0;
        gHomeDirty = true;
        return;
    }
    // 已经在 logo 页，这就是顶层
    display::backlight(false);
}

// 记一次"人还在"。按键、hub 来消息都算
static void noteActivity() {
    gLastActivityMs = millis();
    if (gAutoOff) {
        // 自动息屏后来了活动就自动亮回来。用户按 * 主动关的不在此列——
        // 主动关就该一直关到他再按一次，否则那个键等于没用
        display::backlight(true);
        gAutoOff = false;
        gHomeDirty = true;
    }
}

static void updateHome() {
    uint32_t now = millis();
    if (now - lastHomeDraw < HOME_TICK_MS) return;
    lastHomeDraw = now;

    // 自动息屏。审批界面亮着时绝不息屏——那是在等人做决定
    if (display::backlightOn() && !display::fullscreenActive() &&
        now - gLastActivityMs > SCREEN_OFF_MS) {
        display::backlight(false);
        gAutoOff = true;
        return;
    }
    if (!display::backlightOn()) return;          // 息屏时不刷，白耗 I2C 带宽
    if (display::fullscreenActive()) return;      // 审批界面占屏时不要糊上去
    if (hubOwnsBody()) return;                    // hub 刚写了一条消息，让它显示几秒

    time_t t = time(nullptr);
    struct tm tmv;
    localtime_r(&t, &tmv);
    if (!ntpSynced && tmv.tm_year + 1900 > 2020) ntpSynced = true;
    // 分钟没变且没别的变化就不重绘
    if (tmv.tm_min == lastDrawnMin && !gHomeDirty) return;
    lastDrawnMin = tmv.tm_min;
    gHomeDirty = false;

    char hhmm[8];
    if (ntpSynced) {
        strftime(hhmm, sizeof(hhmm), "%H:%M", &tmv);
    } else {
        snprintf(hhmm, sizeof(hhmm), "--:--");  // 没对时就别装作知道时间
    }

    char link[24];
    if (WiFi.status() == WL_CONNECTED) {
        snprintf(link, sizeof(link), "wifi %ddBm", WiFi.RSSI());
    } else {
        snprintf(link, sizeof(link), "usb only");
    }

    display::Home info{};
    // 标题栏左侧只写 kiboard：链路状态在 logo 页有专门一行（wifi -41dBm / usb only），
    // 标题栏上再挂个 *hub 属于同一信息说两遍，还占掉本就不多的宽度
    info.label = gBadge[0] != '\0' ? gBadge : "kiboard";
    info.labelHighlight = gBadge[0] != '\0';
    info.hhmm = hhmm;
    info.link = link;
    info.hubVersion = gHubVersion;
    info.taskCount = gTaskCount;
    info.taskTotal = gTaskTotal;
    for (int i = 0; i < gTaskCount; i++) info.tasks[i] = gTasks[i];

    if (gTasksViewUntilMs != 0) {
        if (millis() < gTasksViewUntilMs) {
            display::tasksScreen(info);
            return;
        }
        gTasksViewUntilMs = 0;  // 到时间了，落回轮播
    }
    display::homeScreen(gHomePage, info);
}

// ---------- 消息收发 ----------
static void sendJson(JsonDocument &doc) { hublink::send(doc); }

static void sendHello() {
    JsonDocument doc;
    doc["t"] = "hello";
    doc["fw"] = KIBOARD_FW_VERSION;
    doc["fw_git"] = KIBOARD_FW_GIT;
    doc["keys"] = KEY_COUNT;
    doc["leds"] = LED_COUNT;
    doc["disp"] = "ssd1306-128x64";
    doc["ip"] = WiFi.localIP().toString();
    sendJson(doc);
}

static void sendKeyEvent(size_t id, const char *act) {
    JsonDocument doc;
    doc["t"] = "key";
    doc["id"] = static_cast<int>(id);
    doc["row"] = static_cast<int>(id / 4) + 1;
    doc["col"] = static_cast<int>(id % 4) + 1;
    doc["act"] = act;
    sendJson(doc);

    // 脱机自测时在屏上回显按键；有 hub 驱动屏幕时不要抢它的消息区
    if (!hubDrivingScreen()) {
        char buf[40];
        snprintf(buf, sizeof(buf), "R%uC%u %s", static_cast<unsigned>(id / 4 + 1),
                 static_cast<unsigned>(id % 4 + 1), act);
        display::hubMessage(buf, strcmp(act, "long") == 0 ? display::Style::Highlight
                                                          : display::Style::Normal);
    }
}

static void applyLed(const Led &led, bool lit) {
    digitalWrite(led.pin, lit != led.activeLow ? HIGH : LOW);
}

static void onWifiEvent(WiFiEvent_t event, WiFiEventInfo_t info) {
    if (event == ARDUINO_EVENT_WIFI_STA_DISCONNECTED) {
        JsonDocument doc;
        doc["t"] = "wifi";
        doc["status"] = "disconnected";
        doc["reason"] = info.wifi_sta_disconnected.reason;
        sendJson(doc);
    }
}

static void sendWifiStatus() {
    JsonDocument doc;
    doc["t"] = "wifi";
    if (WiFi.status() == WL_CONNECTED) {
        doc["status"] = "connected";
        doc["ssid"] = WiFi.SSID();
        doc["ip"] = WiFi.localIP().toString();
        doc["rssi"] = WiFi.RSSI();
    } else {
        doc["status"] = "disconnected";
    }
    sendJson(doc);
}

static void updateWifi() {
    uint32_t now = millis();
    if (now - lastWifiCheck < WIFI_CHECK_MS) return;
    lastWifiCheck = now;

    bool connected = WiFi.status() == WL_CONNECTED;
    if (!connected && (lastWifiAttempt == 0 || now - lastWifiAttempt >= WIFI_RETRY_MS)) {
        lastWifiAttempt = now;
        connected = wifiMulti.run(WIFI_PER_TRY_MS) == WL_CONNECTED;
    }
    if (connected != wifiWasConnected) {
        wifiWasConnected = connected;
        sendWifiStatus();
        if (connected) startNtp();
    }
}

// 屏幕相关指令。协议里历史上叫 "tft"，v4 换成 OLED 后同时接受 "disp"，
// 保持 hub 不改也能跑。
static void handleDisplayCmd(JsonDocument &doc) {
    const char *op = doc["op"] | "";
    display::Style style = display::styleFromName(doc["color"] | "white");
    // hub 来画东西说明有事发生，唤醒屏幕。审批请求最不能错过
    noteActivity();

    if (strcmp(op, "test") == 0) {
        display::testPattern();
    } else if (strcmp(op, "msg") == 0) {
        display::hubMessage(doc["text"] | "", style);
        gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
    } else if (strcmp(op, "msg_clear") == 0) {
        display::hubClear();
        display::leaveFullscreen();
        gHubBodyUntilMs = 0;
        gHomeDirty = true;
    } else if (strcmp(op, "status") == 0) {
        gFullscreenTransient = doc["transient"] | false;
        // 审批屏必须看得见：哪怕用户刚主动按 * 熄了屏也要点亮。
        // 否则会出现"请求在等、屏幕全黑"——那是这个产品最不能有的状态。
        // 查询屏（0/5/6）不强制点亮：那是用户主动查的，他不会在熄屏状态下按。
        if (!gFullscreenTransient) {
            lightOn();
        }
        // skip 是滚动位置。返回总行数给 hub，它据此判断还能不能往下滚
        int total = display::statusScreen(doc["mode"] | "", doc["text"] | "", style,
                                         doc["skip"] | 0);
        JsonDocument out;
        out["t"] = "disp";
        out["op"] = "status";
        out["lines"] = total;
        sendJson(out);
    } else if (strcmp(op, "hints") == 0) {
        display::keyHints(doc["h"][0] | "", doc["h"][1] | "", doc["h"][2] | "",
                          doc["h"][3] | "");
    } else if (strcmp(op, "text") == 0) {
        display::line(doc["line"] | 0, doc["text"] | "", style);
        gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
    } else if (strcmp(op, "badge") == 0) {
        strncpy(gBadge, doc["text"] | "", sizeof(gBadge) - 1);
        gBadge[sizeof(gBadge) - 1] = '\0';
        gHomeDirty = true;  // 立刻重绘标题栏，角标必须马上可见
    } else if (strcmp(op, "clock") == 0 || strcmp(op, "home") == 0) {
        // "clock" 是旧名字，保留：hub 和固件是分开部署的，
        // 旧 hub 配新固件必须还能把屏幕收回首屏，否则会一直停在审批界面
        display::leaveFullscreen();
        gHubBodyUntilMs = 0;  // hub 明确把屏交回来了，立刻画首屏
        gHomeDirty = true;
    } else if (strcmp(op, "tasks") == 0) {
        gTaskCount = 0;
        JsonArray items = doc["items"].as<JsonArray>();
        for (JsonVariant v : items) {
            if (gTaskCount >= display::MAX_TASKS) break;
            snprintf(gTasks[gTaskCount], sizeof(gTasks[0]), "%s", v.as<const char *>());
            gTaskCount++;
        }
        gTaskTotal = doc["total"] | gTaskCount;
        gHomeDirty = true;  // 正在看任务屏时也会立刻反映出来
    } else if (strcmp(op, "hub_info") == 0) {
        snprintf(gHubVersion, sizeof(gHubVersion), "hub %s", doc["version"] | "?");
        gHomeDirty = true;
    } else if (strcmp(op, "backlight") == 0) {
        bool on = doc["on"] | true;
        if (on) {
            // 走同一条唤醒路径：也要丢掉残留的查询屏
            wakeScreen();
        } else {
            display::backlight(false);
        }
    }
    JsonDocument out;
    out["t"] = "ok";
    out["cmd"] = "disp";
    sendJson(out);
}

static void handleLine(const char *line) {
    JsonDocument doc;
    DeserializationError err = deserializeJson(doc, line);
    if (err) {
        JsonDocument out;
        out["t"] = "err";
        out["msg"] = err.c_str();
        sendJson(out);
        return;
    }

    const char *type = doc["t"];
    if (type == nullptr) return;
    gLastHubCmdMs = millis();  // 收到任何一条 hub 指令都算 hub 在线

    if (strcmp(type, "ping") == 0) {
        JsonDocument out;
        out["t"] = "pong";
        out["uptime_ms"] = millis();
        sendJson(out);
    } else if (strcmp(type, "wifi") == 0) {
        sendWifiStatus();
    } else if (strcmp(type, "scan") == 0) {
        int n = WiFi.scanNetworks();  // 阻塞约 2 秒
        JsonDocument out;
        out["t"] = "scan";
        JsonArray arr = out["aps"].to<JsonArray>();
        for (int i = 0; i < n && i < 15; i++) {
            JsonObject ap = arr.add<JsonObject>();
            ap["ssid"] = WiFi.SSID(i);
            ap["rssi"] = WiFi.RSSI(i);
            ap["ch"] = WiFi.channel(i);
            ap["auth"] = static_cast<int>(WiFi.encryptionType(i));
        }
        WiFi.scanDelete();
        sendJson(out);
    } else if (strcmp(type, "tft") == 0 || strcmp(type, "disp") == 0) {
        handleDisplayCmd(doc);
    } else if (strcmp(type, "keys") == 0) {
        // 诊断：整个矩阵的原始扫描结果 + 空闲电平自检
        JsonDocument out;
        out["t"] = "keys";
        JsonArray rows = out["matrix"].to<JsonArray>();
        for (size_t r = 0; r < 4; r++) {
            bool raw[4];
            readRow(r, raw);
            JsonArray row = rows.add<JsonArray>();
            for (size_t c = 0; c < 4; c++) row.add(raw[c] ? 1 : 0);
        }
        rowsIdle();
        // 空闲时列脚应全为低；为高说明列线接到了电源轨或行线
        JsonArray idle = out["idle_cols"].to<JsonArray>();
        for (uint8_t pin : COL_PINS) idle.add(digitalRead(pin) == HIGH ? 1 : 0);
        sendJson(out);
    } else if (strcmp(type, "led") == 0) {
        int ledId = doc["id"] | -1;
        const char *mode = doc["mode"] | "off";
        if (ledId < 0 || ledId >= static_cast<int>(LED_COUNT)) return;
        Led &led = leds[ledId];
        if (strcmp(mode, "on") == 0) {
            led.mode = 1;
        } else if (strcmp(mode, "blink") == 0) {
            led.mode = 2;
            float hz = doc["hz"] | 2.0f;
            if (hz > 0.1f && hz <= 50.0f) led.halfMs = static_cast<uint32_t>(500.0f / hz);
        } else {
            led.mode = 0;
        }
        JsonDocument out;
        out["t"] = "ok";
        out["cmd"] = "led";
        sendJson(out);
    } else {
        JsonDocument out;
        out["t"] = "err";
        out["msg"] = "unknown cmd";
        sendJson(out);
    }
}

static void scanKeys() {
    uint32_t now = millis();
    for (size_t r = 0; r < 4; r++) {
        bool raw[4];
        readRow(r, raw);
        for (size_t c = 0; c < 4; c++) {
            size_t id = r * 4 + c;
            KeyState &ks = keyStates[id];
            if (raw[c] != ks.lastRaw) {
                ks.lastRaw = raw[c];
                ks.lastEdge = now;
            }
            if (now - ks.lastEdge >= DEBOUNCE_MS && raw[c] != ks.stable) {
                // 冷却期内不接受新的状态切换，滤掉机械弹跳产生的连串事件
                if (now - ks.lastEvent < EVENT_COOLDOWN_MS) continue;
                ks.stable = raw[c];
                ks.lastEvent = now;
                if (raw[c]) {
                    ks.pressAt = now;
                    ks.longSent = false;
                    // 息屏时第一次按键只唤醒，【不】把事件发给 hub。
                    // 否则摸黑按一下可能正好批准了一条在排队的请求——
                    // 唤醒是个无害动作，不该有副作用。
                    if (!display::backlightOn()) {
                        ks.swallow = true;
                        wakeScreen();
                        continue;
                    }
                    ks.swallow = false;
                    noteActivity();
                    // 首屏状态下 A/B 是翻页，由固件独占：翻完【不】把事件发给 hub。
                    //
                    // 一开始是"翻页 + 照常上报"，实测出问题：hub 空闲时收到 A/B 会回一句
                    // no request，那条消息正好盖住刚翻出来的页面。修 hub 当然能治，
                    // 但首屏翻页是固件自己的功能，正确性不该取决于 hub 版本——
                    // hub 是分开部署的，而且离线时更需要能翻到帮助页。
                    // 所以在这里就吞掉，配旧 hub 也对。
                    // * = 退一层 / 熄屏。全在固件本地判断，见 starKey 的注释
                    if (id == 12) {
                        starKey();
                        ks.swallow = true;
                        continue;
                    }
                    // 4 = 任务屏。数据已在设备上，本地画、按下即出，不发给 hub。
                    // 再按一次收起——同一个键开合，比"按 4 开、按别的关"好记
                    if (!display::fullscreenActive() && id == 4) {
                        gTasksViewUntilMs =
                            (gTasksViewUntilMs != 0) ? 0 : millis() + TASKS_VIEW_MS;
                        gHomeDirty = true;
                        ks.swallow = true;
                        continue;
                    }
                    // A/B 翻页时若正停在任务屏上，先把它收起，否则翻了看不见
                    if (!display::fullscreenActive() && (id == 3 || id == 7)) {
                        gTasksViewUntilMs = 0;
                        gHomePage = (id == 3)
                                        ? (gHomePage + display::HOME_PAGES - 1) %
                                              display::HOME_PAGES
                                        : (gHomePage + 1) % display::HOME_PAGES;
                        gHomeDirty = true;
                        ks.swallow = true;  // 连同 long/release 一起吞，别让 hub 收到半截事件
                        continue;
                    }
                    sendKeyEvent(id, "press");
                } else {
                    // 被吞掉的那次按下，对应的松开也要吞：
                    // 只发 release 会让 hub 看到一个没有 press 的松开事件
                    if (ks.swallow) {
                        ks.swallow = false;
                        continue;
                    }
                    sendKeyEvent(id, "release");
                }
            }
            if (ks.stable && !ks.swallow && !ks.longSent &&
                now - ks.pressAt >= LONG_PRESS_MS) {
                ks.longSent = true;
                sendKeyEvent(id, "long");
            }
        }
    }
    rowsIdle();
}

static void updateLeds() {
    uint32_t now = millis();
    for (size_t i = 0; i < LED_COUNT; i++) {
        Led &led = leds[i];
        bool lit = false;
        if (led.mode == 1) lit = true;
        else if (led.mode == 2) lit = (now / led.halfMs) % 2 == 0;
        applyLed(led, lit);
    }
}

void setup() {
    // LED 先初始化：GPIO2 有板载上拉，不先拉低红灯会一直亮着
    for (size_t i = 0; i < LED_COUNT; i++) {
        pinMode(leds[i].pin, OUTPUT);
        applyLed(leds[i], false);
    }

    for (uint8_t pin : COL_PINS) pinMode(pin, INPUT_PULLDOWN);
    rowsIdle();
    // 用上电时的真实电平初始化状态，否则首次扫描会误报一个事件
    for (size_t r = 0; r < 4; r++) {
        bool raw[4];
        readRow(r, raw);
        for (size_t c = 0; c < 4; c++) {
            keyStates[r * 4 + c].stable = raw[c];
            keyStates[r * 4 + c].lastRaw = raw[c];
        }
    }
    rowsIdle();

    hublink::begin(handleLine);
    delay(300);

    bool dispOk = display::begin();
    display::topBar("kiboard", "boot", display::Style::Normal);
    gLastActivityMs = millis();
    if (!dispOk) {
        JsonDocument out;
        out["t"] = "err";
        out["msg"] = "oled init failed";
        sendJson(out);
    }

    WiFi.mode(WIFI_STA);
    // C3 SuperMini 天线缺陷：满功率发射失真导致 auth 握手失败，必须降功率
    WiFi.setTxPower(WIFI_POWER_8_5dBm);
    WiFi.onEvent(onWifiEvent);
    for (const WifiCred &cred : WIFI_CREDS) wifiMulti.addAP(cred.ssid, cred.pass);

    sendHello();
}

void loop() {
    // 链路（重）建立后重新自报，否则 hub 只见 pong、status 里 firmware/keys 为空
    if (hublink::takeJustConnected()) {
        sendHello();
        sendWifiStatus();
    }
    scanKeys();
    updateLeds();
    updateWifi();
    updateHome();
    display::loop();
    hublink::loop();
}
