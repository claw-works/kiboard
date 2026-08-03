//! agent 状态上报：让设备平时不是块死屏
//!
//! 审批是"要人做决定"的时刻，但那只占很小一部分时间。其余时候设备应该能回答
//! 一个更日常的问题：**现在轮到我了吗？** 抬头看一眼灯就知道，不用切回终端。
//!
//! 状态从客户端的 hook 推过来（agentSpawn / userPromptSubmit / postToolUse / stop）。
//! 关键约束：**上报必须是 fire-and-forget** —— 短超时、永远 exit 0。
//! 一个"看看它在干什么"的功能绝不能变成新的失败模式：hub 挂了不该让 agent 卡住。
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::protocol::{DispOp, HostMsg, LedMode};
use crate::state::Shared;
use crate::wire::Source;

/// agent 现在处于什么状态。取值刻意少——设备只有一块小屏和三个灯，
/// 分得太细人也看不出差别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// 会话开始
    Start,
    /// 正在干活（收到指令、工具执行中）
    Working,
    /// 一轮结束，轮到人了
    YourTurn,
    /// 出错了
    Error,
    /// 会话结束或空闲
    Idle,
}

impl AgentState {
    /// 屏幕上那一行。前面留着放来源标签，所以这里只写状态本身
    fn text(self) -> &'static str {
        match self {
            AgentState::Start => "session start",
            AgentState::Working => "working...",
            AgentState::YourTurn => "YOUR TURN",
            AgentState::Error => "ERROR",
            AgentState::Idle => "idle",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct StateReport {
    #[serde(default)]
    pub source: Source,
    pub state: AgentState,
    /// 一句话补充，比如出错信息或正在跑的工具名
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateView {
    pub label: String,
    pub state: AgentState,
    pub detail: String,
    pub age_s: u64,
}

#[derive(Debug)]
pub struct Current {
    label: String,
    state: AgentState,
    detail: String,
    at: Instant,
}

impl Current {
    pub fn view(&self) -> StateView {
        StateView {
            label: self.label.clone(),
            state: self.state,
            detail: self.detail.clone(),
            age_s: self.at.elapsed().as_secs(),
        }
    }
}

/// 记下状态，并在设备空闲时把它画到屏上。
///
/// 有待批请求时**不动屏幕**——那时屏幕在问一个需要决定的问题，
/// 用状态信息去覆盖它是本末倒置。
pub async fn apply(shared: &Shared, report: &StateReport, device_idle: bool) -> Current {
    let label = report.source.label();
    let cur = Current {
        label: label.clone(),
        state: report.state,
        detail: report.detail.clone(),
        at: Instant::now(),
    };

    if !device_idle {
        return cur;
    }

    // 蓝灯（板载）表示"agent 在忙还是在等你"，和黄灯的审批语义分开：
    //   慢闪 = 在干活    常亮 = 轮到你了    灭 = 空闲
    // 红灯只在出错时亮一下，避免它常亮变成背景噪音让人无视
    let (led2, led1) = match report.state {
        AgentState::Working | AgentState::Start => (LedMode::Blink, LedMode::Off),
        AgentState::YourTurn => (LedMode::On, LedMode::Off),
        AgentState::Error => (LedMode::Off, LedMode::On),
        AgentState::Idle => (LedMode::Off, LedMode::Off),
    };
    let hz = if matches!(led2, LedMode::Blink) { Some(1.0) } else { None };
    shared.send(HostMsg::Led { id: 2, mode: led2, hz }).await;
    shared.send(HostMsg::Led { id: 1, mode: led1, hz: None }).await;

    // 出错时红灯亮一会儿就灭，别让它常亮
    if report.state == AgentState::Error {
        let s = shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            s.send(HostMsg::Led { id: 1, mode: LedMode::Off, hz: None }).await;
        });
    }

    // 屏幕消息区：来源 + 状态 + 补充。空闲状态直接清掉，别留一行陈旧信息
    if report.state == AgentState::Idle {
        shared.send(HostMsg::Disp(DispOp::MsgClear)).await;
        return cur;
    }
    let mut line = if label.is_empty() {
        String::new()
    } else {
        format!("{label} ")
    };
    line.push_str(report.state.text());
    if !report.detail.is_empty() {
        line.push(' ');
        line.push_str(&report.detail);
    }
    let style = if report.state == AgentState::YourTurn || report.state == AgentState::Error {
        "yellow"
    } else {
        "white"
    };
    shared.send(HostMsg::msg(line, style)).await;
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态文本简短到能放进一行() {
        for st in [
            AgentState::Start,
            AgentState::Working,
            AgentState::YourTurn,
            AgentState::Error,
            AgentState::Idle,
        ] {
            // 屏幕一行 21 个 ASCII，来源标签要占十几个，状态本身得短
            assert!(st.text().len() <= 13, "{:?} 的文本太长: {}", st, st.text());
        }
    }

    #[test]
    fn 上报可以只给状态不给来源() {
        let r: StateReport = serde_json::from_str(r#"{"state":"working"}"#).unwrap();
        assert_eq!(r.state, AgentState::Working);
        assert_eq!(r.detail, "");
    }
}
