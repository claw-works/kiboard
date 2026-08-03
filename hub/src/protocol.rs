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
        act: KeyAct,
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
    Key { id: u8, label: &'static str, row: u8, col: u8, act: KeyAct },
    Wifi { status: String, ssid: Option<String>, rssi: Option<i32> },
    Mode { name: String },
    /// 新的审批请求已展示到设备上
    Request { id: u64, title: String, detail: String, risk: crate::approval::Risk },
    /// 请求有了结果
    Decision { id: u64, decision: Decision, key: Option<u8> },
    /// 自动裁决状态变化
    Auto { mode: &'static str, remaining_s: u64 },
    Log { text: String },
}
