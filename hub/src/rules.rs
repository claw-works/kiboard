//! 风险分级规则表
//!
//! 规则放在 hub 而不是客户端脚本里，理由：规则集中可审计、改规则不用动每台机器、
//! 新客户端接入只需在这里补一组工具名。
//!
//! 匹配作用在 `tool.input` 的 JSON 序列化文本上，而不是解析后的字段。
//! 各客户端的参数字段名不一样（execute_bash 的 command vs Claude Code 的 Bash），
//! 逐个写映射是没完的活；对扁平化文本写正则，一条规则对所有客户端成立。
use std::path::Path;

use regex::RegexSet;
use serde::Deserialize;
use tracing::{info, warn};

use crate::approval::Risk;

/// 分级结果。allow 表示不必打扰人。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 直接放行，不上屏
    Allow,
    /// 需要人裁决
    Ask(Risk),
}

#[derive(Debug, Deserialize)]
struct RawRule {
    name: String,
    #[serde(default)]
    clients: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
    /// allow | normal | high
    risk: String,
}

#[derive(Debug, Deserialize)]
struct RawRules {
    #[serde(default = "default_risk")]
    default: String,
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
}

fn default_risk() -> String {
    "normal".into()
}

struct Rule {
    name: String,
    clients: Vec<String>,
    tools: Vec<String>,
    set: RegexSet,
    /// 没有有效 patterns：只按工具名匹配
    match_all: bool,
    verdict: Verdict,
}

pub struct Rules {
    rules: Vec<Rule>,
    default: Verdict,
    /// 规则文件原文与指纹，用于 GET /rules 下发给客户端做本地缓存
    source: String,
    etag: String,
}

/// 内容指纹。不引 sha2：这里只用来判断"规则变没变"，FNV-1a 足够，
/// 而且客户端拿它做条件请求，碰撞的后果只是少刷一次缓存
fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn parse_verdict(s: &str) -> Option<Verdict> {
    match s {
        "allow" => Some(Verdict::Allow),
        "normal" => Some(Verdict::Ask(Risk::Normal)),
        "high" => Some(Verdict::Ask(Risk::High)),
        _ => None,
    }
}

impl Rules {
    /// 从规则文件原文解析。客户端缓存的是原文，所以它走这个入口，
    /// 与服务端共用同一套解析和匹配逻辑——规则语义只有一个实现，不会两边对不上。
    pub fn from_toml(text: &str) -> Self {
        Self::parse(text.to_string())
    }

    /// 规则文件缺失不是错误：没有规则表就一切按 default 走（问一句）。
    /// 这样部署时忘了拷 rules.toml 也不会变成静默放行。
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                warn!("规则文件 {} 读不到（{e}），全部按 normal 处理", path.display());
                return Self::empty();
            }
        };
        Self::parse(text)
    }

    fn parse(text: String) -> Self {
        let raw: RawRules = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                warn!("规则表解析失败（{e}），全部按 normal 处理");
                return Self::empty();
            }
        };

        let default = parse_verdict(&raw.default).unwrap_or(Verdict::Ask(Risk::Normal));
        let mut rules = Vec::new();
        for r in raw.rules {
            let Some(verdict) = parse_verdict(&r.risk) else {
                warn!("规则 {} 的 risk={} 不认识，跳过", r.name, r.risk);
                continue;
            };
            let patterns: Vec<String> =
                r.patterns.iter().filter(|p| !p.is_empty()).cloned().collect();
            let match_all = patterns.is_empty();
            // 正则写错宁可跳过这条规则，也不要让整个规则表加载失败
            match RegexSet::new(&patterns) {
                Ok(set) => rules.push(Rule {
                    name: r.name,
                    clients: r.clients,
                    tools: r.tools,
                    set,
                    match_all,
                    verdict,
                }),
                Err(e) => warn!("规则 {} 的正则无效（{e}），跳过", r.name),
            }
        }
        let etag = fingerprint(&text);
        info!("规则表加载完成：{} 条，default={:?}，etag={etag}", rules.len(), default);
        Self { rules, default, source: text, etag }
    }

    /// 加载失败时的退化：没有规则，一切按 normal（问一句），而不是放行。
    /// source 留空，客户端拿到空规则表也只会退化成"每次都问"，方向仍然安全。
    fn empty() -> Self {
        Self {
            rules: Vec::new(),
            default: Verdict::Ask(Risk::Normal),
            source: String::new(),
            etag: "empty".into(),
        }
    }

    /// 规则文件原文，供客户端缓存到本地做 allow 短路
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn etag(&self) -> &str {
        &self.etag
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 空列表视为「匹配全部」，`*` 同义
    fn list_matches(list: &[String], value: &str) -> bool {
        list.is_empty() || list.iter().any(|v| v == "*" || v == value)
    }

    /// 返回（结论，命中的规则名）
    pub fn classify(&self, client: &str, tool: &str, input_text: &str) -> (Verdict, String) {
        for r in &self.rules {
            if !Self::list_matches(&r.clients, client) {
                continue;
            }
            if !Self::list_matches(&r.tools, tool) {
                continue;
            }
            // 没写 patterns（或只写了空串）= 只按工具名匹配，命中该工具的任何调用。
            // 「委派给子 agent」这类规则就是这样：不看参数，这个动作本身就要人批。
            if r.match_all || r.set.is_match(input_text) {
                return (r.verdict, r.name.clone());
            }
        }
        (self.default, "default".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/rules.toml")))
    }

    #[test]
    fn 递归删除算高危() {
        let (v, name) = rules().classify(
            "kiro-cli",
            "execute_bash",
            r#"{"command":"rm -rf build"}"#,
        );
        assert_eq!(v, Verdict::Ask(Risk::High), "命中规则: {name}");
    }

    #[test]
    fn 强推算高危() {
        let (v, _) = rules().classify(
            "kiro-cli",
            "execute_bash",
            r#"{"command":"git push --force origin main"}"#,
        );
        assert_eq!(v, Verdict::Ask(Risk::High));
    }

    #[test]
    fn 只读命令直接放行() {
        let (v, _) =
            rules().classify("kiro-cli", "execute_bash", r#"{"command":"git status"}"#);
        assert_eq!(v, Verdict::Allow);
        let (v, _) = rules().classify("kiro-cli", "execute_bash", r#"{"command":"ls -la"}"#);
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn 普通命令落到默认档() {
        let (v, name) = rules().classify(
            "kiro-cli",
            "execute_bash",
            r#"{"command":"npm install lodash"}"#,
        );
        assert_eq!(v, Verdict::Ask(Risk::Normal));
        assert_eq!(name, "default");
    }

    #[test]
    fn 同一条正则对别的客户端也成立() {
        // Claude Code 的工具名不同、字段名也可能不同，但扁平化文本一样能命中
        let (v, _) = rules().classify("claude-code", "Bash", r#"{"cmd":"sudo rm -rf /tmp/x"}"#);
        assert_eq!(v, Verdict::Ask(Risk::High));
    }

    #[test]
    fn 委派给子agent一律高危() {
        // 这条规则没写 patterns，应该只按工具名命中，不看参数
        let (v, name) = rules().classify("kiro-cli", "use_subagent", r#"{"query":"随便什么"}"#);
        assert_eq!(v, Verdict::Ask(Risk::High), "命中: {name}");
        assert_eq!(name, "委派给子 agent");
    }

    #[test]
    fn 改动审批链路自身算高危() {
        let r = rules();
        for input in [
            r#"{"path":"/Users/x/.kiro/agents/foo.json","text":"..."}"#,
            r#"{"command":"vi clients/kiro-cli/hook.sh"}"#,
            r#"{"command":"echo x > rules.toml"}"#,
        ] {
            let (v, name) = r.classify("kiro-cli", "fs_write", input);
            assert_eq!(v, Verdict::Ask(Risk::High), "{input} 应为高危，实际命中 {name}");
        }
    }

    #[test]
    fn 规则文件缺失时按普通档而不是放行() {
        let r = Rules::load(Path::new("/nonexistent/rules.toml"));
        let (v, _) = r.classify("x", "y", "z");
        assert_eq!(v, Verdict::Ask(Risk::Normal), "缺规则表不能变成静默放行");
    }
}
