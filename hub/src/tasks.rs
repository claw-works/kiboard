//! 任务列表：让设备待机时能回答"agent 现在在做什么"
//!
//! 和状态上报（agentstate）的分工：那个回答"轮到我了吗"，只有一个瞬时状态；
//! 这个回答**它现在正在做的几件事**。
//!
//! 三条刻意的设计：
//!
//! 1. **只显示进行中的**。待办不上屏——待办是计划，计划随时会变，
//!    而站在设备前的人关心的是"此刻在动什么"。已完成的同理，屏幕只有 4 行，
//!    历史和计划都没有位置。
//!
//! 2. **按「api key + agent」分桶，每个 api key 名下最多 100 条**。
//!    api key 代表租户（将来做多用户时就是账号），桶内再按 agent 分——
//!    实测发现只按 api key 分不行：同一个人的 kiro 和 cc 用的是同一个 key，
//!    会互相把对方的任务覆盖掉。而这两个恰恰是最需要同时看到的。
//!    100 条是租户级的防御上限，超了就淘汰最久没上报的 agent；
//!    正常上报是全量替换、不累积，这个上限只为防一个写错循环的客户端打爆内存。
//!
//! 3. **不落盘**。hub 重启后旧任务大概已经过时，显示过时的进度比显示空的更误导。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::protocol::{DispOp, HostMsg};
use crate::state::Shared;
use crate::wire::{short_client, Source};

/// 设备一屏放得下的条数。多的只报个数，不占行
pub const DEVICE_SLOTS: usize = 6;
/// 每个 api key 最多留多少条。防御性上限，不是业务语义
pub const MAX_PER_KEY: usize = 100;
/// 多久没上报就认为这个 agent 的任务已经过时，不再上屏。
///
/// 没有这条会出实际问题：一个跑完就退出的 session 不会来说"我结束了"，
/// 它最后那件"正在做"的事会永远挂在屏幕上。而待机屏的价值全在于**可信**——
/// 显示一件半小时前的事比显示空的更糟。
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 待办。**不上屏**——计划随时会变，设备上只显示此刻在动的
    #[default]
    Todo,
    /// 进行中，上屏
    Doing,
    /// 已完成，不上屏
    Done,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskEntry {
    pub title: String,
    #[serde(default)]
    pub status: TaskStatus,
}

#[derive(Debug, Deserialize)]
pub struct TaskReport {
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub tasks: Vec<TaskEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketView {
    /// 客户端简称，kiro / cc / codex
    pub client: String,
    pub label: String,
    pub tasks: Vec<TaskEntry>,
    pub age_s: u64,
}

#[derive(Debug, Clone)]
struct Bucket {
    key_fp: String,
    client: String,
    label: String,
    tasks: Vec<TaskEntry>,
    at: Instant,
}

/// 按 api key 分桶的任务表。
///
/// key 用的是 api key 的指纹而不是明文——它会出现在 GET /tasks 的响应里，
/// 而响应可能被贴到日志或聊天窗口。
#[derive(Debug, Default)]
pub struct Tasks {
    buckets: HashMap<String, Bucket>,
}

impl Tasks {
    /// 全量替换某个 key 名下的任务。
    ///
    /// 替换而不是追加：agent 每次把当前列表整份推过来。
    /// 增量同步的状态机是 bug 的温床，而这点数据量全量推毫无代价。
    pub fn replace(&mut self, key_fp: &str, report: TaskReport) {
        let mut tasks = report.tasks;
        tasks.truncate(MAX_PER_KEY);
        let client = short_client(&report.source.client).to_string();
        let label = report.source.label();
        // 桶 id = 租户 + agent 身份。
        //
        // agent 身份优先用 session：它才是"这一次 agent 运行"的准确标识。
        // 一开始用的是 label（client@目录名），实测出问题——同一个 agent 换个目录
        // 就分成两个桶，两桶各显示一行、内容还可能一样，屏幕上就是重复。
        // session 拿不到时退回 label，总比全塞进一个桶好。
        let identity = if report.source.session.is_empty() {
            label.clone()
        } else {
            report.source.session.clone()
        };
        let id = format!("{key_fp}/{identity}");
        self.buckets.insert(
            id,
            Bucket {
                key_fp: key_fp.to_string(),
                client,
                label,
                tasks,
                at: Instant::now(),
            },
        );
        self.enforce_key_limit(key_fp);
        self.drop_stale();
    }

    /// 租户级上限：一个 api key 名下总条数超了就淘汰最久没上报的 agent。
    /// 淘汰整桶而不是截断条目——半截的任务列表比没有更让人误解
    fn enforce_key_limit(&mut self, key_fp: &str) {
        loop {
            let total: usize = self
                .buckets
                .values()
                .filter(|b| b.key_fp == key_fp)
                .map(|b| b.tasks.len())
                .sum();
            if total <= MAX_PER_KEY {
                return;
            }
            let victim = self
                .buckets
                .iter()
                .filter(|(_, b)| b.key_fp == key_fp)
                .max_by_key(|(_, b)| b.at.elapsed())
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.buckets.remove(&k);
                }
                None => return,
            }
        }
    }

    /// 丢掉太久没上报的桶。在每次读取前调用，而不是起后台任务定时清——
    /// 数据量小，惰性清理就够，少一个需要停止的任务
    fn drop_stale(&mut self) {
        self.buckets.retain(|_, b| b.at.elapsed() < STALE_AFTER);
    }

    pub fn views(&self) -> Vec<BucketView> {
        let mut out: Vec<BucketView> = self
            .buckets
            .values()
            .filter(|b| b.at.elapsed() < STALE_AFTER)
            .map(|b| BucketView {
                client: b.client.clone(),
                label: b.label.clone(),
                tasks: b.tasks.clone(),
                age_s: b.at.elapsed().as_secs(),
            })
            .collect();
        // 新报的排前面，设备也按这个顺序显示
        out.sort_by_key(|v| v.age_s);
        out
    }

    /// 上屏用的行：`[kiro] 正在做的事`。
    ///
    /// 带客户端 tag 是因为一个 hub 会同时接 kiro / cc / codex，
    /// 不标出来就不知道这条是谁在做——多客户端下这是必要信息而不是装饰。
    pub fn display_lines(&self) -> Vec<String> {
        let mut buckets: Vec<&Bucket> =
            self.buckets.values().filter(|b| b.at.elapsed() < STALE_AFTER).collect();
        buckets.sort_by_key(|b| b.at.elapsed());
        let mut out = Vec::new();
        for b in buckets {
            // 进度：只显示进行中的话，一行看不出整体到哪了。
            // 用"第几件/共几件"而不是"完成数/总数"——人问的是"做到哪了"
            let total = b.tasks.len();
            let done = b.tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
            for t in b.tasks.iter().filter(|t| t.status == TaskStatus::Doing) {
                let mut line = String::new();
                if !b.client.is_empty() {
                    line.push_str(&format!("[{}] ", b.client));
                }
                line.push_str(&t.title);
                // 只有一件事时 "1/1" 是噪音，不加
                if total > 1 {
                    line.push_str(&format!(" {}/{total}", done + 1));
                }
                out.push(line);
            }
        }
        out
    }
}

/// 记下任务列表，并在设备空闲时推给它。
///
/// `device_idle` 为假时只记不推 —— 屏幕上正有一条待批请求，不能被进度信息顶掉。
pub async fn apply(shared: &Shared, tasks: &Tasks, device_idle: bool) {
    if device_idle {
        push(shared, tasks).await;
    }
}

/// 把当前列表下发到设备。排序、过滤、加 tag 都在这边做完，设备只负责画
pub async fn push(shared: &Shared, tasks: &Tasks) {
    let lines = tasks.display_lines();
    let total = lines.len();
    let items: Vec<String> = lines.into_iter().take(DEVICE_SLOTS).collect();
    shared.set_last_tasks(items.clone(), total).await;
    shared.send(HostMsg::Disp(DispOp::Tasks { items, total })).await;
}

/// 设备（重）连时补推最后一次的列表。
/// 否则设备重启后任务页空着，而下一次上报可能要等很久——
/// 待机屏的价值恰恰在于随时抬头看都是对的。
pub async fn repaint(shared: &Shared) {
    let (items, total) = shared.last_tasks().await;
    shared.send(HostMsg::Disp(DispOp::Tasks { items, total })).await;
}

/// api key 的短指纹。用 FNV-1a，不引 sha2——这里只要"同一个 key 映射到同一个桶"，
/// 不需要抗碰撞的密码学强度。空 key（未设密钥的本地开发）归到 default 桶。
pub fn key_fingerprint(api_key: &str) -> String {
    if api_key.is_empty() {
        return "default".to_string();
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in api_key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(client: &str, items: &[(&str, TaskStatus)]) -> TaskReport {
        TaskReport {
            source: Source { client: client.into(), ..Default::default() },
            tasks: items
                .iter()
                .map(|(t, s)| TaskEntry { title: (*t).into(), status: *s })
                .collect(),
        }
    }

    #[test]
    fn 只有进行中的上屏_待办和完成都不显示() {
        let mut t = Tasks::default();
        t.replace(
            "k1",
            report(
                "kiro-cli",
                &[
                    ("待办的", TaskStatus::Todo),
                    ("做完的", TaskStatus::Done),
                    ("正在做", TaskStatus::Doing),
                ],
            ),
        );
        // 待办是计划、计划随时会变；设备上只显示此刻在动的。
        // 后面的 2/3 是进度：只显示一行的话，不带进度就看不出整体到哪了
        assert_eq!(t.display_lines(), vec!["[kiro] 正在做 2/3"]);
    }

    #[test]
    fn 带客户端tag_多客户端下才分得清是谁在做() {
        let mut t = Tasks::default();
        t.replace("k1", report("kiro-cli", &[("编译固件", TaskStatus::Doing)]));
        t.replace("k2", report("claude-code", &[("跑测试", TaskStatus::Doing)]));
        let lines = t.display_lines();
        assert!(lines.contains(&"[kiro] 编译固件".to_string()));
        assert!(lines.contains(&"[cc] 跑测试".to_string()));
    }

    #[test]
    fn 同一个key下的不同客户端互不覆盖() {
        // 这是实测撞到的：同一个人的 kiro 和 cc 用同一个 api key，
        // 只按 key 分桶会让后报的把先报的抹掉，而这两个最需要同时看到
        let mut t = Tasks::default();
        t.replace("k1", report("kiro-cli", &[("编译固件", TaskStatus::Doing)]));
        t.replace("k1", report("claude-code", &[("跑测试", TaskStatus::Doing)]));
        let lines = t.display_lines();
        assert_eq!(lines.len(), 2, "两个客户端都该在：{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("[kiro]")));
        assert!(lines.iter().any(|l| l.starts_with("[cc]")));
        // 各自只有一件事，不该出现 1/1 这种噪音
        assert!(!lines.iter().any(|l| l.contains("1/1")), "{lines:?}");
    }

    #[test]
    fn 同一个客户端再报是替换不是追加() {
        let mut t = Tasks::default();
        t.replace("k1", report("kiro-cli", &[("甲", TaskStatus::Doing)]));
        t.replace("k1", report("kiro-cli", &[("丙", TaskStatus::Doing)]));
        let lines = t.display_lines();
        assert_eq!(lines, vec!["[kiro] 丙"]);
    }

    #[test]
    fn 单个key的条数有上限_写错循环的客户端打不爆内存() {
        let mut t = Tasks::default();
        let many: Vec<(&str, TaskStatus)> =
            (0..500).map(|_| ("刷屏", TaskStatus::Doing)).collect();
        t.replace("k1", report("kiro-cli", &many));
        assert_eq!(t.views()[0].tasks.len(), MAX_PER_KEY);
    }

    #[test]
    fn 租户总量超限时淘汰最久没上报的agent() {
        let mut t = Tasks::default();
        let full: Vec<(&str, TaskStatus)> =
            (0..MAX_PER_KEY).map(|_| ("占满", TaskStatus::Doing)).collect();
        t.replace("k1", report("kiro-cli", &full));
        assert_eq!(t.views().len(), 1);
        // 再来一个 agent，总量就超了，最久没上报的那桶被整个淘汰
        t.replace("k1", report("claude-code", &[("新来的", TaskStatus::Doing)]));
        let views = t.views();
        assert_eq!(views.len(), 1, "应只剩一桶：{views:?}");
        assert_eq!(views[0].client, "cc");
    }

    #[test]
    fn 同一个agent换目录不该分成两个桶() {
        // 实测撞到的：桶 id 原本用 client@目录名，同一个 agent 在 /tmp 和项目目录里
        // 各占一桶，屏幕上出现两行内容相同的任务
        let mut t = Tasks::default();
        let mut a = report("kiro-cli", &[("跑测试", TaskStatus::Doing)]);
        a.source.session = "sess-1".into();
        a.source.cwd = "/tmp".into();
        t.replace("k1", a);
        let mut b = report("kiro-cli", &[("跑测试", TaskStatus::Doing)]);
        b.source.session = "sess-1".into();
        b.source.cwd = "/Users/me/proj".into();
        t.replace("k1", b);
        assert_eq!(t.views().len(), 1, "同一 session 必须是同一桶：{:?}", t.views());
    }

    #[test]
    fn 指纹不泄露明文密钥() {
        let fp = key_fingerprint("super-secret-key");
        assert!(!fp.contains("secret"));
        assert_eq!(fp.len(), 16);
        // 同一个 key 必须稳定映射到同一个桶
        assert_eq!(fp, key_fingerprint("super-secret-key"));
        assert_ne!(fp, key_fingerprint("another-key"));
        assert_eq!(key_fingerprint(""), "default");
    }
}
