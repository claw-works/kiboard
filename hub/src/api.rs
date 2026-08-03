//! HTTP + WebSocket 接口
//!
//! 给 agent 用的：
//!   POST /approve   提交审批请求，阻塞直到用户按键或超时（主入口）
//!   GET  /pending   当前待批请求
//!   POST /decide    不按实体键也能裁决（无硬件时测试用）
//!   POST /cancel    取消当前及排队中的全部请求
//!   GET/POST /auto  查询 / 清除「全部接受」状态
//!
//! 给调试用的：/status /msg /led /keymap
//! 链路：/device 设备接入（需 token）、/ws 客户端订阅事件
use std::collections::HashMap;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::agentstate::{self, Current, StateReport};
use crate::approval::{Approvals, Decision, Risk};
use crate::audit::{self, Audit};
use crate::device;
use crate::keymap;
use crate::protocol::{DispOp, HostMsg, HubEvent, LedMode};
use crate::rules::{Rules, Verdict};
use crate::state::{Shared, Transport};
use crate::tasks;
use crate::wire::{ApproveRequest, ApproveResponse};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    pub shared: Shared,
    pub events: broadcast::Sender<HubEvent>,
    pub approvals: Approvals,
    pub token: String,
    pub approve_timeout: Duration,
    pub rules: Arc<Rules>,
    pub audit: Audit,
    /// agent 最近上报的状态。只有一份——设备只有一块屏，同时显示多个 agent 的状态没意义
    pub agent_state: Arc<Mutex<Option<Current>>>,
    /// 任务列表，按 api key 分桶。为将来一个 hub 服务多个人留的接缝
    pub agent_tasks: Arc<Mutex<tasks::Tasks>>,
    /// 用来给任务分桶的 api key。今天只有一个，所以所有上报归同一桶
    pub api_key: Option<String>,
}

pub fn router(state: AppState, api_key: Option<String>) -> Router {
    if api_key.is_some() {
        info!("HTTP 接口需要 X-Api-Key（/device 用自己的 token，/health 放行）");
    }
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/keymap", get(keymap_view))
        .route("/rules", get(rules_view))
        .route("/state", get(state_get).post(state_post))
        .route("/tasks", get(tasks_get).post(tasks_post))
        .route("/audit", get(audit_view))
        .route("/msg", post(msg))
        .route("/disp", post(disp))
        .route("/led", post(led))
        .route("/approve", post(approve))
        .route("/pending", get(pending))
        .route("/decide", post(decide))
        .route("/cancel", post(cancel))
        .route("/auto", get(auto_get).post(auto_clear))
        .route("/device", get(device_upgrade))
        .route("/ws", get(client_upgrade))
        .layer(axum::middleware::from_fn_with_state(api_key, crate::auth::require_api_key))
        .with_state(state)
}

/// 不需要密钥，给部署侧探活用。**刻意带上版本**：
/// 部署完 curl 一下就知道跑的是不是刚推的那版，不用猜
async fn health() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": "kiboard-hub",
        "version": crate::version::VERSION,
        "sha": crate::version::GIT_SHA,
        "commit_date": crate::version::GIT_DATE,
    }))
}

async fn status(State(st): State<AppState>) -> impl IntoResponse {
    let (pending, depth) = st.approvals.pending().await;
    Json(json!({
        "device": st.shared.status().await,
        "pending": pending,
        "queued": depth,
        "auto": st.approvals.auto_view().await,
    }))
}

async fn keymap_view() -> impl IntoResponse {
    let keys: Vec<_> = (0u8..16)
        .map(|id| {
            json!({
                "id": id,
                "label": keymap::label(id),
                "row": id / 4 + 1,
                "col": id % 4 + 1,
                "action": keymap::action(id),
            })
        })
        .collect();
    Json(json!({"keys": keys}))
}

/// 把规则表原文下发给客户端做本地缓存。
///
/// 为什么要下发：规则判断原本全在 hub，意味着连 git status 都要先跑一趟公网；
/// hub 一挂、网络一抖，fail-closed 会把所有命令拦下——包括根本不需要问的。
/// 客户端缓存规则后可以本地判 allow 直接放行，只有真正需要人裁决的才联网。
/// 中心管理仍然保留：规则只在 hub 上维护，客户端按 etag 拉。
async fn rules_view(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "etag": st.rules.etag(),
        "toml": st.rules.source(),
    }))
}

#[derive(Deserialize)]
struct MsgReq {
    text: String,
    #[serde(default = "default_color")]
    color: String,
}

fn default_color() -> String {
    "white".into()
}

async fn msg(State(st): State<AppState>, Json(req): Json<MsgReq>) -> impl IntoResponse {
    info!("msg: {}", req.text);
    st.shared.send(HostMsg::msg(req.text, req.color)).await;
    Json(json!({"ok": true}))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    limit: Option<usize>,
    /// 只看某种裁决：accept / reject / auto_accept / ruleallow / timeout / cancelled
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    client: Option<String>,
}

/// 审计查询。
///
/// 一个审批设备的价值有一半在事后：出事之后要能回答「这条命令当时是谁批的、
/// 什么时候、按的哪个键」。原来只能手工 grep JSONL，现在给个接口。
///
/// 默认返回最近 50 条 + 各类裁决的计数。计数比明细更常用——
/// 看一眼就知道最近是不是一直在被拒，或者自动放行的占比是不是高得不正常。
async fn audit_view(
    State(st): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).min(1000);
    let items = st.audit.tail(limit, q.decision.as_deref(), q.client.as_deref());
    Json(json!({
        "path": st.audit.path().map(|p| p.display().to_string()),
        "count": items.len(),
        "summary": st.audit.summary(1000),
        // 这条必须写出来：客户端本地命中 allow 规则的调用直接放行、没联网，
        // 所以不在这份日志里。审计记的是「需要人裁决过的」，不是全量工具调用。
        // 不说清楚的话，看日志的人会以为 agent 只跑了这么几条命令。
        "note": "客户端本地命中 allow 规则的调用不联网、不记录；这里只有到达过 hub 的请求",
        "items": items,
    }))
}

/// agent 状态上报。见 hub/src/agentstate.rs 的说明。
///
/// 这个接口永远返回 200：上报方是 fire-and-forget 的 hook，
/// 它拿到错误也没什么能做，而让它因此失败会把一个观测功能变成失败模式。
async fn state_post(State(st): State<AppState>, Json(req): Json<StateReport>) -> impl IntoResponse {
    let idle = st.shared.status().await.mode == crate::state::Mode::Idle
        && st.approvals.pending().await.0.is_none();
    let cur = agentstate::apply(&st.shared, &req, idle).await;
    debug!("state {:?} from {} (device_idle={idle})", req.state, req.source.label());
    *st.agent_state.lock().await = Some(cur);
    Json(json!({"ok": true}))
}

async fn state_get(State(st): State<AppState>) -> impl IntoResponse {
    let v = st.agent_state.lock().await.as_ref().map(Current::view);
    Json(json!({"state": v}))
}

/// 任务列表上报。语义是**全量替换**：agent 每次把当前列表整份推过来。
///
/// 和状态上报一样永远返回 200 —— 上报方是 fire-and-forget，
/// 让它因为一个观测功能而失败是本末倒置。
async fn tasks_post(
    State(st): State<AppState>,
    Json(req): Json<tasks::TaskReport>,
) -> impl IntoResponse {
    let idle = st.shared.status().await.mode == crate::state::Mode::Idle
        && st.approvals.pending().await.0.is_none();
    let n = req.tasks.len();
    let label = req.source.label();
    let fp = tasks::key_fingerprint(st.api_key.as_deref().unwrap_or(""));
    {
        let mut t = st.agent_tasks.lock().await;
        t.replace(&fp, req);
        tasks::apply(&st.shared, &t, idle).await;
    }
    debug!("tasks {n} from {label} (device_idle={idle}, bucket={fp})");
    Json(json!({"ok": true, "accepted": n, "pushed_to_device": idle}))
}

async fn tasks_get(State(st): State<AppState>) -> impl IntoResponse {
    let t = st.agent_tasks.lock().await;
    Json(json!({
        "buckets": t.views(),
        "device_lines": t.display_lines(),
        "note": "设备上只显示 status=doing 的条目：待办是计划、计划随时会变，                 而站在设备前的人关心此刻在动什么",
    }))
}

#[derive(Deserialize)]
struct DispReq {
    /// msg | msg_clear | status | hints | clock | test
    op: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    mode: String,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    h: Vec<String>,
}

/// 通用屏幕指令，主要给调试用；agent 走 /approve 就够了
async fn disp(State(st): State<AppState>, Json(req): Json<DispReq>) -> impl IntoResponse {
    let op = match req.op.as_str() {
        "msg" => DispOp::Msg { text: req.text, color: req.color },
        "msg_clear" => DispOp::MsgClear,
        "status" => {
            // 手工从 /disp 推的全屏视图算查询屏（可用 * 退掉）：
            // 真正的审批屏只由 approval.rs 产生
            DispOp::Status {
                mode: req.mode,
                text: req.text,
                color: req.color,
                skip: 0,
                transient: true,
            }
        }
        "hints" => {
            let mut h: [String; 4] = Default::default();
            for (i, slot) in h.iter_mut().enumerate() {
                *slot = req.h.get(i).cloned().unwrap_or_default();
            }
            DispOp::Hints { h }
        }
        "clock" => DispOp::Clock,
        "badge" => DispOp::Badge { text: req.text },
        "test" => DispOp::Test,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("unknown op: {other}")})),
            );
        }
    };
    st.shared.send(HostMsg::Disp(op)).await;
    (StatusCode::OK, Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct LedReq {
    id: u8,
    mode: String,
    #[serde(default)]
    hz: Option<f32>,
}

async fn led(State(st): State<AppState>, Json(req): Json<LedReq>) -> impl IntoResponse {
    st.shared.send(HostMsg::Led { id: req.id, mode: parse_led_mode(&req.mode), hz: req.hz }).await;
    Json(json!({"ok": true}))
}

fn parse_led_mode(s: &str) -> LedMode {
    match s {
        "on" => LedMode::On,
        "blink" => LedMode::Blink,
        _ => LedMode::Off,
    }
}

// ---------- 审批 ----------

/// 阻塞式审批入口。请求体见 docs/client-protocol.md，同时兼容旧的扁平形态。
///
/// 顺序：规则分级 -> allow 直接放行（不打扰人）-> 否则上屏等按键 -> 写审计。
async fn approve(
    State(st): State<AppState>,
    Json(req): Json<ApproveRequest>,
) -> impl IntoResponse {
    let started = Instant::now();
    let title = req.display_title();
    let detail = req.display_detail();
    let label = req.source.label();
    let input_text = req.tool.input_text();
    // 规则匹配的语料：优先用工具参数，纯手工请求（没有 tool）时退回标题
    let corpus = if input_text.is_empty() { title.clone() } else { input_text.clone() };

    let (verdict, rule) = st.rules.classify(&req.source.client, &req.tool.name, &corpus);
    // 请求里显式给的 risk 优先于规则表
    let (verdict, rule) = match req.risk {
        Some(r) => (Verdict::Ask(r), "explicit".to_string()),
        None => (verdict, rule),
    };

    let write_audit = |id: u64, risk: &str, decision: Decision, key: Option<&'static str>| {
        st.audit.write(&audit::Entry {
            ts: audit::now_rfc3339(),
            id,
            client: req.source.client.clone(),
            host: req.source.host.clone(),
            user: req.source.user.clone(),
            cwd: req.source.cwd.clone(),
            session: req.source.session.clone(),
            tool: req.tool.name.clone(),
            input: truncate(&input_text, 400),
            risk: risk.to_string(),
            rule: rule.clone(),
            decision: format!("{decision:?}").to_lowercase(),
            key: key.map(str::to_string),
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    };

    // 规则判定为 allow：不上屏、不打扰，但要留审计痕迹
    let risk = match verdict {
        Verdict::Allow => {
            debug!("rule `{rule}` allows {} without asking", req.tool.name);
            write_audit(0, "allow", Decision::RuleAllow, None);
            return (
                StatusCode::OK,
                Json(ApproveResponse {
                    ok: true,
                    id: 0,
                    decision: Decision::RuleAllow,
                    approved: true,
                    reason: format!("allowed by rule: {rule}"),
                    risk: "allow".into(),
                    rule,
                }),
            );
        }
        Verdict::Ask(r) => r,
    };

    let risk_name = if risk == Risk::High { "high" } else { "normal" };

    if !st.shared.status().await.device_online {
        // 设备不在线时不要把调用方吊死，直接说清楚。
        // 客户端据此决定放行还是阻止（见 docs/client-protocol.md 的失败语义）
        warn!("approve rejected: device offline");
        write_audit(0, risk_name, Decision::Cancelled, None);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApproveResponse {
                ok: false,
                id: 0,
                decision: Decision::Cancelled,
                approved: false,
                reason: "device offline".into(),
                risk: risk_name.into(),
                rule,
            }),
        );
    }

    let timeout = req.timeout_seconds().map(Duration::from_secs).unwrap_or(st.approve_timeout);
    info!(
        "approve [{}] {} tool={} risk={risk_name} rule={rule}",
        label, title, req.tool.name
    );
    // 屏幕一行 21 个 ASCII，前缀 @ 占 1 个，剩 20 个给路径
    let cwd_short = req.source.cwd_short(20);
    let (id, decision) = st
        .approvals
        .request(crate::approval::RequestSpec {
            title,
            detail,
            label,
            client: crate::wire::short_client(&req.source.client).to_string(),
            cwd: cwd_short,
            risk,
            timeout,
        })
        .await;

    let key = st.shared.status().await.last_key.map(|k| k.label);
    let key = if matches!(decision, Decision::Accept | Decision::Reject) { key } else { None };
    write_audit(id, risk_name, decision, key);

    (
        StatusCode::OK,
        Json(ApproveResponse {
            ok: true,
            id,
            decision,
            approved: decision.approved(),
            reason: reason_for(decision, key),
            risk: risk_name.into(),
            rule,
        }),
    )
}

/// 拒绝时这段话会被客户端写到 stderr，进而进入 agent 的上下文。
/// 所以要写得让 agent 知道「被人拒了、该换方案」，而不是一句无信息的 denied。
fn reason_for(decision: Decision, key: Option<&'static str>) -> String {
    match decision {
        Decision::Accept => format!("approved on kiboard (key {})", key.unwrap_or("?")),
        Decision::AutoAccept => "auto-approved: kiboard is in accept-all window".into(),
        Decision::RuleAllow => "allowed by rule".into(),
        Decision::Reject => format!(
            "rejected by the user on kiboard (key {}). Do not retry the same action; \
             ask the user what to do instead or propose a different approach.",
            key.unwrap_or("?")
        ),
        Decision::Timeout => {
            "no response on kiboard before timeout. The user may be away; do not retry \
             automatically."
                .into()
        }
        Decision::Cancelled => "request cancelled or device offline".into(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "..."
}

async fn pending(State(st): State<AppState>) -> impl IntoResponse {
    let (p, depth) = st.approvals.pending().await;
    Json(json!({"pending": p, "queued": depth}))
}

#[derive(Deserialize)]
struct DecideReq {
    /// accept | reject
    decision: String,
}

async fn decide(State(st): State<AppState>, Json(req): Json<DecideReq>) -> impl IntoResponse {
    let d = match req.decision.as_str() {
        "accept" => Decision::Accept,
        "reject" => Decision::Reject,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("unknown decision: {other}")})),
            );
        }
    };
    match st.approvals.decide(d).await {
        Some(id) => (StatusCode::OK, Json(json!({"ok": true, "id": id}))),
        None => (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "no pending request"})),
        ),
    }
}

async fn cancel(State(st): State<AppState>) -> impl IntoResponse {
    let n = st.approvals.cancel_all().await;
    Json(json!({"ok": true, "cancelled": n}))
}

async fn auto_get(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.approvals.auto_view().await)
}

async fn auto_clear(State(st): State<AppState>) -> impl IntoResponse {
    st.approvals.clear_auto().await;
    Json(json!({"ok": true, "mode": "off"}))
}

// ---------- 设备接入 ----------

async fn device_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(st): State<AppState>,
) -> impl IntoResponse {
    // 监听 0.0.0.0，局域网内任何人都能连，必须校验 token
    if params.get("token").map(String::as_str) != Some(st.token.as_str()) {
        warn!("device connection rejected: bad token");
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| device_session(socket, st)).into_response()
}

async fn device_session(mut socket: WebSocket, st: AppState) {
    info!("device connected via websocket");

    // 注册无线出口，之后 shared.send 会优先走这里
    let (tx, mut rx) = mpsc::channel::<HostMsg>(64);
    let ws_id = st.shared.register_ws_out(tx).await;
    st.shared.send(HostMsg::Ping).await;

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(txt))) => {
                    device::handle_line(
                        txt.trim(), Transport::WebSocket,
                        &st.shared, &st.events, &st.approvals,
                    ).await;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { debug!("device ws error: {e}"); break; }
            },
            Some(cmd) = rx.recv() => {
                let Ok(txt) = serde_json::to_string(&cmd) else { continue };
                debug!("-> ws {txt}");
                if socket.send(Message::Text(txt.into())).await.is_err() { break; }
            }
        }
    }

    st.shared.unregister_ws_out(ws_id).await;
    if st.shared.mark_transport_down(Transport::WebSocket).await {
        let _ = st.events.send(HubEvent::DeviceDown);
    }
    info!("device disconnected (websocket)");
}

// ---------- 客户端订阅 ----------

async fn client_upgrade(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| client_session(socket, st))
}

async fn client_session(mut socket: WebSocket, st: AppState) {
    info!("ws client connected");
    let mut rx = st.events.subscribe();

    let (p, depth) = st.approvals.pending().await;
    if let Ok(snapshot) = serde_json::to_string(&json!({
        "event": "snapshot",
        "status": st.shared.status().await,
        "pending": p,
        "queued": depth,
        "auto": st.approvals.auto_view().await,
    })) {
        let _ = socket.send(Message::Text(snapshot.into())).await;
    }

    loop {
        tokio::select! {
            evt = rx.recv() => match evt {
                Ok(evt) => {
                    let Ok(txt) = serde_json::to_string(&evt) else { continue };
                    if socket.send(Message::Text(txt.into())).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => debug!("ws lagged {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(txt))) => handle_client_cmd(&txt, &st).await,
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { debug!("ws error: {e}"); break; }
            },
        }
    }
    info!("ws client disconnected");
}

async fn handle_client_cmd(txt: &str, st: &AppState) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else {
        debug!("ws bad json: {txt}");
        return;
    };
    match v["t"].as_str() {
        Some("msg") => {
            let text = v["text"].as_str().unwrap_or("").to_string();
            let color = v["color"].as_str().unwrap_or("white").to_string();
            st.shared.send(HostMsg::msg(text, color)).await;
        }
        Some("led") => {
            let id = v["id"].as_u64().unwrap_or(0) as u8;
            let mode = parse_led_mode(v["mode"].as_str().unwrap_or("off"));
            let hz = v["hz"].as_f64().map(|f| f as f32);
            st.shared.send(HostMsg::Led { id, mode, hz }).await;
        }
        Some("clock") => st.shared.send(HostMsg::Disp(DispOp::Clock)).await,
        Some("decide") => {
            let d = match v["decision"].as_str() {
                Some("accept") => Decision::Accept,
                Some("reject") => Decision::Reject,
                _ => return,
            };
            st.approvals.decide(d).await;
        }
        Some("ping") => st.shared.send(HostMsg::Ping).await,
        other => debug!("ws unknown cmd: {other:?}"),
    }
}
