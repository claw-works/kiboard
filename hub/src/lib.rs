//! kiboard-hub 的库入口。
//!
//! 之所以拆出 lib：服务端（kiboard-hub）和客户端闸门（kiboard-ask）是两个可执行文件，
//! 但它们必须共用同一份消息体定义（wire）与决策枚举（approval::Decision）。
//! Rust 的多个 bin 之间不能互相 use，只有通过 lib 才能共享——
//! 改了字段两边一起编译报错，比两处手写 JSON 可靠。
pub mod agentstate;
pub mod api;
pub mod approval;
pub mod audit;
pub mod auth;
pub mod config;
pub mod device;
pub mod keymap;
pub mod protocol;
pub mod rules;
pub mod serial;
pub mod state;
pub mod tasks;
pub mod wire;

/// 构建期由 build.rs 从 git 注入。存在的理由是"一眼确认远端跑的是不是刚推的那版"——
/// 手工维护的版本号迟早漏改一次，而漏改那次恰好是最需要它的时候。
pub mod version {
    /// git describe，带 +dirty 标记
    pub const VERSION: &str = env!("KIBOARD_VERSION");
    /// 短 sha，8 位
    pub const GIT_SHA: &str = env!("KIBOARD_GIT_SHA");
    /// 提交时间（不是编译时间：同一份代码必须给出同一个版本号）
    pub const GIT_DATE: &str = env!("KIBOARD_GIT_DATE");

    /// 一行摘要，日志和屏幕上用
    pub fn line() -> String {
        format!("{VERSION} ({GIT_DATE})")
    }
}
