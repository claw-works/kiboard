//! kiboard-hub：设备（串口/WiFi）<-> 客户端（WS/HTTP）的中转 + 审批状态机

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tracing::info;

use kiboard_hub::approval::Approvals;
use kiboard_hub::config::Config;
use kiboard_hub::protocol::{HostMsg, HubEvent};
use kiboard_hub::state::Shared;
use kiboard_hub::{api, audit, rules, serial, version};

const HEARTBEAT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kiboard_hub=debug".into()),
        )
        .init();

    // 第一行就打版本：远端日志一翻到头就知道跑的是哪一版
    info!("kiboard-hub {} sha={}", version::line(), version::GIT_SHA);

    let cfg = Config::from_env();
    info!(
        "config: listen={} serial={} approve_timeout={}s auto_accept_ttl={}s high_hold={}ms",
        cfg.listen,
        if cfg.serial_enabled() { cfg.serial_port.as_str() } else { "off" },
        cfg.approve_timeout.as_secs(),
        cfg.auto_accept_ttl.as_secs(),
        cfg.high_hold.as_millis()
    );

    // 串口出口队列；无线出口在设备 WS 连上时动态注册
    let (serial_tx, serial_rx) = mpsc::channel::<HostMsg>(64);
    let (evt_tx, _) = broadcast::channel::<HubEvent>(256);

    // 规则表：找不到不是错误，一切按 normal 走（问一句），不会变成静默放行
    let rules_path = std::env::var("KIBOARD_RULES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("rules.toml"));
    let rules = Arc::new(rules::Rules::load(&rules_path));
    let audit = audit::Audit::from_env();
    match audit.path() {
        Some(p) => info!("审计日志: {}", p.display()),
        None => info!("审计日志: 已关闭"),
    }

    let shared = Shared::new(serial_tx);
    let approvals =
        Approvals::new(shared.clone(), evt_tx.clone(), cfg.auto_accept_ttl, cfg.high_hold);

    if cfg.serial_enabled() {
        tokio::spawn(serial::run(
            cfg.serial_port.clone(),
            cfg.baud,
            shared.clone(),
            serial_rx,
            evt_tx.clone(),
            approvals.clone(),
        ));
    } else {
        info!("serial link disabled (KIBOARD_SERIAL=off)");
        // 没有串口链路时把接收端丢掉，避免 send 时队列积压
        drop(serial_rx);
    }
    tokio::spawn(heartbeat(shared.clone()));
    // 刷新「自动接受」角标的剩余时间，并在到期时清掉
    tokio::spawn(approvals.clone().badge_ticker());

    if cfg.exposed_without_api_key() {
        tracing::warn!(
            "!! HTTP 接口没有认证，而监听地址是 {} —— 局域网（若做了端口转发则包括公网）\
             内任何人都能调用 /decide 直接批准请求。设置 KIBOARD_API_KEY 后再对外暴露。",
            cfg.listen
        );
    }

    let app = api::router(api::AppState {
        shared,
        events: evt_tx,
        approvals,
        token: cfg.token.clone(),
        approve_timeout: cfg.approve_timeout,
        rules,
        audit,
        agent_state: Arc::new(tokio::sync::Mutex::new(None)),
        agent_tasks: Arc::new(tokio::sync::Mutex::new(Default::default())),
        api_key: cfg.api_key.clone(),
    }, cfg.api_key.clone());
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    let port = cfg.listen.rsplit(':').next().unwrap_or("26041");
    info!("hub listening on http://{}", cfg.listen);
    info!("device endpoint: ws://<host>:{port}/device?token=<KIBOARD_TOKEN>");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn heartbeat(shared: Shared) {
    let mut tick = tokio::time::interval(HEARTBEAT);
    loop {
        tick.tick().await;
        shared.send(HostMsg::Ping).await;
    }
}
