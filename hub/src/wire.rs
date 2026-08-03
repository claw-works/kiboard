//! 客户端 <-> hub 的统一消息体
//!
//! 契约见 docs/client-protocol.md。这个模块被服务端和 kiboard-ask 客户端
//! 同时引用（同一个 crate 的两个 bin），改了字段两边一起编译报错，
//! 比两处手写 JSON 可靠。
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::{Decision, Risk};

/// 谁在请求。用于上屏、审计、选规则组。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Source {
    /// 客户端标识，例如 kiro-cli / claude-code。新客户端接入先注册一个名字
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub user: String,
}

impl Source {
    /// 上屏用的短标签：client@项目名。屏幕只有 21 字符宽，得省着用
    pub fn label(&self) -> String {
        let client = short_client(&self.client);
        let project = self
            .cwd
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("");
        match (client.is_empty(), project.is_empty()) {
            (true, true) => String::new(),
            (true, false) => project.to_string(),
            (false, true) => client.to_string(),
            (false, false) => format!("{client}@{project}"),
        }
    }
}

impl Source {
    /// 缩短后的工作目录，用于高危请求的审批界面。
    ///
    /// 为什么高危请求必须显示它：屏上只写 `rm -rf build` 的话，人无法判断 build 在哪——
    /// 同一条命令在不同目录后果完全不同，结果只能一律拒绝。一个让人无法判断的审批界面
    /// 只会训练出「一律拒绝」或「一律批准」两种坏习惯。
    ///
    /// 屏幕一行只有 21 个 ASCII，放不下完整路径。保留尾部（最具体的部分），
    /// 前面加 … 表示有省略；路径本身够短就完整显示。
    pub fn cwd_short(&self, max: usize) -> String {
        let cwd = self.cwd.trim_end_matches('/');
        if cwd.is_empty() {
            return String::new();
        }
        if cwd.chars().count() <= max {
            return cwd.to_string();
        }
        // 从尾部往前凑整段，凑不下一整段就直接按字符截
        let segs: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
        let mut out = String::new();
        for seg in segs.iter().rev() {
            let candidate = if out.is_empty() {
                seg.to_string()
            } else {
                format!("{seg}/{out}")
            };
            // +2 是给前缀 …/ 留位置
            if candidate.chars().count() + 2 > max {
                break;
            }
            out = candidate;
        }
        if out.is_empty() {
            // 连最后一段都放不下，硬截尾部
            let tail: String = cwd.chars().rev().take(max.saturating_sub(2)).collect();
            out = tail.chars().rev().collect();
        }
        format!("…/{out}")
    }
}

/// kiro-cli -> kiro，claude-code -> cc：屏幕太窄，长名字挤掉正文没意义
pub fn short_client(c: &str) -> &str {
    match c {
        "kiro-cli" => "kiro",
        "claude-code" => "cc",
        other => other,
    }
}

/// 被拦下的工具调用。input 原样透传，hub 不解析结构。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

impl ToolCall {
    /// 规则匹配和审计都作用在这个扁平化文本上。
    /// 各客户端参数字段名不同，逐个写映射是没完的活；对文本写正则一条通吃。
    pub fn input_text(&self) -> String {
        match &self.input {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Intent {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Policy {
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// closed | open。仅供审计记录，真正的失败处置在客户端脚本里
    #[serde(default)]
    pub on_failure: Option<String>,
}

/// POST /approve 的请求体。
/// 同时兼容旧的扁平形态（title/detail/risk/timeout_s），方便手工触发和调试。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ApproveRequest {
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub tool: ToolCall,
    #[serde(default)]
    pub intent: Intent,
    #[serde(default)]
    pub policy: Policy,

    // ---- 旧的扁平形态 ----
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// 显式指定风险等级，优先于规则表
    #[serde(default)]
    pub risk: Option<Risk>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

impl ApproveRequest {
    /// 屏幕上给人看的标题。客户端给了 intent 就用它，否则从工具调用生成。
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.title
            && !t.is_empty() {
                return t.clone();
            }
        if !self.intent.title.is_empty() {
            return self.intent.title.clone();
        }
        // 从 tool 生成：命令类工具直接显示命令本身，比 "execute_bash" 有用得多
        let text = self.tool.input_text();
        if text.is_empty() {
            return self.tool.name.clone();
        }
        // 去掉 JSON 包装，尽量露出人能读的部分
        let cleaned = strip_json_noise(&text);
        if self.tool.name.is_empty() {
            cleaned
        } else if cleaned.is_empty() {
            self.tool.name.clone()
        } else {
            cleaned
        }
    }

    pub fn display_detail(&self) -> String {
        if let Some(d) = &self.detail
            && !d.is_empty() {
                return d.clone();
            }
        self.intent.detail.clone()
    }

    pub fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_s.or(self.policy.timeout_s)
    }
}

/// `{"command":"rm -rf build"}` -> `rm -rf build`
/// 只做最朴素的处理：单键对象且值是字符串时取值，否则原样返回。
/// 屏幕标题优先取这些键，按顺序找第一个命中的。
///
/// 顺序即优先级：先显示「真正要做的动作」。故意不包含 summary 之类由模型自己写的
/// 说明字段——那些只能当 detail。屏幕上必须先显示真实的命令/路径，否则一个措辞良善、
/// 内容危险的说明会让人在错误的前提下批准。
const TITLE_KEYS: [&str; 7] =
    ["command", "cmd", "path", "file_path", "url", "query", "operation_name"];

/// `{"command":"rm -rf build","summary":"清理"}` -> `rm -rf build`
///
/// 屏幕只有 21 字符宽 5 行，把整个 JSON 糊上去等于什么都没显示。
fn strip_json_noise(text: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    for key in TITLE_KEYS {
        if let Some(Value::String(s)) = map.get(key)
            && !s.is_empty()
        {
            return s.clone();
        }
    }
    // 单键对象直接取值：键名不在白名单里也无妨，反正只有一个
    if map.len() == 1
        && let Some(Value::String(s)) = map.values().next()
    {
        return s.clone();
    }
    text.to_string()
}

/// POST /approve 的响应体
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApproveResponse {
    pub ok: bool,
    #[serde(default)]
    pub id: u64,
    pub decision: Decision,
    pub approved: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub risk: String,
    #[serde(default)]
    pub rule: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 标签取客户端简称与项目名() {
        let s = Source {
            client: "kiro-cli".into(),
            cwd: "/Users/x/projects/kiboard".into(),
            ..Default::default()
        };
        assert_eq!(s.label(), "kiro@kiboard");
    }

    #[test]
    fn 缩短工作目录保留尾部并标出省略() {
        let s = Source {
            cwd: "/Users/wellxie/projects/claw-works/kiboard".into(),
            ..Default::default()
        };
        // 20 字符放不下完整路径，应保留最具体的尾部并加省略号
        let short = s.cwd_short(20);
        assert!(short.starts_with("…/"), "{short}");
        assert!(short.ends_with("kiboard"), "{short}");
        assert!(short.chars().count() <= 20, "{short} 超宽");
    }

    #[test]
    fn 短路径完整显示不加省略号() {
        let s = Source { cwd: "/tmp/x".into(), ..Default::default() };
        assert_eq!(s.cwd_short(20), "/tmp/x");
        let root = Source { cwd: "/".into(), ..Default::default() };
        assert_eq!(root.cwd_short(20), "", "根目录去掉尾斜杠后为空，交给调用方处理");
    }

    #[test]
    fn 标签容忍尾部斜杠与缺字段() {
        let s = Source { client: "claude-code".into(), cwd: "/a/b/".into(), ..Default::default() };
        assert_eq!(s.label(), "cc@b");
        assert_eq!(Source::default().label(), "");
    }

    #[test]
    fn 标题优先用意图其次从工具生成() {
        let mut r = ApproveRequest {
            tool: ToolCall {
                name: "execute_bash".into(),
                input: serde_json::json!({"command": "rm -rf build"}),
            },
            ..Default::default()
        };
        assert_eq!(r.display_title(), "rm -rf build", "单键对象应剥掉 JSON 外壳");
        r.intent.title = "清理构建产物".into();
        assert_eq!(r.display_title(), "清理构建产物");
    }

    #[test]
    fn 多字段输入取动作字段而不是糊整个json() {
        let r = ApproveRequest {
            tool: ToolCall {
                name: "fs_write".into(),
                input: serde_json::json!({"path": "/etc/hosts", "text": "x"}),
            },
            ..Default::default()
        };
        assert_eq!(r.display_title(), "/etc/hosts");
    }

    #[test]
    fn 标题取真实命令而非模型写的summary() {
        // kiro 的 tool_input 同时有 command 与 summary，标题必须是 command
        let r = ApproveRequest {
            tool: ToolCall {
                name: "execute_bash".into(),
                input: serde_json::json!({
                    "command": "npm install some-package",
                    "summary": "安装依赖"
                }),
            },
            ..Default::default()
        };
        assert_eq!(r.display_title(), "npm install some-package");
    }

    #[test]
    fn 认不出的结构才退回原文() {
        let r = ApproveRequest {
            tool: ToolCall {
                name: "weird".into(),
                input: serde_json::json!({"a": 1, "b": 2}),
            },
            ..Default::default()
        };
        assert!(r.display_title().starts_with('{'), "{}", r.display_title());
    }

    #[test]
    fn 扁平化文本用于规则匹配() {
        let t = ToolCall {
            name: "execute_bash".into(),
            input: serde_json::json!({"command": "sudo reboot"}),
        };
        assert_eq!(t.input_text(), r#"{"command":"sudo reboot"}"#);
    }

    #[test]
    fn 旧的扁平请求体仍能解析() {
        let r: ApproveRequest =
            serde_json::from_str(r#"{"title":"t","detail":"d","risk":"high","timeout_s":30}"#)
                .unwrap();
        assert_eq!(r.display_title(), "t");
        assert_eq!(r.display_detail(), "d");
        assert_eq!(r.risk, Some(Risk::High));
        assert_eq!(r.timeout_seconds(), Some(30));
    }
}
