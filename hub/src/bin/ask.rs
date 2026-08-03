//! kiboard-ask：装在跑 agent 的机器上的审批闸门
//!
//! 各客户端的钩子都是本机子进程，没有 webhook 形态，所以必须有这么一个东西：
//! 读客户端喂进 stdin 的 JSON -> 转成统一消息体 -> 问 hub -> 把结论变成退出码。
//!
//! 用法（见 clients/ 下各客户端的 README）：
//!   kiboard-ask --client kiro-cli      # 按 Kiro CLI 的 preToolUse 载荷解析
//!   kiboard-ask --client claude-code   # 按 Claude Code 的 PreToolUse 载荷解析
//!   kiboard-ask --client raw           # stdin 已经是统一消息体，给自定义适配器用
//!
//! 退出码（Kiro CLI / Claude Code 的 PreToolUse 都是这个语义）：
//!   0  放行
//!   2  阻止，理由写到 stderr（会进 agent 上下文，所以要写得能引导它换方案）
//!
//! 失败一律 fail-closed（exit 2）。文档明确「非 0/2 退出码 = 警告后照样执行」，
//! 而超时语义未写明，所以默认倾向是放行——闸门若在 hub 挂掉时自动放行，
//! 就等于没有闸门。要放宽只能显式设 KIBOARD_ON_FAILURE=open。
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use kiboard_hub::approval::Decision;
use kiboard_hub::rules::{Rules, Verdict};
use kiboard_hub::wire::{ApproveRequest, ApproveResponse, Intent, Source, ToolCall};
use serde_json::Value;

const EXIT_ALLOW: i32 = 0;
const EXIT_BLOCK: i32 = 2;

/// 本地规则缓存多久刷一次
const DEFAULT_RULES_TTL_S: u64 = 3600;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let client = arg_value(&args, "--client").unwrap_or_else(|| "raw".to_string());
    // 版本：客户端和 hub 可能不同步部署，出问题时第一件事就是核版本
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "kiboard-ask {} sha={} ({})",
            kiboard_hub::version::VERSION,
            kiboard_hub::version::GIT_SHA,
            kiboard_hub::version::GIT_DATE
        );
        return EXIT_ALLOW;
    }

    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{}", include_str!("ask_help.txt"));
        return EXIT_ALLOW;
    }

    let cfg = load_config();

    // 状态上报：和审批完全不同的路径——不阻塞、短超时、永远 exit 0。
    // 一个"看看 agent 在干什么"的功能绝不能变成新的失败模式：
    // hub 挂了、网络断了，都不该让 agent 卡住或让工具被拦。
    if let Some(state) = arg_value(&args, "--state") {
        report_state(&state, &client, &cfg);
        return EXIT_ALLOW;
    }

    // 任务列表上报。和 --state 同一类：fire-and-forget、短超时、永远 exit 0。
    // 观测功能不能变成失败模式。
    if args.iter().any(|a| a == "--tasks") {
        let session = arg_value(&args, "--session").unwrap_or_default();
        report_tasks(&client, &session, &cfg);
        return EXIT_ALLOW;
    }

    if args.iter().any(|a| a == "--refresh-rules") {
        let Some(url) = cfg.get("KIBOARD_URL") else {
            eprintln!("kiboard: 配置里没有 KIBOARD_URL");
            return EXIT_BLOCK;
        };
        let key = cfg.get("KIBOARD_API_KEY").cloned().unwrap_or_default();
        match fetch_rules(url, &key) {
            Ok(toml) => {
                let Some(path) = cache_path() else { return EXIT_BLOCK };
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(&path, &toml) {
                    Ok(()) => {
                        let n = Rules::from_toml(&toml).len();
                        println!("已刷新 {} 条规则 -> {}", n, path.display());
                        return EXIT_ALLOW;
                    }
                    Err(e) => {
                        eprintln!("写缓存失败：{e}");
                        return EXIT_BLOCK;
                    }
                }
            }
            Err(e) => {
                eprintln!("拉规则失败：{e}");
                return EXIT_BLOCK;
            }
        }
    }
    let fail_open = cfg.get("KIBOARD_ON_FAILURE").map(String::as_str) == Some("open");
    let fail = |msg: &str| -> i32 {
        if fail_open {
            eprintln!("kiboard: {msg}（KIBOARD_ON_FAILURE=open，放行）");
            EXIT_ALLOW
        } else {
            eprintln!("kiboard: {msg}. 审批链路不可用，已按 fail-closed 阻止此操作。");
            EXIT_BLOCK
        }
    };

    // 逃逸阀：显式的文件开关，不要用「连不上就放行」这种隐式退化
    if let Some(home) = std::env::var_os("HOME") {
        let bypass = std::path::Path::new(&home).join(".kiboard/bypass");
        if bypass.exists() {
            eprintln!("kiboard: ~/.kiboard/bypass 存在，跳过审批");
            return EXIT_ALLOW;
        }
    }

    let Some(url) = cfg.get("KIBOARD_URL") else {
        return fail("~/.kiboard/config 里没有 KIBOARD_URL");
    };
    let api_key = cfg.get("KIBOARD_API_KEY").cloned().unwrap_or_default();

    let mut stdin_buf = String::new();
    if std::io::stdin().read_to_string(&mut stdin_buf).is_err() {
        return fail("读 stdin 失败");
    }
    let payload: Value = match serde_json::from_str(stdin_buf.trim()) {
        Ok(v) => v,
        Err(e) => return fail(&format!("stdin 不是合法 JSON：{e}")),
    };

    let req = match build_request(&client, &payload, &cfg) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };

    // 调试用：只打印统一消息体就走，不联网、不判规则。
    // 写新客户端适配器时拿它对字段映射；绝不该出现在真实 hook 里
    // （Claude Code 的 stdout 是决策通道，往里写东西会被当成裁决）。
    if args.iter().any(|a| a == "--dump-request") {
        match serde_json::to_string_pretty(&req) {
            Ok(j) => println!("{j}"),
            Err(e) => return fail(&format!("序列化失败：{e}")),
        }
        return EXIT_ALLOW;
    }

    // 本地 allow 短路：只读命令不必往公网跑一趟，也不必等 hub 在线。
    //
    // 规则原本全在 hub，意味着每次工具调用都要一个 RTT，而 hub 一挂、网络一抖，
    // fail-closed 就把所有命令拦下——包括 git status 这种根本不需要问的。
    // 现在客户端缓存规则表（内容由 hub 下发，仍是中心管理），本地只做一件事：
    // 判断"这条要不要问人"。判 allow 直接放行，其余照常联网让 hub 定 normal/high。
    //
    // 这不削弱安全性：客户端本来就不是信任边界——能改缓存文件的人也能直接删掉 hook。
    // 闸门防的是 agent 判断失误，不是防有本机写权限的攻击者。
    if let Some(rules) = load_rules(url, &api_key, &cfg) {
        let corpus = {
            let t = req.tool.input_text();
            if t.is_empty() { req.display_title() } else { t }
        };
        let (verdict, rule) = rules.classify(&req.source.client, &req.tool.name, &corpus);
        if verdict == Verdict::Allow {
            eprintln!("kiboard: 本地规则放行（{rule}），未联网");
            return allow_exit(&client, &cfg, &format!("kiboard rule: {rule}"));
        }
    }

    // 超时留足余量：让 hub 先超时返回 decision=timeout，把裁决权留在自己手里。
    // 绝不能让上游 hook 先超时——那条路的行为文档没写明。
    let hub_timeout = req.timeout_seconds().unwrap_or(120);
    let http_timeout = Duration::from_secs(hub_timeout + 30);

    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => return fail(&format!("请求体序列化失败：{e}")),
    };

    let resp = match post_json(url, "/approve", &api_key, &body, http_timeout) {
        Ok(r) => r,
        Err(e) => return fail(&format!("调 hub 失败：{e}")),
    };

    let parsed: ApproveResponse = match serde_json::from_slice(&resp.body) {
        Ok(p) => p,
        Err(e) => {
            return fail(&format!(
                "hub 响应无法解析（HTTP {}）：{e}；原文：{}",
                resp.status,
                String::from_utf8_lossy(&resp.body).chars().take(200).collect::<String>()
            ));
        }
    };

    // 设备离线时 hub 返回 503 + cancelled。这不是"批准"，按失败策略处置
    if !parsed.approved && parsed.decision == Decision::Cancelled {
        return fail(&format!("hub 说：{}", parsed.reason));
    }

    if parsed.approved {
        // stdout 在 Kiro 的 preToolUse 下不会展示也不进上下文，信息写 stderr 更有用；
        // Claude Code 例外——它的 stdout 是决策通道，见 allow_exit
        eprintln!("kiboard: {} ({})", parsed.reason, decision_name(parsed.decision));
        allow_exit(&client, &cfg, &format!("kiboard: {}", parsed.reason))
    } else {
        eprintln!("{}", parsed.reason);
        EXIT_BLOCK
    }
}

/// 放行时的退出处理。除 Claude Code 外都是"静默 exit 0"。
///
/// Claude Code 的 PreToolUse 把 stdout 当决策通道：静默 exit 0 只是"不反对"，
/// 它自己的权限系统照旧会再弹一次确认——那样实体键盘就白按了。要免掉第二次确认
/// 必须显式回 `hookSpecificOutput.permissionDecision = "allow"`。
///
/// 但这等于**用 kiboard 的 rules.toml 顶替 Claude Code 自己的权限系统**，
/// 是个有代价的决定，所以默认不这么做：
///   KIBOARD_CC_DECISION=passthrough（默认）静默 exit 0，CC 的权限提示照旧。
///                                    代价是同一条命令问两遍。
///   KIBOARD_CC_DECISION=explicit     显式回 allow，CC 不再问。
///                                    此时 rules.toml 就是唯一的放行依据。
/// 默认选 passthrough 是因为"悄悄关掉宿主的安全机制"不该是默认行为。
///
/// 三个坑（都是 claude-code 仓库里的已知 issue）：
///   1. permissionDecision 必须包在 hookSpecificOutput 里，扁平写法被静默丢弃（#48760）
///   2. 不用 "ask" 档：多个版本上它不被强制执行（#79356 / #81041），
///      在 bypassPermissions 下还会被静默批准（#77212）。反正人已经在键盘上答过了
///   3. 拒绝走 exit 2 而不是 permissionDecision="deny"：exit 2 被证实可靠，
///      且 stderr 会进模型上下文，能引导它换方案
fn allow_exit(client: &str, cfg: &HashMap<String, String>, reason: &str) -> i32 {
    let explicit = cfg.get("KIBOARD_CC_DECISION").map(String::as_str) == Some("explicit");
    if client == "claude-code" && explicit {
        println!("{}", cc_allow_json(reason));
    }
    EXIT_ALLOW
}

/// Claude Code 的放行裁决。**必须**包在 hookSpecificOutput 里，
/// 扁平的 {"permissionDecision":"allow"} 会被静默丢弃（claude-code#48760）
fn cc_allow_json(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Accept => "accept",
        Decision::AutoAccept => "auto_accept",
        Decision::RuleAllow => "rule_allow",
        Decision::Reject => "reject",
        Decision::Timeout => "timeout",
        Decision::Cancelled => "cancelled",
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// 读 ~/.kiboard/config（KEY=VALUE 每行一条），环境变量优先
fn load_config() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".kiboard/config");
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    for key in [
        "KIBOARD_URL",
        "KIBOARD_API_KEY",
        "KIBOARD_ON_FAILURE",
        "KIBOARD_TIMEOUT_S",
        "KIBOARD_CC_DECISION",
    ] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty() {
                map.insert(key.to_string(), v);
            }
    }
    map
}

/// 把各客户端的钩子载荷转成统一消息体。
///
/// 新客户端有两条路：要么在这里加一个分支（载荷格式固定、值得内建），
/// 要么用 --client raw 自己在外面拼好统一消息体。
fn build_request(
    client: &str,
    payload: &Value,
    cfg: &HashMap<String, String>,
) -> Result<ApproveRequest, String> {
    let timeout_s = cfg.get("KIBOARD_TIMEOUT_S").and_then(|v| v.parse::<u64>().ok());

    let mut req = match client {
        // stdin 已经是统一消息体
        "raw" => serde_json::from_value::<ApproveRequest>(payload.clone())
            .map_err(|e| format!("stdin 不是统一消息体：{e}"))?,

        // Kiro CLI 2.16 preToolUse 实测载荷：
        //   {hook_event_name, cwd, tool_name, tool_input:{command, summary}}
        // 注意 2.16 实际【没有】session_id 字段（文档里有，实测没有），所以 session 会是空。
        //
        // tool_input.summary 是模型自己写的意图说明。它只能当 detail，绝不能当标题——
        // 屏幕上必须先显示真正要执行的命令，否则一个措辞良善、内容危险的 summary
        // 会让人在错误的前提下批准。
        "kiro-cli" => ApproveRequest {
            source: Source {
                client: "kiro-cli".into(),
                version: std::env::var("KIBOARD_CLIENT_VERSION").unwrap_or_default(),
                agent: std::env::var("KIRO_AGENT").unwrap_or_default(),
                session: str_field(payload, "session_id"),
                cwd: str_field(payload, "cwd"),
                ..Default::default()
            },
            tool: ToolCall {
                name: str_field(payload, "tool_name"),
                input: payload.get("tool_input").cloned().unwrap_or(Value::Null),
            },
            intent: Intent {
                title: String::new(),
                detail: payload
                    .get("tool_input")
                    .and_then(|t| t.get("summary"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            ..Default::default()
        },

        // Claude Code PreToolUse: {session_id, cwd, tool_name, tool_input, ...}
        // 字段名与 Kiro 相近，但它的决策通道更强（stdout 可表达 allow/deny/ask）
        "claude-code" => ApproveRequest {
            source: Source {
                client: "claude-code".into(),
                session: str_field(payload, "session_id"),
                cwd: str_field(payload, "cwd"),
                ..Default::default()
            },
            tool: ToolCall {
                name: str_field(payload, "tool_name"),
                input: payload.get("tool_input").cloned().unwrap_or(Value::Null),
            },
            ..Default::default()
        },

        other => return Err(format!("不认识的 --client {other}（可用：kiro-cli, claude-code, raw）")),
    };

    // 补齐环境信息：hub 用它上屏和写审计
    if req.source.host.is_empty() {
        req.source.host = hostname();
    }
    if req.source.user.is_empty() {
        req.source.user = std::env::var("USER").unwrap_or_default();
    }
    if req.source.cwd.is_empty() {
        req.source.cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
    }
    if req.policy.timeout_s.is_none() {
        req.policy.timeout_s = timeout_s;
    }
    Ok(req)
}

/// 上报任务列表。stdin 收两种格式，都常用：
///   - JSON 数组：[{"title":"...","status":"doing"}, ...]
///   - 纯文本每行一条，行首 "- [x] " / "- [ ] " 前缀会被识别成完成/待办
///     （直接把 markdown 待办清单管道进来就行，不用先转 JSON）
///
/// 和 --state 一样 fire-and-forget、永远 exit 0。
fn report_tasks(client: &str, session: &str, cfg: &HashMap<String, String>) {
    let Some(url) = cfg.get("KIBOARD_URL") else { return };
    let api_key = cfg.get("KIBOARD_API_KEY").cloned().unwrap_or_default();

    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);

    let tasks: Vec<Value> = match serde_json::from_str::<Value>(buf.trim()) {
        // 已经是 JSON 数组，原样用
        Ok(Value::Array(a)) => a,
        _ => parse_task_lines(&buf),
    };

    let body = serde_json::json!({
        "source": {
            "client": client,
            // session 决定 hub 那边怎么分桶：它才是"这一次 agent 运行"的准确标识。
            // 不传的话同一个 agent 换个目录会被当成两个 agent，屏幕上出现重复行
            "session": session,
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            "host": hostname(),
            "user": std::env::var("USER").unwrap_or_default(),
        },
        "tasks": tasks,
    });
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    if let Err(e) = post_json(url, "/tasks", &api_key, &bytes, Duration::from_secs(3)) {
        eprintln!("kiboard: 任务上报失败（{e}），忽略");
    }
}

/// 把纯文本待办清单转成任务项。支持直接粘 markdown 清单。
fn parse_task_lines(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // 去掉列表符号与勾选框，识别状态
        let (status, rest) = if let Some(r) = strip_prefixes(line, &["- [x]", "- [X]", "* [x]", "[x]"]) {
            ("done", r)
        } else if let Some(r) = strip_prefixes(line, &["- [ ]", "* [ ]", "[ ]"]) {
            ("todo", r)
        } else if let Some(r) = strip_prefixes(line, &["- ", "* ", "+ "]) {
            ("doing", r)
        } else {
            ("doing", line)
        };
        let title = rest.trim();
        if title.is_empty() {
            continue;
        }
        out.push(serde_json::json!({"title": title, "status": status}));
    }
    out
}

fn strip_prefixes<'a>(line: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| line.strip_prefix(p))
}

/// 上报 agent 状态。失败只在 stderr 留一句，绝不影响退出码。
fn report_state(state: &str, client: &str, cfg: &HashMap<String, String>) {
    let Some(url) = cfg.get("KIBOARD_URL") else { return };
    let api_key = cfg.get("KIBOARD_API_KEY").cloned().unwrap_or_default();

    // hook 的载荷里能捞到 cwd 和一些上下文；读不到就算了，别为此失败
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    let payload: Value = serde_json::from_str(stdin_buf.trim()).unwrap_or(Value::Null);

    // 从 hook 载荷里取一句有用的补充：工具名 / prompt 开头 / 出错信息
    let detail = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload.get("prompt").and_then(Value::as_str).map(|p| {
                let short: String = p.chars().take(24).collect();
                short
            })
        })
        .unwrap_or_default();

    let body = serde_json::json!({
        "source": {
            "client": client,
            "cwd": str_or_cwd(&payload),
            "host": hostname(),
            "user": std::env::var("USER").unwrap_or_default(),
        },
        "state": state,
        "detail": detail,
    });
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    // 短超时：宁可漏一次上报，也不能让 agent 等
    if let Err(e) = post_json(url, "/state", &api_key, &bytes, Duration::from_secs(3)) {
        eprintln!("kiboard: 状态上报失败（{e}），忽略");
    }
}

fn str_or_cwd(payload: &Value) -> String {
    let from_payload = payload.get("cwd").and_then(Value::as_str).unwrap_or_default();
    if !from_payload.is_empty() {
        return from_payload.to_string();
    }
    std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
}

/// 缓存路径 ~/.kiboard/rules.cache.toml
fn cache_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::Path::new(&home).join(".kiboard/rules.cache.toml"))
}

/// 拿到可用的规则表：缓存新鲜就直接用，过期或没有就去 hub 拉一次。
///
/// 拉不到而有旧缓存 -> 用旧的（降级但能用）。
/// 既没缓存又拉不到 -> 返回 None，后面照常走联网审批（那条路会 fail-closed）。
/// 这个退化方向是对的：不知道规则时不能自己决定放行。
fn load_rules(url: &str, api_key: &str, cfg: &HashMap<String, String>) -> Option<Rules> {
    let path = cache_path()?;
    let ttl = cfg
        .get("KIBOARD_RULES_TTL_S")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RULES_TTL_S);

    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() < ttl).unwrap_or(false))
        .unwrap_or(false);

    if !fresh {
        match fetch_rules(url, api_key) {
            Ok(toml) => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(&path, &toml) {
                    eprintln!("kiboard: 规则缓存写不进去（{e}），这次用内存里的");
                    return Some(Rules::from_toml(&toml));
                }
            }
            Err(e) => eprintln!("kiboard: 刷新规则失败（{e}），改用本地缓存"),
        }
    }

    let text = std::fs::read_to_string(&path).ok()?;
    Some(Rules::from_toml(&text))
}

fn fetch_rules(url: &str, api_key: &str) -> Result<String, String> {
    let resp = get(url, "/rules", api_key, Duration::from_secs(15))?;
    if resp.status != 200 {
        return Err(format!("GET /rules 返回 {}", resp.status));
    }
    let v: Value = serde_json::from_slice(&resp.body).map_err(|e| e.to_string())?;
    v.get("toml")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "响应里没有 toml 字段".to_string())
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

/// 极简 HTTP/1.1 POST。
///
/// 故意不引 reqwest：这个二进制要拷到别人机器上，越小越少依赖越好，
/// 而它只需要向一个已知地址发一个 POST。用 Connection: close 读到 EOF，
/// 省掉 chunked 与 Content-Length 的解析。
/// 只支持 http://；hub 若要走 https，前面挂反代或改用 clients/ 下的 curl 兜底脚本。
fn get(
    base_url: &str,
    path: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    request("GET", base_url, path, api_key, None, timeout)
}

fn post_json(
    base_url: &str,
    path: &str,
    api_key: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<HttpResponse, String> {
    request("POST", base_url, path, api_key, Some(body), timeout)
}

fn request(
    method: &str,
    base_url: &str,
    path: &str,
    api_key: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let rest = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("只支持 http:// 的 KIBOARD_URL，收到 {base_url}"))?;
    let (hostport, _) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "端口号不是数字".to_string())?),
        None => (hostport, 80u16),
    };

    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析 {host} 失败：{e}"))?
        .next()
        .ok_or_else(|| format!("{host} 没有解析到地址"))?;

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("连不上 {addr}：{e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(15))).ok();

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nX-Api-Key: {api_key}\r\n\
         Connection: close\r\n"
    );
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|e| format!("发送请求头失败：{e}"))?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| format!("发送请求体失败：{e}"))?;
    }
    stream.flush().ok();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("读响应失败（可能超时）：{e}"))?;

    let sep = find_subslice(&raw, b"\r\n\r\n")
        .ok_or_else(|| "响应里找不到头体分隔".to_string())?;
    let head_txt = String::from_utf8_lossy(&raw[..sep]).to_string();
    let status = head_txt
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("解析不出状态码：{head_txt}"))?;

    Ok(HttpResponse { status, body: raw[sep + 4..].to_vec() })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_载荷转统一消息体() {
        let payload = serde_json::json!({
            "hook_event_name": "preToolUse",
            "cwd": "/Users/x/projects/kiboard",
            "session_id": "abc",
            "tool_name": "execute_bash",
            "tool_input": {"command": "rm -rf build"}
        });
        let req = build_request("kiro-cli", &payload, &HashMap::new()).unwrap();
        assert_eq!(req.source.client, "kiro-cli");
        assert_eq!(req.source.session, "abc");
        assert_eq!(req.source.label(), "kiro@kiboard");
        assert_eq!(req.tool.name, "execute_bash");
        assert_eq!(req.tool.input_text(), r#"{"command":"rm -rf build"}"#);
        assert!(!req.source.host.is_empty(), "host 应自动补齐");
    }

    #[test]
    fn 实测载荷_summary进detail且session为空() {
        // 这是 kiro-cli 2.16 实测抓到的真实载荷：有 summary，没有 session_id
        let payload = serde_json::json!({
            "hook_event_name": "preToolUse",
            "cwd": "/private/tmp/kbprobe",
            "tool_name": "execute_bash",
            "tool_input": {"command": "echo hi", "summary": "按用户要求执行 echo"}
        });
        let req = build_request("kiro-cli", &payload, &HashMap::new()).unwrap();
        assert_eq!(req.source.session, "", "2.16 实测没有 session_id");
        assert_eq!(req.display_detail(), "按用户要求执行 echo");
        // 标题必须是真正要执行的命令，不能被模型写的 summary 顶替
        assert!(req.display_title().contains("echo hi"), "{}", req.display_title());
    }

    #[test]
    fn raw_模式直接吃统一消息体() {
        let payload = serde_json::json!({
            "source": {"client": "custom", "cwd": "/a/b"},
            "tool": {"name": "x", "input": {"k": "v"}}
        });
        let req = build_request("raw", &payload, &HashMap::new()).unwrap();
        assert_eq!(req.source.client, "custom");
        assert_eq!(req.source.label(), "custom@b");
    }

    #[test]
    fn 不认识的客户端要报错而不是猜() {
        let err = build_request("emacs", &serde_json::json!({}), &HashMap::new()).unwrap_err();
        assert!(err.contains("不认识"), "{err}");
    }

    #[test]
    fn 配置里的超时会填进策略() {
        let mut cfg = HashMap::new();
        cfg.insert("KIBOARD_TIMEOUT_S".to_string(), "90".to_string());
        let req = build_request("kiro-cli", &serde_json::json!({}), &cfg).unwrap();
        assert_eq!(req.timeout_seconds(), Some(90));
    }

    #[test]
    fn cc载荷转统一消息体() {
        let payload = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/tmp/proj",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "rm -rf /", "description": "清理" },
        });
        let req = build_request("claude-code", &payload, &HashMap::new()).unwrap();
        assert_eq!(req.source.client, "claude-code");
        assert_eq!(req.source.session, "abc123");
        assert_eq!(req.source.cwd, "/tmp/proj");
        assert_eq!(req.tool.name, "Bash");
        // tool_input 原样透传，不解析——规则匹配作用在它的 JSON 文本上
        assert!(req.tool.input_text().contains("rm -rf /"));
    }

    #[test]
    fn cc放行裁决必须包在钩子专属输出里() {
        // 扁平写法会被 Claude Code 静默丢弃，这个测试就是钉死这个形状
        let v: Value = serde_json::from_str(&cc_allow_json("kiboard: 人按了 1")).unwrap();
        let h = v.get("hookSpecificOutput").expect("必须有 hookSpecificOutput 包一层");
        assert_eq!(h["hookEventName"], "PreToolUse");
        assert_eq!(h["permissionDecision"], "allow");
        assert_eq!(h["permissionDecisionReason"], "kiboard: 人按了 1");
        // 绝不能出现在顶层
        assert!(v.get("permissionDecision").is_none());
        // 绝不用 ask 档（多版本上不被强制执行、bypassPermissions 下被静默批准）
        assert_ne!(h["permissionDecision"], "ask");
    }

    #[test]
    fn cc决策默认不输出_不顶替宿主权限系统() {
        let cfg = HashMap::new();
        assert_eq!(allow_exit("claude-code", &cfg, "x"), EXIT_ALLOW);
        let mut explicit = HashMap::new();
        explicit.insert("KIBOARD_CC_DECISION".to_string(), "explicit".to_string());
        assert_eq!(allow_exit("claude-code", &explicit, "x"), EXIT_ALLOW);
        // kiro-cli 无论怎么设都不该走 CC 那条输出
        assert_eq!(allow_exit("kiro-cli", &explicit, "x"), EXIT_ALLOW);
    }

    #[test]
    fn 只接受http协议的地址() {
        let err = post_json("https://x.example", "/approve", "k", b"{}", Duration::from_secs(1))
            .unwrap_err();
        assert!(err.contains("只支持 http://"), "{err}");
    }
}
