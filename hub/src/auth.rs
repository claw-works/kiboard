//! HTTP / 客户端 WS 的访问控制
//!
//! 为什么需要：hub 必须绑 0.0.0.0 才能让设备从局域网连入，而一旦路由器上做了
//! 端口转发（把它暴露到公网），没有认证的 `POST /decide` 就等于把「批准按钮」
//! 挂到互联网上——任何人都能替你批准掉屏幕上挂着的请求。
//! 一个审批设备被人绕过审批，就完全失去意义了。
//!
//! 两个密钥职责分开，不要复用：
//!   KIBOARD_TOKEN   设备接入 /device 用。它烧在固件里，泄漏面较大。
//!   KIBOARD_API_KEY 调用 HTTP 接口和订阅 /ws 用。只在 agent 侧持有。
//! 分开的好处：固件被人读出 token，也只能冒充设备上报按键，不能直接裁决请求。
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tracing::warn;

/// 需要放行的路径：/device 有自己的 token 校验，健康检查不需要密钥
fn is_open_path(path: &str) -> bool {
    matches!(path, "/device" | "/health")
}

pub async fn require_api_key(
    State(expected): State<Option<String>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = expected else {
        // 没配 API key：放行，但启动时已经警告过了
        return next.run(req).await;
    };

    let path = req.uri().path();
    if is_open_path(path) {
        return next.run(req).await;
    }

    // 优先看 header；浏览器发起 WebSocket 时没法自定义 header，所以也接受 ?key=
    let from_header = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let from_query = req.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == "key")
            .map(|(_, v)| v.to_string())
    });

    let provided = from_header.or(from_query);
    if provided.as_deref() == Some(expected.as_str()) {
        return next.run(req).await;
    }

    warn!("unauthorized request to {path}");
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "ok": false,
            "error": "missing or bad API key",
            "hint": "带上 X-Api-Key 头，或对 WebSocket 用 ?key=<KIBOARD_API_KEY>"
        })),
    )
        .into_response()
}
