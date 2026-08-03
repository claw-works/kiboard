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

// ---------- 待审请求 ----------
//
// hub 只给字段（{"t":"request","verbatim":...,"risk":...,"hold_ms":...}），
// **排版全在这里**：屏幕多宽、怎么折行、滚到第几行，只有设备知道。
// 以前是 hub 拼好字符串下发，那意味着 hub 里写着 21 字符和 4 行这种常数——
// 换一块屏或换个设备方案就得改 hub。
//
// 键位映射同理：hub 不知道"第 0 号键"，它只收到 accept / reject 这样的语义。
static bool gReqActive = false;
static uint32_t gReqId = 0;
static char gReqVerbatim[192];  // 逐字原文，required-to-display
static char gReqSummary[96];    // agent 自己写的意图，可信度最低，挤掉不影响判断
static char gReqLabel[24];
static char gReqClient[16];
static char gReqCwd[48];
static bool gReqHigh = false;
static uint32_t gReqHoldMs = 0;
static int gReqQueued = 0;
static int gReqScroll = 0;      // 滚动位置，本地持有
static int gReqTotalLines = 0;  // 折行后的总行数，夹住滚动范围

// 高危长按：本地做进度反馈（不等网络往返），原始时间戳随裁决报给 hub 复核。
// 阈值是 hub 给的（gReqHoldMs），所以改阈值不用重烧固件。
static uint32_t gHoldStart = 0;
static bool gHoldReady = false;

// 丝印标签。hub 不再持有键位表，标签随按键事件一起上报，只为日志和 WS 订阅者可读
static const char *KEY_LABELS[16] = {"1", "2", "3", "A", "4", "5", "6", "B",
                                     "7", "8", "9", "C", "*", "0", "#", "D"};

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
    // 能力声明。render=self 表示"排版我自己来，别给我拼好的字符串"。
    // 如实声明是硬要求：声明做不到的能力，后果是 hub 把它不能安全处理的请求推过来
    JsonObject caps = doc["caps"].to<JsonObject>();
    caps["render"] = "self";
    caps["input"] = "matrix16";
    JsonArray confirm = caps["confirm"].to<JsonArray>();
    confirm.add("tap");
    confirm.add("hold");
    // 按住时长由 hub 依据原始 press/release 事件复核，不是设备自己说了算
    caps["confirm_verifiable"] = true;
    sendJson(doc);
}

static void sendKeyEvent(size_t id, const char *act) {
    JsonDocument doc;
    doc["t"] = "key";
    doc["id"] = static_cast<int>(id);
    doc["row"] = static_cast<int>(id / 4) + 1;
    doc["col"] = static_cast<int>(id % 4) + 1;
    doc["label"] = KEY_LABELS[id];
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

// ---------- 审批：语义裁决 ----------
//
// 设备发的是**人的意思**，不是按键。hub 因此不需要知道这块板子有几个键、
// 哪个键在哪——换成触摸屏或手机 App，发的还是这几个值。
static void sendDecision(const char *verdict, bool withHold = false) {
    JsonDocument doc;
    doc["t"] = "decision";
    if (gReqActive) doc["id"] = gReqId;  // 绑定请求：隔夜的 accept 不能落到新请求上
    doc["verdict"] = verdict;
    if (withHold && gHoldStart != 0) {
        // 报原始事件而不是"我确认过了"这个结论：阈值和判定都留在 hub，
        // 这样改阈值只要改配置，而且不会各设备实现各判一套
        JsonObject c = doc["confirm"].to<JsonObject>();
        c["method"] = "hold";
        JsonArray evs = c["events"].to<JsonArray>();
        JsonObject p = evs.add<JsonObject>();
        p["ev"] = "press";
        p["device_ts"] = gHoldStart;
        JsonObject r = evs.add<JsonObject>();
        r["ev"] = "release";
        r["device_ts"] = millis();
    }
    sendJson(doc);
}

// 要一屏只有 hub 知道的数据（链路状态、审批历史）。
// 设备不知道内容，但它知道人想看什么
static void sendQuery(const char *what) {
    JsonDocument doc;
    doc["t"] = "query";
    doc["what"] = what;
    sendJson(doc);
}

// 画审批界面。**verbatim 必须显示**：它是真正会执行的东西；
// summary 是 agent 自己写的，措辞良善内容危险的 summary 会让人在错误前提下批准，
// 所以它只能排在后面，绝不能单独出现。
static void drawRequest() {
    if (!gReqActive) return;

    // 标题条：真正会变的信息只有"短按还是按住"。队列深度和来源跟在后面，
    // 放不下就被截断——那两个是参考信息，缺了不影响判断
    char head[28];
    if (gReqHigh) {
        snprintf(head, sizeof(head), "!! HOLD1 %.1fs", gReqHoldMs / 1000.0);
    } else {
        snprintf(head, sizeof(head), "APPROVE?");
    }
    if (gReqQueued > 0) {
        size_t n = strlen(head);
        snprintf(head + n, sizeof(head) - n, " +%d", gReqQueued);
    }
    if (gReqClient[0] != '\0') {
        size_t n = strlen(head);
        snprintf(head + n, sizeof(head) - n, " %s", gReqClient);
    }

    // 正文空间按重要性分配：
    //   命令 —— 必须完整看到（放不下就靠 A/B 滚动，不能静默截断）
    //   目录 —— 同一条命令在不同目录后果完全不同，高危时是判断的必要信息
    //   说明 —— 模型自己写的，最不可信，排最后
    char body[320];
    if (gReqHigh) {
        // 高危给目录单独一行；来源已经在标题条里，正文不再重复
        snprintf(body, sizeof(body), "%s%s%s%s%s", gReqVerbatim,
                 gReqCwd[0] ? "\n@" : "", gReqCwd[0] ? gReqCwd : "",
                 gReqSummary[0] ? "\n" : "", gReqSummary[0] ? gReqSummary : "");
    } else {
        snprintf(body, sizeof(body), "%s%s%s%s%s%s", gReqLabel[0] ? "[" : "",
                 gReqLabel[0] ? gReqLabel : "", gReqLabel[0] ? "] " : "", gReqVerbatim,
                 gReqSummary[0] ? " " : "", gReqSummary[0] ? gReqSummary : "");
    }
    gReqTotalLines = display::statusScreen(head, body, display::Style::Highlight, gReqScroll);
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

    if (strcmp(type, "request") == 0) {
        // 待审请求：只有字段，排版在本地
        gReqActive = true;
        gReqId = doc["id"] | 0;
        snprintf(gReqVerbatim, sizeof(gReqVerbatim), "%s", doc["verbatim"] | "");
        snprintf(gReqSummary, sizeof(gReqSummary), "%s", doc["summary"] | "");
        snprintf(gReqLabel, sizeof(gReqLabel), "%s", doc["label"] | "");
        snprintf(gReqClient, sizeof(gReqClient), "%s", doc["client"] | "");
        snprintf(gReqCwd, sizeof(gReqCwd), "%s", doc["cwd"] | "");
        gReqHigh = strcmp(doc["risk"] | "normal", "high") == 0;
        gReqHoldMs = doc["hold_ms"] | 0;
        gReqQueued = doc["queued"] | 0;
        gReqScroll = 0;
        gHoldStart = 0;
        gHoldReady = false;
        gFullscreenTransient = false;  // 审批屏不可退：* 不能把一条等着裁决的请求顶掉
        // 审批请求必须看得见：哪怕用户刚主动熄了屏也要点亮。
        // "请求在等、屏幕全黑"是这个产品最不能有的状态
        lightOn();
        noteActivity();
        drawRequest();
        JsonDocument out;
        out["t"] = "disp";
        out["op"] = "request";
        out["lines"] = gReqTotalLines;
        sendJson(out);
        return;
    }
    if (strcmp(type, "request_done") == 0) {
        // 请求有了结果。**文案在设备侧**——怎么写、要不要反色是这块屏的事
        gReqActive = false;
        gHoldStart = 0;
        gHoldReady = false;
        const char *v = doc["verdict"] | "";
        const char *text = "DONE";
        if (strcmp(v, "accept") == 0) text = "ACCEPTED";
        else if (strcmp(v, "reject") == 0) text = "REJECTED";
        else if (strcmp(v, "auto_accept") == 0) text = "AUTO ACCEPTED";
        else if (strcmp(v, "rule_allow") == 0) text = "ALLOWED BY RULE";
        else if (strcmp(v, "timeout") == 0) text = "TIMED OUT";
        else if (strcmp(v, "cancelled") == 0) text = "CANCELLED";
        display::leaveFullscreen();
        gHomeDirty = true;
        display::hubMessage(text, display::Style::Normal);
        gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
        noteActivity();
        return;
    }

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

// 一次按下的本地处理。返回 true 表示这个键已经处理完、不再往 hub 发按键事件。
//
// 键位表在这里，不在 hub。分工原则是**数据在谁手里**：
//   本地数据（页码、滚动位置、任务列表、有没有待批请求）→ 固件自己判，按下即出
//   只有 hub 知道的（审批历史、链路状态、自动接受窗口）→ 发语义消息问 hub
static bool handleKeyLocally(size_t id) {
    // ---------- 审批界面上的键 ----------
    if (gReqActive) {
        switch (id) {
            case 0:  // 1 = 接受。高危走长按，由调用方处理（要记 press 时刻）
                if (!gReqHigh) {
                    sendDecision("accept");
                    return true;
                }
                return false;
            case 1:  // 2 = 拒绝。安全方向，不设门槛
                sendDecision("reject");
                return true;
            case 2:  // 3 = 全部接受
                sendDecision("accept_window");
                return true;
            case 3:    // A = 正文上滚
            case 7: {  // B = 正文下滚
                // 滚动完全本地：滚到第几行是这块屏的状态，hub 不需要知道。
                // 往下滚到底就不动，避免滚出一屏空白让人以为内容没了
                int maxSkip = gReqTotalLines - 4;
                if (maxSkip < 0) maxSkip = 0;
                int want = (id == 3) ? gReqScroll - 1 : gReqScroll + 1;
                if (want < 0) want = 0;
                if (want > maxSkip) want = maxSkip;
                if (want != gReqScroll) {
                    gReqScroll = want;
                    drawRequest();
                }
                return true;
            }
            case 11:  // C = 取消全部
                sendDecision("cancel_all");
                return true;
            case 15:  // D = 关掉「全部接受」
                sendDecision("clear_auto");
                return true;
            default:
                break;
        }
    }

    // ---------- 待机时的键 ----------
    // * = 退一层 / 熄屏。全在本地判断，见 starKey 的注释
    if (id == 12) {
        starKey();
        return true;
    }
    // 4 = 任务屏。数据已在设备上，本地画、按下即出，不等网络往返。
    // 再按一次收起——同一个键开合，比"按 4 开、按别的关"好记
    if (!display::fullscreenActive() && id == 4) {
        gTasksViewUntilMs = (gTasksViewUntilMs != 0) ? 0 : millis() + TASKS_VIEW_MS;
        gHomeDirty = true;
        return true;
    }
    // A/B = 首屏翻页。刻意不发给 hub：翻页是设备自己的功能，
    // 正确性不该取决于 hub 版本，而且 hub 离线时更需要能翻到帮助页
    if (!display::fullscreenActive() && (id == 3 || id == 7)) {
        gTasksViewUntilMs = 0;  // 停在任务屏上时先收起，否则翻了看不见
        gHomePage = (id == 3) ? (gHomePage + display::HOME_PAGES - 1) % display::HOME_PAGES
                              : (gHomePage + 1) % display::HOME_PAGES;
        gHomeDirty = true;
        return true;
    }
    // 只有 hub 知道的三屏：审批历史、上次详情、链路状态
    if (id == 5) {
        sendQuery("recent");
        return true;
    }
    if (id == 6) {
        sendQuery("last");
        return true;
    }
    if (id == 13) {
        sendQuery("info");
        return true;
    }
    // 队列控制与请求无关，空闲时也要能用（自动接受窗口是 hub 的状态）
    if (id == 11) {
        sendDecision("cancel_all");
        return true;
    }
    if (id == 15) {
        sendDecision("clear_auto");
        return true;
    }
    // 没有待批请求时按裁决键：本地就知道答案，不必问 hub
    if (id == 0 || id == 1 || id == 2) {
        display::hubMessage("no request", display::Style::Normal);
        gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
        return true;
    }
    return false;  // 7/8/9/# 没绑动作，照常上报 key 事件供诊断
}

// 高危长按到点了：灯转常亮 + 提示松手即生效。
//
// 这段反馈以前要等 hub 往返（固件发 long、hub 算时间、再下发提示），
// 现在本地做——阈值是 hub 在 request 里给的，所以既即时又不用重烧固件改阈值。
static void updateHold() {
    if (gHoldStart == 0 || gHoldReady || gReqHoldMs == 0) return;
    if (millis() - gHoldStart < gReqHoldMs) return;
    gHoldReady = true;
    leds[0].mode = 1;  // 常亮
    display::hubMessage("release to accept", display::Style::Highlight);
    gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
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
                    // ---------- 键位映射：全在设备侧 ----------
                    //
                    // hub 收到的是 accept / reject 这样的语义，不是键号。这么分的理由：
                    // 键位是"这块板子长什么样"决定的，换成触摸屏或手机根本没有键号；
                    // 而且能本地判断的就别绕一圈网络（滚动、翻页、没有请求时的提示）。
                    if (handleKeyLocally(id)) {
                        ks.swallow = true;  // 连同 long/release 一起吞，别让 hub 收到半截事件
                        continue;
                    }
                    // 高危长按的 press 不立刻上报：release 时才带着真实时长一起发。
                    // 计时起点记在这里
                    if (gReqActive && gReqHigh && id == 0) {
                        gHoldStart = now;
                        gHoldReady = false;
                        char buf[32];
                        snprintf(buf, sizeof(buf), "hold %.1fs to accept", gReqHoldMs / 1000.0);
                        display::hubMessage(buf, display::Style::Highlight);
                        gHubBodyUntilMs = millis() + HUB_BODY_HOLD_MS;
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
                    // 高危长按松手：带上原始 press/release 时间戳，由 hub 复核够不够
                    if (gReqActive && gReqHigh && id == 0 && gHoldStart != 0) {
                        sendDecision("accept", true);
                        gHoldStart = 0;
                        gHoldReady = false;
                        continue;
                    }
                    sendKeyEvent(id, "release");
                }
            }
            if (ks.stable && !ks.swallow && !ks.longSent &&
                now - ks.pressAt >= LONG_PRESS_MS) {
                ks.longSent = true;
                // long 不再发给 hub：高危门槛靠 release 时的真实时长判定，
                // 而 600ms 的 long 对人手区分不开"点一下"和"按住"（实测踩过）。
                // 这里只留给本地回显用
                if (!gReqActive) sendKeyEvent(id, "long");
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
    updateHold();
    updateLeds();
    updateWifi();
    updateHome();
    display::loop();
    hublink::loop();
}
