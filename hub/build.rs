//! 构建期把 git 版本编进二进制。
//!
//! 用户建议"每次 push 时 hardcode 版本号"，改成自动注入：手工维护的版本号
//! 迟早会漏改一次，而漏改的那次恰恰是最需要它的时候——你以为部署了新版，
//! 实际跑的是旧的，然后开始怀疑代码。让构建期从 git 取，就没有"忘记"这回事。
//!
//! 拿不到 git（比如从 tarball 部署）时退化成 "unknown"，不让构建失败。

use std::process::Command;

fn main() {
    // 只在 HEAD 变动时重跑，否则每次编译都要 fork 几个 git 进程
    for p in ["../.git/HEAD", "../.git/refs/heads/main"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }

    let describe = git(&["describe", "--always", "--dirty=+dirty", "--tags"])
        .unwrap_or_else(|| "unknown".to_string());
    let sha = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // 提交时间而不是编译时间：编译时间会让同一份代码每次构建都产生不同版本号，
    // 那就没法用版本号判断"跑的是不是同一份代码"了
    let date = git(&["log", "-1", "--format=%cd", "--date=format:%Y-%m-%d %H:%M"])
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=KIBOARD_VERSION={describe}");
    println!("cargo:rustc-env=KIBOARD_GIT_SHA={sha}");
    println!("cargo:rustc-env=KIBOARD_GIT_DATE={date}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
