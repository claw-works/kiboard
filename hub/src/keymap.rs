//! 16 键的键位与动作映射
//!
//! 物理布局（id = (行-1)*4 + (列-1)，行 1→4 为上→下，列 1→4 为左→右）：
//!
//! ```text
//!   id0 [1]   id1 [2]   id2 [3]   id3 [A]
//!   id4 [4]   id5 [5]   id6 [6]   id7 [B]
//!   id8 [7]   id9 [8]   id10[9]   id11[C]
//!   id12[*]   id13[0]   id14[#]   id15[D]
//! ```
//!
//! 现在只绑三个核心动作（接受 / 拒绝 / 全部接受），放在第一行最好记的位置。
//! 外加 D 键清除「全部接受」——一个只能开不能关的自动批准是个陷阱，必须有关闭开关。
//! 其余 11 键只广播 key 事件，留给后续再定。
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// 接受本次
    Accept,
    /// 拒绝本次
    Reject,
    /// 接受本次，并在 TTL 内自动接受后续（含已排队的）
    AcceptAll,
    /// 清除自动接受状态
    ClearAuto,
    /// 正文向上滚一行
    ScrollUp,
    /// 正文向下滚一行
    ScrollDown,
    /// 取消当前及排队中的全部请求
    CancelAll,
    /// 息屏 / 唤醒。新固件自己处理 * 键，这条只用于兼容老固件
    ToggleScreen,
    /// 显示链路与状态一览
    ShowInfo,
    /// 显示审批过的（最近裁决列表）
    ShowRecent,
    /// 显示最近一次审批的详情
    ShowLastDetail,
    /// 未绑定动作，只广播事件
    None,
}

/// 丝印标签，用于日志和屏幕提示
pub const LABELS: [&str; 16] = [
    "1", "2", "3", "A", //
    "4", "5", "6", "B", //
    "7", "8", "9", "C", //
    "*", "0", "#", "D", //
];

pub fn label(id: u8) -> &'static str {
    LABELS.get(id as usize).copied().unwrap_or("?")
}

pub fn action(id: u8) -> Action {
    match id {
        0 => Action::Accept,
        1 => Action::Reject,
        2 => Action::AcceptAll,
        3 => Action::ScrollUp,
        5 => Action::ShowRecent,
        6 => Action::ShowLastDetail,
        7 => Action::ScrollDown,
        11 => Action::CancelAll,
        12 => Action::ToggleScreen,
        13 => Action::ShowInfo,
        15 => Action::ClearAuto,
        _ => Action::None,
    }
}

/// 屏幕标题条。真正会变的信息只有「短按还是按住」，写进标题里比画一排看不懂的
/// 提示格有用得多——一格 32px 放不下 4 个字符以上，"H1.2s" 会截成 "H1.~"。
pub fn head(high_risk: bool, hold_ms: u64, client: &str, queued: usize) -> String {
    let mut h = if high_risk {
        format!("!! HOLD1 {:.1}s", hold_ms as f64 / 1000.0)
    } else {
        "APPROVE?".to_string()
    };
    if queued > 0 {
        h.push_str(&format!(" +{queued}"));
    }
    if !client.is_empty() {
        h.push(' ');
        h.push_str(client);
    }
    h
}
