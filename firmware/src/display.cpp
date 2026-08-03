#include "display.h"

#include <Adafruit_GFX.h>
#include <Adafruit_SSD1306.h>
#include <Wire.h>

#include "cnfont.h"

namespace {
constexpr int PIN_SDA = 4;
constexpr int PIN_SCL = 5;
constexpr uint8_t I2C_ADDR = 0x3C;  // 实测确认，总线上唯一设备
constexpr uint32_t I2C_HZ = 400000;

constexpr int16_t W = 128;
constexpr int16_t H = 64;

// 内置字体 size1 是 6x8；cnfont 是 6x12(ASCII) / 12x12(汉字)
constexpr int16_t BUILTIN_W = 6;
constexpr int16_t BUILTIN_H = 8;

// 屏幕分区（y 坐标）。竖向 64px 很紧，这套划分是按 12px 中文行高倒推出来的：
//
//   0..7    顶栏      内置 6x8（hub 只往这里发 ASCII，省 4px 给下面）
//   10..25  时钟      内置 size2 12x16
//   28..35  日期      内置 6x8
//   38      分隔线
//   40..51  消息行 1  cnfont 12px
//   52..63  消息行 2  cnfont 12px
constexpr int16_t Y_TOPBAR = 0;
constexpr int16_t Y_CLOCK = 10;
constexpr int16_t Y_DATE = 28;
constexpr int16_t Y_RULE = 38;
constexpr int16_t Y_MSG = 40;
constexpr int16_t MSG_STEP = 12;
constexpr int MSG_LINES = 2;

// 全屏审批界面：标题条 + 4 行正文。
// 原本第 4 行是按键提示格，但 32px 一格放不下 "H1.2s"，截成 "H1.~" 没人看得懂，
// 于是把那一行还给正文——内容比图例值钱。
constexpr int16_t Y_STATUS_BODY = 13;
constexpr int16_t STATUS_STEP = 13;
constexpr int STATUS_LINES = 4;
constexpr int16_t Y_HINTS = 52;
// 右边留出画滚动箭头的地方
constexpr int16_t SCROLL_W = 7;

// 首屏：标题栏 11px，正文 13..63 共 4 行 12px（和审批界面同一套行距，视觉一致）
constexpr int16_t Y_HOME_BODY = 14;
constexpr int16_t HOME_STEP = 12;
constexpr int HOME_LINES = 4;

Adafruit_SSD1306 oled(W, H, &Wire, -1);
bool ready = false;

bool dirty = false;
uint32_t lastFlush = 0;
constexpr uint32_t FLUSH_MIN_MS = 30;

// statusScreen 占用全屏时置位，首屏停止重绘
bool fullscreen = false;
// 息屏状态。主循环据此跳过重绘——息屏时还刷屏是白耗 I2C 带宽
bool lit = true;

void markDirty() { dirty = true; }

// 顶栏、时钟、日期用内置字体（只有 ASCII，但省地方）
void drawBuiltin(int16_t x, int16_t y, const char *text, uint8_t size, bool invert) {
    oled.setTextSize(size);
    oled.setTextColor(invert ? SSD1306_BLACK : SSD1306_WHITE);
    oled.setCursor(x, y);
    oled.print(text);
    oled.setTextColor(SSD1306_WHITE);
}
}  // namespace

namespace display {

Style styleFromName(const char *name) {
    if (name == nullptr) {
        return Style::Normal;
    }
    // 单色屏只有两档：需要抢注意力的用反色，其余正常
    if (strcmp(name, "red") == 0 || strcmp(name, "yellow") == 0) {
        return Style::Highlight;
    }
    return Style::Normal;
}

bool begin() {
    Wire.begin(PIN_SDA, PIN_SCL, I2C_HZ);
    ready = oled.begin(SSD1306_SWITCHCAPVCC, I2C_ADDR);
    if (!ready) {
        return false;
    }
    oled.clearDisplay();
    oled.setTextWrap(false);
    oled.display();
    return true;
}

void loop() {
    if (!ready || !dirty) {
        return;
    }
    uint32_t now = millis();
    if (now - lastFlush < FLUSH_MIN_MS) {
        return;
    }
    lastFlush = now;
    dirty = false;
    oled.display();
}

bool backlightOn() { return lit; }

void backlight(bool on) {
    lit = on;
    if (!ready) {
        return;
    }
    // OLED 可以整屏断电，这是相对 ST7735 的实质改进（那块屏 BLK 硬接 3V3 关不掉）
    oled.ssd1306_command(on ? SSD1306_DISPLAYON : SSD1306_DISPLAYOFF);
}

void testPattern() {
    if (!ready) {
        return;
    }
    fullscreen = true;
    oled.clearDisplay();
    oled.drawRect(0, 0, W, H, SSD1306_WHITE);
    oled.drawLine(0, 0, W - 1, H - 1, SSD1306_WHITE);
    oled.drawLine(W - 1, 0, 0, H - 1, SSD1306_WHITE);
    constexpr int16_t s = 6;
    oled.fillRect(1, 1, s, s, SSD1306_WHITE);
    oled.fillRect(W - s - 1, 1, s, s, SSD1306_WHITE);
    oled.fillRect(1, H - s - 1, s, s, SSD1306_WHITE);
    oled.fillRect(W - s - 1, H - s - 1, s, s, SSD1306_WHITE);
    // 顺带验证中文字库：这几个字画不出来就是字库没嵌进去
    cnfont::drawString(oled, 22, 26, "中文字库正常", SSD1306_WHITE);
    drawBuiltin(30, 42, "128x64 SSD1306", 1, false);
    markDirty();
}

bool fullscreenActive() { return fullscreen; }

void leaveFullscreen() {
    if (!ready) {
        return;
    }
    fullscreen = false;
    oled.clearDisplay();
    markDirty();
}

int statusScreen(const char *mode, const char *text, Style style, int skip) {
    if (!ready) {
        return 0;
    }
    fullscreen = true;
    oled.clearDisplay();

    // 顶部模式条：始终反色，作为标题。hub 往这里发的是 ASCII（APPROVE? / !! APPROVE）
    oled.fillRect(0, 0, W, 11, SSD1306_WHITE);
    drawBuiltin(2, 2, mode, 1, true);

    // 正文可能含中文，走点阵字库
    (void)style;  // 正文用反色会糊成一片，样式只体现在标题条上
    int total = cnfont::drawWrapped(oled, 1, Y_STATUS_BODY, W - 2 - SCROLL_W, STATUS_LINES,
                                    STATUS_STEP, text, SSD1306_WHITE, false, 0, skip);

    // 有内容被藏起来时画箭头。截断而不告知是危险的——看不见的那段可能正是要命的。
    int16_t ax = W - SCROLL_W + 1;
    if (skip > 0) {
        oled.fillTriangle(ax, Y_STATUS_BODY + 5, ax + 4, Y_STATUS_BODY + 5,
                          ax + 2, Y_STATUS_BODY, SSD1306_WHITE);
    }
    if (skip + STATUS_LINES < total) {
        int16_t by = H - 2;
        oled.fillTriangle(ax, by - 5, ax + 4, by - 5, ax + 2, by, SSD1306_WHITE);
    }
    markDirty();
    return total;
}

void keyHints(const char *h0, const char *h1, const char *h2, const char *h3) {
    if (!ready) {
        return;
    }
    constexpr int16_t bw = W / 4;
    const char *hints[4] = {h0, h1, h2, h3};
    oled.fillRect(0, Y_HINTS, W, H - Y_HINTS, SSD1306_BLACK);
    for (int i = 0; i < 4; i++) {
        if (hints[i] == nullptr || hints[i][0] == '\0') {
            continue;
        }
        oled.drawRect(i * bw, Y_HINTS, bw - 1, 12, SSD1306_WHITE);
        // 格子内宽 bw-4，放不下就截断，别越到隔壁格
        cnfont::drawString(oled, i * bw + 2, Y_HINTS + 1,
                           cnfont::ellipsize(hints[i], bw - 4), SSD1306_WHITE);
    }
    markDirty();
}

void line(uint8_t row, const char *text, Style style) {
    if (!ready) {
        return;
    }
    int16_t y = Y_STATUS_BODY + row * STATUS_STEP;
    if (y + cnfont::LINE_H > H) {
        return;
    }
    oled.fillRect(0, y, W, cnfont::LINE_H, SSD1306_BLACK);
    if (style == Style::Highlight) {
        oled.fillRect(0, y, W, cnfont::LINE_H, SSD1306_WHITE);
        cnfont::drawString(oled, 1, y, text, SSD1306_BLACK);
    } else {
        cnfont::drawString(oled, 1, y, text, SSD1306_WHITE);
    }
    markDirty();
}

void clock(const char *hhmmss, const char *date, const char *weekday, bool synced) {
    if (!ready) {
        return;
    }
    // 大号时间居中：内置字体 size2 每字符 12px
    int16_t tw = static_cast<int16_t>(strlen(hhmmss)) * BUILTIN_W * 2;
    oled.fillRect(0, Y_CLOCK, W, 16, SSD1306_BLACK);
    drawBuiltin((W - tw) / 2, Y_CLOCK, hhmmss, 2, false);

    // 未对时的时候标出来，别让人以为时间是真的
    char buf[40];
    if (synced) {
        snprintf(buf, sizeof(buf), "%s %s", date, weekday);
    } else {
        snprintf(buf, sizeof(buf), "%s %s (no ntp)", date, weekday);
    }
    int16_t dw = static_cast<int16_t>(strlen(buf)) * BUILTIN_W;
    if (dw > W) {
        dw = W;
    }
    oled.fillRect(0, Y_DATE, W, BUILTIN_H, SSD1306_BLACK);
    drawBuiltin((W - dw) / 2, Y_DATE, buf, 1, false);

    oled.drawFastHLine(0, Y_RULE, W, SSD1306_WHITE);
    markDirty();
}

void topBar(const char *left, const char *right, Style style) {
    if (!ready) {
        return;
    }
    oled.fillRect(0, Y_TOPBAR, W, BUILTIN_H + 2, SSD1306_BLACK);
    if (style == Style::Highlight) {
        // 角标反色：AUTO 这类必须常驻可见的状态
        int16_t lw = static_cast<int16_t>(strlen(left)) * BUILTIN_W;
        oled.fillRect(0, Y_TOPBAR, lw + 4, BUILTIN_H + 2, SSD1306_WHITE);
        drawBuiltin(2, Y_TOPBAR + 1, left, 1, true);
    } else {
        drawBuiltin(2, Y_TOPBAR + 1, left, 1, false);
    }
    int16_t rw = static_cast<int16_t>(strlen(right)) * BUILTIN_W;
    drawBuiltin(W - rw - 2, Y_TOPBAR + 1, right, 1, false);
    markDirty();
}

void hubMessage(const char *text, Style style) {
    if (!ready) {
        return;
    }
    oled.fillRect(0, Y_MSG, W, H - Y_MSG, SSD1306_BLACK);
    if (style == Style::Highlight) {
        // 只把第一行刷成反色底，两行全反色会糊成一片
        oled.fillRect(0, Y_MSG, W, cnfont::LINE_H, SSD1306_WHITE);
        cnfont::drawWrapped(oled, 1, Y_MSG, W - 2, 1, MSG_STEP, text, SSD1306_BLACK);
        // 第一行装不下的部分照常白字画在第二行
        int firstLineChars = 0;
        int w = 0;
        const char *s = text;
        while (*s && w + cnfont::ASCII_W <= W - 2) {
            uint8_t c = static_cast<uint8_t>(*s);
            int adv = (c < 0x80) ? 1 : 3;  // UTF-8 汉字按 3 字节算
            int cw = (c < 0x80) ? cnfont::ASCII_W : cnfont::CJK_W;
            if (w + cw > W - 2) {
                break;
            }
            w += cw;
            s += adv;
            firstLineChars += adv;
        }
        if (*s) {
            cnfont::drawWrapped(oled, 1, Y_MSG + MSG_STEP, W - 2, 1, MSG_STEP, s,
                                SSD1306_WHITE);
        }
    } else {
        cnfont::drawWrapped(oled, 1, Y_MSG, W - 2, MSG_LINES, MSG_STEP, text,
                            SSD1306_WHITE);
    }
    markDirty();
}

void hubClear() {
    if (!ready) {
        return;
    }
    oled.fillRect(0, Y_MSG, W, H - Y_MSG, SSD1306_BLACK);
    markDirty();
}

// ---------- 首屏 ----------
namespace {

// 帮助文案写死在固件里，不由 hub 下发。
// 理由：hub 连不上的时候恰恰是最需要看"这键干什么"的时候。
// 键位本身也是固定的（印在键盘上），没有必须动态化的部分。
const char *const HELP_A[] = {
    "1 接受    2 拒绝",
    "3 全部接受(10分钟)",
    "D 关闭全部接受",
    "红框=高危 按住1",
};
const char *const HELP_B[] = {
    "4 任务  5 审批过的",
    "6 上次审批详情",
    "0 信息  C 全取消",
    "* 退一层/熄屏",
};

// 页码点画在右边缘、竖排。当前页实心。
//
// 一开始画在底部横排，和第 4 行正文（画到 y=60）撞了。改竖排放右边缘，
// 和审批界面的滚动箭头同一个位置、同一套视觉语言：右边缘表示"还有别的内容"。
// 用点而不是 "2/4"：不用读，扫一眼就知道在哪。
void pageDots(uint8_t page) {
    constexpr int16_t r = 2;
    constexpr int16_t gap = 9;
    int16_t x = W - r - 1;
    int16_t y0 = Y_HOME_BODY + 4;
    for (int i = 0; i < HOME_PAGES; i++) {
        int16_t y = y0 + i * gap;
        if (i == page) {
            oled.fillCircle(x, y, r, SSD1306_WHITE);
        } else {
            oled.drawCircle(x, y, r, SSD1306_WHITE);
        }
    }
}

void drawCenteredBuiltin(int16_t y, const char *text, uint8_t size) {
    int16_t w = static_cast<int16_t>(strlen(text)) * BUILTIN_W * size;
    drawBuiltin((W - w) / 2, y, text, size, false);
}

constexpr int16_t HOME_BODY_W = W - 2 - SCROLL_W;  // 右边缘留给页码点

void drawHelpPage(const char *const *lines, int n) {
    for (int i = 0; i < n && i < HOME_LINES; i++) {
        cnfont::drawString(oled, 1, Y_HOME_BODY + i * HOME_STEP,
                           cnfont::ellipsize(lines[i], HOME_BODY_W), SSD1306_WHITE);
    }
}

void drawTasksPage(const Home &info) {
    if (info.taskTotal <= 0) {
        cnfont::drawString(oled, 1, Y_HOME_BODY, "没有进行中的任务", SSD1306_WHITE);
        cnfont::drawString(oled, 1, Y_HOME_BODY + HOME_STEP * 2, "agent 上报后显示",
                           SSD1306_WHITE);
        return;
    }

    // 四行正文全给任务；条数超出时最后一行让给"还有 n 条"。
    // 截断而不告知会让人以为任务就这些——那比不显示更糟。
    int shown = info.taskCount < HOME_LINES ? info.taskCount : HOME_LINES;
    bool more = info.taskTotal > shown;
    if (more && shown == HOME_LINES) {
        shown = HOME_LINES - 1;
    }

    for (int i = 0; i < shown; i++) {
        cnfont::drawString(oled, 1, Y_HOME_BODY + HOME_STEP * i,
                           cnfont::ellipsize(info.tasks[i], HOME_BODY_W), SSD1306_WHITE);
    }
    if (info.taskTotal > shown) {
        char buf[24];
        snprintf(buf, sizeof(buf), "... 还有 %d 条", info.taskTotal - shown);
        cnfont::drawString(oled, 1, Y_HOME_BODY + HOME_STEP * shown, buf, SSD1306_WHITE);
    }
}

}  // namespace

namespace {
// 标题栏：左边链路/角标，右边时间（只到分钟——秒在待机屏上没有信息量，
// 反而让整屏每秒重绘一次，白耗 I2C 带宽）
void drawTitleBar(const Home &info, const char *overrideLeft) {
    const char *left = overrideLeft != nullptr ? overrideLeft : info.label;
    bool hl = overrideLeft == nullptr && info.labelHighlight;
    if (hl) {
        int16_t lw = static_cast<int16_t>(strlen(left)) * BUILTIN_W;
        oled.fillRect(0, Y_TOPBAR, lw + 4, BUILTIN_H + 2, SSD1306_WHITE);
        drawBuiltin(2, Y_TOPBAR + 1, left, 1, true);
    } else {
        drawBuiltin(2, Y_TOPBAR + 1, left, 1, false);
    }
    if (info.hhmm != nullptr && info.hhmm[0] != '\0') {
        int16_t rw = static_cast<int16_t>(strlen(info.hhmm)) * BUILTIN_W;
        drawBuiltin(W - rw - 2, Y_TOPBAR + 1, info.hhmm, 1, false);
    }
    oled.drawFastHLine(0, BUILTIN_H + 3, W, SSD1306_WHITE);
}
}  // namespace

void tasksScreen(const Home &info) {
    if (!ready) {
        return;
    }
    oled.clearDisplay();
    drawTitleBar(info, "TASKS");
    drawTasksPage(info);
    markDirty();
}

void homeScreen(uint8_t page, const Home &info) {
    if (!ready) {
        return;
    }
    page %= HOME_PAGES;
    // 首屏不是 fullscreen：审批一来直接盖掉，不需要先退出
    oled.clearDisplay();
    drawTitleBar(info, nullptr);

    switch (page) {
        case 0: {
            // logo 页。size3 每字符 18px，KIBOARD 七个字符 126px，正好占满 128
            drawCenteredBuiltin(16, "KIBOARD", 3);
            drawCenteredBuiltin(44, info.link, 1);
            if (info.hubVersion != nullptr && info.hubVersion[0] != '\0') {
                drawCenteredBuiltin(53, info.hubVersion, 1);
            }
            break;
        }
        case 1:
            drawHelpPage(HELP_A, 4);
            break;
        default:
            drawHelpPage(HELP_B, 4);
            break;
    }
    // logo 页不画页码点：那一屏就是块门面，右边挂三个圈会把 KIBOARD 挤得不居中。
    // 其余页都有点，翻过去一次就知道有几页了
    if (page != 0) {
        pageDots(page);
    }
    markDirty();
}

}  // namespace display
