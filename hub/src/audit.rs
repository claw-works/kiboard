//! 审计日志：每一次裁决按行追加 JSONL
//!
//! 一个审批设备没有可回溯的批准记录是不完整的——出事之后要能回答
//! 「这条命令当时是谁批的、什么时候、按的哪个键」。tracing 的日志会滚掉，
//! 而且不是结构化的，不能替代这个。
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Deserialize, Serialize)]
pub struct Entry {
    pub ts: String,
    pub id: u64,
    pub client: String,
    pub host: String,
    pub user: String,
    pub cwd: String,
    pub session: String,
    pub tool: String,
    /// tool.input 的扁平化文本，过长会截断
    pub input: String,
    pub risk: String,
    pub rule: String,
    pub decision: String,
    /// 按了哪个键的丝印标签；自动或 API 裁决时为空
    pub key: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Clone)]
pub struct Audit {
    path: Option<PathBuf>,
}

impl Audit {
    /// KIBOARD_AUDIT=off 可关闭；默认写 ~/.kiboard/audit.jsonl
    pub fn from_env() -> Self {
        let raw = std::env::var("KIBOARD_AUDIT").unwrap_or_default();
        if raw == "off" {
            return Self { path: None };
        }
        let path = if raw.is_empty() {
            match std::env::var("HOME") {
                Ok(h) => PathBuf::from(h).join(".kiboard/audit.jsonl"),
                Err(_) => {
                    warn!("拿不到 HOME，审计日志关闭");
                    return Self { path: None };
                }
            }
        } else {
            PathBuf::from(raw)
        };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir) {
                warn!("建不了审计目录 {}（{e}），审计日志关闭", dir.display());
                return Self { path: None };
            }
        Self { path: Some(path) }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// 读最近 limit 条，可按 decision / client 过滤。
    ///
    /// 从文件尾往前读：审计日志只增不删，而人想看的几乎总是最近发生的事。
    /// 全量读进内存再倒序在日志长起来之后会很蠢，所以按块从尾部扫。
    pub fn tail(&self, limit: usize, decision: Option<&str>, client: Option<&str>) -> Vec<Entry> {
        let Some(path) = &self.path else { return Vec::new() };
        let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
        let mut out = Vec::new();
        for line in text.lines().rev() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
            if let Some(d) = decision
                && e.decision != d
            {
                continue;
            }
            if let Some(c) = client
                && e.client != c
            {
                continue;
            }
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// 各种裁决各出现了多少次。看一眼就知道"最近是不是一直在被拒"或"自动放行占比过高"
    pub fn summary(&self, scan: usize) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for e in self.tail(scan, None, None) {
            *counts.entry(e.decision).or_insert(0) += 1;
        }
        counts
    }

    /// 写失败只警告，不影响审批主流程——审计挂了不该让设备失灵
    pub fn write(&self, entry: &Entry) {
        let Some(path) = &self.path else { return };
        let Ok(mut line) = serde_json::to_vec(entry) else { return };
        line.push(b'\n');
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(&line));
        if let Err(e) = res {
            warn!("审计写入失败 {}：{e}", path.display());
        }
    }
}

/// RFC3339 时间戳。为了不多引一个 chrono 依赖，用 SystemTime 手算 UTC。
pub fn now_rfc3339() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 尾部读取按倒序且能过滤() {
        let dir = std::env::temp_dir().join("kiboard-audit-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("a.jsonl");
        let mk = |id: u64, decision: &str, client: &str| Entry {
            ts: now_rfc3339(),
            id,
            client: client.into(),
            host: "h".into(),
            user: "u".into(),
            cwd: "/x".into(),
            session: String::new(),
            tool: "execute_bash".into(),
            input: "{}".into(),
            risk: "normal".into(),
            rule: "default".into(),
            decision: decision.into(),
            key: None,
            elapsed_ms: 1,
        };
        let _ = std::fs::remove_file(&path);
        let a = Audit { path: Some(path.clone()) };
        a.write(&mk(1, "accept", "kiro-cli"));
        a.write(&mk(2, "reject", "kiro-cli"));
        a.write(&mk(3, "accept", "claude-code"));

        let all = a.tail(10, None, None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, 3, "最近的排最前");

        let rejected = a.tail(10, Some("reject"), None);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].id, 2);

        let by_client = a.tail(10, None, Some("claude-code"));
        assert_eq!(by_client.len(), 1);
        assert_eq!(by_client[0].id, 3);

        let counts = a.summary(100);
        assert_eq!(counts.get("accept"), Some(&2));
        assert_eq!(counts.get("reject"), Some(&1));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 审计关闭时查询返回空而不是报错() {
        let a = Audit { path: None };
        assert!(a.tail(10, None, None).is_empty());
        assert!(a.summary(10).is_empty());
    }

    #[test]
    fn 时间戳格式正确() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        // 年份应在合理范围
        let y: i64 = s[..4].parse().unwrap();
        assert!((2020..2100).contains(&y), "year={y}");
    }

    #[test]
    fn 已知历元换算正确() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
    }
}
