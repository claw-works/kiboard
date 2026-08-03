#pragma once
#include <Arduino.h>

// 0.96" SSD1306 OLED，128x64 单色，I2C 地址 0x3C（SDA=GPIO4 / SCL=GPIO5）
//
// 相比 v1 的 ST7735 彩屏，这块屏没有任何需要试参数的地方：
// 无 initR 变体、无行列偏移、无色序/反色陷阱，begin() 成功就能用。
//
// 单色屏没有颜色，协议里的 color 字段映射为 Style：
//   Normal    正常white-on-black
//   Highlight 反色底（用于 yellow/red 这类需要抢注意力的消息）
//
// SSD1306 是帧缓冲屏，画完必须 flush 才会显示。各绘制函数只写缓冲并置脏，
// 由 display::loop() 合并刷新——避免同一 tick 内多次绘制触发多次整屏传输
// （1024 字节 @400kHz 约 25ms）。
namespace display {

enum class Style : uint8_t {
    Normal = 0,
    Highlight = 1,
};

// 把协议里的颜色名映射成单色屏的样式
Style styleFromName(const char *name);

bool begin();
void loop();  // 合并刷新，必须在主循环里调用

// 全屏视图（statusScreen）与时钟视图会争夺整块屏幕：
// statusScreen 会清屏并占用全屏，此时主循环必须停止重绘时钟，
// 否则时钟每秒糊在审批界面上。用这两个函数做仲裁。
bool fullscreenActive();
void leaveFullscreen();  // 回到时钟视图

void backlight(bool on);  // OLED 可以真正息屏（DISPLAYOFF），不像 ST7735 的 BLK 硬接 3V3
bool backlightOn();
void testPattern();

// ---------- 首屏 ----------
//
// 原来空闲时是一屏大时钟。时钟好看但没信息量——设备大部分时间在待机，
// 那块屏应该告诉人"我是什么、怎么用、agent 在忙什么"，时间缩到标题栏就够了。
//
// A/B 轮播里【只放固件自己的东西】：logo 和帮助。
// 任务、审批记录这些有数据的屏各自绑一个数字键（4/5/6），不掺进轮播——
// 混在一起翻会让人不知道下一页是什么，而"看任务"是个明确意图，该一键直达。
// 页码由固件持有，因为翻页必须在 hub 离线时也能用。
constexpr int HOME_PAGES = 3;  // 0=logo 1/2=帮助
constexpr int MAX_TASKS = 6;

struct Home {
    const char *label;               // 标题栏左侧：链路/角标，ASCII
    const char *hhmm;                // 标题栏右侧：时间，只到分钟
    bool labelHighlight;             // 左侧反色（AUTO 这类必须扫一眼看到的状态）
    const char *link;                // logo 页的链路详情，如 "wifi -41dBm"
    const char *hubVersion;          // hub 版本，空则不显示
    const char *tasks[MAX_TASKS];    // 进行中任务标题
    int taskCount;                   // 本次带来的条数（<= MAX_TASKS）
    int taskTotal;                   // 实际总条数，可能多于 taskCount
};

// 画整屏首屏（含标题栏）。page 超范围时取模。
//
// 刻意把标题栏也画在这里：之前 topBar 由主循环单独调用，
// 而首屏要 clearDisplay，两者顺序错了标题栏就被抹掉。合成一个函数就没有顺序问题。
void homeScreen(uint8_t page, const Home &info);

// 任务屏：按 4 直接出。数据是 hub 推过来存在固件里的，
// 所以不必等网络往返，hub 抖一下也照样能看。
void tasksScreen(const Home &info);

// 全屏审批界面。skip 是滚动位置（跳过前几行）。
// 返回折行后的总行数，hub 据此判断能不能再往下滚。
int statusScreen(const char *mode, const char *text, Style style, int skip = 0);

// 底部四格按键提示。
//
// 现在 hub 不再下发它：屏幕一格只有 32px，减去边框只放得下 4 个字符，
// "H1.2s" 会截成 "H1.~"，一个占了整整一行却看不懂的东西比不显示更糟。
// 键位是固定的（1 接受 / 2 拒绝 / 3 全部 / D 关自动），印在键盘丝印上，
// 用一天就成肌肉记忆；真正会变的只有"短按还是按住"，那个写进标题条。
// 接口保留，将来若换更宽的屏可以再用。
void keyHints(const char *h0, const char *h1, const char *h2, const char *h3);
void line(uint8_t row, const char *text, Style style);
void clock(const char *hhmmss, const char *date, const char *weekday, bool synced);
void topBar(const char *left, const char *right, Style style);

// hub 下发的消息区（屏幕底部两行，不影响时钟区域）
void hubMessage(const char *text, Style style);
void hubClear();

}  // namespace display
