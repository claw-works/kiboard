//! JSON Lines 协议：hub <-> 设备（串口与 WebSocket 共用同一套消息）
use serde::{Deserialize, Serialize};

use crate::approval::Decision;

/// 设备 -> hub
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DeviceMsg {
    Hello {
        fw: String,
        keys: u8,
        #[serde(default)]
        leds: u8,
        #[serde(default)]
        disp: Option<String>,
        #[serde(default)]
        ip: Option<String>,
    },
    Pong {
        uptime_ms: u64,
    },
    /// 设备请求 hub 重画当前该显示的东西。
    ///
    /// 息屏唤醒时用：固件先立刻画出 logo 页（给即时反馈），再发这条让 hub 补画
    /// 真正该在的内容。如果此刻有待批请求，hub 的 present() 会盖上来——
    /// 于是"唤醒后看到的是最新状态"不依赖固件记住任何东西。
    Repaint {},
    Key {
        id: u8,
        /// v4 固件额外带上行列，老固件没有这两个字段
        #[serde(default)]
        row: Option<u8>,
        #[serde(default)]
        col: Option<u8>,
        /// 丝印标签由设备给。hub 不再持有键位表——键位是设备的事，
        /// 换个方案（触摸屏、手机）根本没有"第 3 号键"这种东西
        #[serde(default)]
        label: Option<String>,
        act: KeyAct,
    },
    /// 设备裁决。**这是审批路径的唯一入口**：设备自己把按键翻成语义，
    /// hub 不知道人按了哪个键，只知道人的意思。
    ///
    /// `id` 为空表示这条裁决不针对具体请求（clear_auto / cancel_all 这类队列控制）。
    Decision {
        #[serde(default)]
        id: Option<u64>,
        verdict: Verdict,
        #[serde(default)]
        confirm: Option<Confirm>,
    },
    /// 设备要一屏只有 hub 知道的数据（链路状态、审批历史）。
    /// 设备不知道这些内容，但它知道人想看什么。
    Query {
        what: String,
    },

    Wifi {
        status: String,
        #[serde(default)]
        ssid: Option<String>,
        #[serde(default)]
        ip: Option<String>,
        #[serde(default)]
        rssi: Option<i32>,
        #[serde(default)]
        reason: Option<u16>,
    },
    Ok {
        cmd: String,
    },
    /// 屏幕指令的回执。status 会带上折行后的总行数，hub 据此判断还能不能往下滚
    Disp {
        #[serde(default)]
        op: String,
        #[serde(default)]
        lines: Option<u16>,
    },
    /// 矩阵扫描诊断：matrix 是 4x4 原始读数，idle_cols 是空闲时列脚电平
    Keys {
        #[serde(default)]
        matrix: Vec<Vec<u8>>,
        #[serde(default)]
        idle_cols: Vec<u8>,
    },
    Err {
        msg: String,
    },
    /// 固件的 ESP-IDF 日志等非协议输出
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAct {
    Press,
    Long,
    Release,
}

/// 设备能表达的裁决。这是**语义**，不是按键——手机 App 上滑动确认发的也是这几个值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accept,
    Reject,
    /// 接受本次并开启「全部接受」窗口
    AcceptWindow,
    /// 取消当前及排队中的全部请求
    CancelAll,
    /// 关掉「全部接受」
    ClearAuto,
}

/// 强确认的证据。设备报**原始事件**而不是"我确认过了"这个结论：
///
/// 阈值留在 hub 才能改配置就生效（不用为一个常量重烧板子），而且 hub 能自己复核。
/// 这不防被改过的固件——设备在这个模型里是可信的哑终端；它防的是
/// "阈值散落到各设备实现里，各家判各家的"。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Confirm {
    pub method: String,
    #[serde(default)]
    pub events: Vec<ConfirmEvent>,
    /// true = 设备自述、hub 无法复核（如手机生物识别）
    #[serde(default)]
    pub asserted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfirmEvent {
    pub ev: String,
    /// 设备单调时钟毫秒，只用来算差值
    pub device_ts: u64,
}

impl Confirm {
    /// 从原始事件算按住时长。缺 press 或 release 一律算 0 —— 安全方向：
    /// 证据不全就当没按够，而不是当按够了
    pub fn held_ms(&self) -> u64 {
        let ts = |name: &str| {
            self.events.iter().find(|e| e.ev == name).map(|e| e.device_ts)
        };
        match (ts("press"), ts("release")) {
            (Some(a), Some(b)) if b >= a => b - a,
            _ => 0,
        }
    }
}


/// hub -> 设备
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostMsg {
    Ping,
    /// 主动问一次 Wi-Fi 状态（设备也会在状态变化时自己上报）
    #[allow(dead_code)]
    Wifi,
    Led {
        id: u8,
        mode: LedMode,
        #[serde(skip_serializing_if = "Option::is_none")]
        hz: Option<f32>,
    },
    /// 屏幕指令。v4 起用 disp（固件同时兼容旧的 tft）
    Disp(DispOp),
    /// 待审请求。**hub 只给字段，排版全在设备**——21 字符折行、滚动、分页
    /// 都是"这块屏多大"决定的事，hub 不该知道。
    Request(RequestMsg),
    /// 请求已有结果，设备可以收屏。verdict 只用于设备显示结果条
    RequestDone { id: u64, verdict: &'static str },
}

/// 推给设备的待审请求。字段顺序即重要性顺序，设备按自己的屏幕大小取舍。
#[derive(Debug, Clone, Serialize)]
pub struct RequestMsg {
    pub id: u64,
    /// 逐字原文。**required-to-display**：设备必须显示它，
    /// 且不得用 summary 取代——措辞良善内容危险的 summary 会让人在错误前提下批准
    pub verbatim: String,
    /// agent 自己写的意图说明，可信度最低，挤掉不影响判断
    pub summary: String,
    /// 来源短标签 kiro@kiboard
    pub label: String,
    /// 客户端简称
    pub client: String,
    /// 缩短后的工作目录。同一条命令在不同目录后果完全不同
    pub cwd: String,
    pub risk: crate::approval::Risk,
    /// 高危请求要按住多久。**由 hub 给**，这样改阈值不用重烧固件；
    /// 设备拿它做本地进度反馈（灯转常亮、提示松手），不必等网络往返
    pub hold_ms: u64,
    /// 排队中还有几条，设备可以显示在标题条上
    pub queued: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedMode {
    On,
    Off,
    Blink,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DispOp {
    Msg { text: String, color: String },
    MsgClear,
    /// skip 是滚动位置：跳过正文前几行。长命令一定放不下一屏，
    /// 截断而不让人知道是危险的，所以支持滚动 + 箭头提示
    /// 全屏视图。`transient` 区分"查询屏"和"审批屏"：
    ///
    /// 设备上 * 键的语义是"退一层，退到顶就熄屏"，但审批屏不能被这样退掉——
    /// 那是一条等着人裁决的请求，被顶掉就等于悄悄消失。设备自己看不出全屏是哪种，
    /// 所以由 hub 明确标注。**默认 false（当审批屏处理）**：老 hub 不带这个字段时，
    /// 新固件会把它当成不可退的审批屏，宁可多留一屏也不能弄丢一条请求。
    Status {
        mode: String,
        text: String,
        color: String,
        skip: usize,
        #[serde(default)]
        transient: bool,
    },
    /// 底部四格提示。屏幕一格只有 32px、放不下 4 个字符以上，
    /// 现在不再使用（见 firmware/src/display.h 的说明），接口留着以防换屏
    #[allow(dead_code)]
    Hints { h: [String; 4] },
    /// 退出全屏视图，回到时钟
    Clock,
    /// 顶栏左侧角标：非空时反色显示（用于「自动接受中」这类必须常驻可见的状态）
    Badge { text: String },
    /// 息屏 / 唤醒。SSD1306 能真正断电，不像 ST7735 的 BLK 硬接 3V3 关不掉
    Backlight { on: bool },
    /// 待机首屏的任务页。items 是标题（已排序、已过滤掉完成的），
    /// total 是实际条数——可能多于 items，设备据此显示"还有 n 条"
    Tasks { items: Vec<String>, total: usize },
    /// 告诉设备 hub 是哪一版，显示在 logo 页。
    /// 和 /health 带版本同一个目的：一眼看出连的是哪一版，不用猜
    HubInfo { version: String },
    Test,
}

impl HostMsg {
    pub fn msg(text: impl Into<String>, color: impl Into<String>) -> Self {
        HostMsg::Disp(DispOp::Msg { text: text.into(), color: color.into() })
    }
}

/// 推给 WS 订阅者的事件
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HubEvent {
    DeviceUp { fw: String, keys: u8 },
    DeviceDown,
    Key { id: u8, label: String, row: u8, col: u8, act: KeyAct },
    Wifi { status: String, ssid: Option<String>, rssi: Option<i32> },
    Mode { name: String },
    /// 新的审批请求已展示到设备上
    Request { id: u64, title: String, detail: String, risk: crate::approval::Risk },
    /// 请求有了结果。`by` 是裁决来源（device / api），不再是键号——
    /// 键号只有物理键盘才有，手机方案上没有这个概念
    Decision { id: u64, decision: Decision, by: Option<&'static str> },
    /// 自动裁决状态变化
    Auto { mode: &'static str, remaining_s: u64 },
    Log { text: String },
}
