//! 共享状态：设备在线情况、Wi-Fi、最近按键、当前模式、出口路由
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{mpsc, Mutex};

use crate::protocol::{HostMsg, KeyAct};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Idle,
    AwaitingAction,
    /// 预留：agent 正在执行已批准的动作
    #[allow(dead_code)]
    Running,
    /// 预留：出错等待处理
    #[allow(dead_code)]
    Error,
}

/// 设备接入方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Serial,
    WebSocket,
}

#[derive(Debug)]
struct Inner {
    device_online: bool,
    transport: Option<Transport>,
    firmware: String,
    uptime_ms: u64,
    wifi_status: String,
    wifi_ssid: Option<String>,
    wifi_rssi: Option<i32>,
    mode: Mode,
    last_key: Option<(u8, String, KeyAct)>,
    keys_total: u8,
    started: Instant,
    /// 无线链路出口，设备 WS 连上时注册；带 id 以防后来者清掉别人的注册
    ws_out: Option<(u64, mpsc::Sender<HostMsg>)>,
    next_ws_id: u64,
    /// 串口出口，常驻
    serial_out: mpsc::Sender<HostMsg>,
    /// 最后一次下发给设备的任务标题（已排序过滤）与实际总数。
    /// 放在这里而不是 AppState，是为了设备重连时能就地补推——
    /// 设备重启后任务页空着，而下一次上报可能要等很久，
    /// 待机屏的价值恰恰在于随时抬头看都是对的。
    last_tasks: (Vec<String>, usize),
}

#[derive(Clone)]
pub struct Shared {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Serialize)]
pub struct StatusView {
    pub device_online: bool,
    pub transport: Option<Transport>,
    pub firmware: String,
    pub device_uptime_ms: u64,
    pub wifi_status: String,
    pub wifi_ssid: Option<String>,
    pub wifi_rssi: Option<i32>,
    pub mode: Mode,
    pub keys_total: u8,
    pub last_key: Option<LastKey>,
    pub hub_uptime_s: u64,
}

#[derive(Debug, Serialize)]
pub struct LastKey {
    pub id: u8,
    /// 丝印标签由设备上报。hub 不持有键位表
    pub label: String,
    pub row: u8,
    pub col: u8,
    pub act: KeyAct,
}

impl Shared {
    pub fn new(serial_out: mpsc::Sender<HostMsg>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                device_online: false,
                transport: None,
                firmware: String::new(),
                uptime_ms: 0,
                wifi_status: "unknown".into(),
                wifi_ssid: None,
                wifi_rssi: None,
                mode: Mode::Idle,
                last_key: None,
                keys_total: 0,
                started: Instant::now(),
                ws_out: None,
                next_ws_id: 1,
                serial_out,
                last_tasks: (Vec::new(), 0),
            })),
        }
    }

    /// 记下最后一次下发的任务，供设备重连时补推
    pub async fn set_last_tasks(&self, items: Vec<String>, total: usize) {
        self.inner.lock().await.last_tasks = (items, total);
    }

    pub async fn last_tasks(&self) -> (Vec<String>, usize) {
        self.inner.lock().await.last_tasks.clone()
    }

    /// 发指令给设备：优先走无线，没有无线才走串口。
    /// 两条链路都不通时静默丢弃（设备离线，指令无意义）。
    pub async fn send(&self, msg: HostMsg) {
        let g = self.inner.lock().await;
        let out = match g.ws_out.as_ref() {
            Some((_, tx)) => tx,
            None => &g.serial_out,
        };
        if out.try_send(msg).is_err() {
            tracing::debug!("outbox full or closed, command dropped");
        }
    }

    /// 返回注册 id，断开时用它调 unregister
    pub async fn register_ws_out(&self, tx: mpsc::Sender<HostMsg>) -> u64 {
        let mut g = self.inner.lock().await;
        let id = g.next_ws_id;
        g.next_ws_id += 1;
        g.ws_out = Some((id, tx));
        id
    }

    /// 只有当前注册者才能注销，避免后连的客户端断开时清掉别人的出口
    pub async fn unregister_ws_out(&self, id: u64) {
        let mut g = self.inner.lock().await;
        if g.ws_out.as_ref().map(|(cur, _)| *cur) == Some(id) {
            g.ws_out = None;
        }
    }

    /// 返回 true 表示在线状态发生了变化
    pub async fn set_device_online(&self, online: bool, transport: Transport) -> bool {
        let mut g = self.inner.lock().await;
        let changed = g.device_online != online;
        g.device_online = online;
        g.transport = if online { Some(transport) } else { None };
        changed
    }

    /// 某条链路断开：只有当前生效的那条断了才算设备离线。
    ///
    /// 无线链路要额外确认没有别的会话顶上来：重刷固件时设备会立刻重连，
    /// 新会话已经注册好出口了，旧会话的清理才跑到这里。若不检查 ws_out，
    /// 旧会话的收尾会把新会话刚建立的在线状态清掉（表现为设备明明连着却显示离线，
    /// 要等下一次心跳 pong 才自愈）。
    pub async fn mark_transport_down(&self, transport: Transport) -> bool {
        let mut g = self.inner.lock().await;
        if g.transport != Some(transport) {
            return false;
        }
        if transport == Transport::WebSocket && g.ws_out.is_some() {
            return false;  // 已有新会话接管
        }
        let changed = g.device_online;
        g.device_online = false;
        g.transport = None;
        changed
    }

    pub async fn set_firmware(&self, fw: String) {
        self.inner.lock().await.firmware = fw;
    }

    pub async fn firmware(&self) -> String {
        self.inner.lock().await.firmware.clone()
    }

    pub async fn note_pong(&self, uptime_ms: u64) {
        self.inner.lock().await.uptime_ms = uptime_ms;
    }

    pub async fn note_key(&self, id: u8, label: String, act: KeyAct) {
        self.inner.lock().await.last_key = Some((id, label, act));
    }

    pub async fn set_wifi(&self, status: String, ssid: Option<String>, rssi: Option<i32>) {
        let mut g = self.inner.lock().await;
        g.wifi_status = status;
        g.wifi_ssid = ssid;
        g.wifi_rssi = rssi;
    }

    pub async fn set_keys_total(&self, n: u8) {
        self.inner.lock().await.keys_total = n;
    }

    pub async fn set_mode(&self, mode: Mode) {
        self.inner.lock().await.mode = mode;
    }

    pub async fn status(&self) -> StatusView {
        let g = self.inner.lock().await;
        StatusView {
            device_online: g.device_online,
            transport: g.transport,
            firmware: g.firmware.clone(),
            device_uptime_ms: g.uptime_ms,
            wifi_status: g.wifi_status.clone(),
            wifi_ssid: g.wifi_ssid.clone(),
            wifi_rssi: g.wifi_rssi,
            mode: g.mode,
            keys_total: g.keys_total,
            last_key: g.last_key.clone().map(|(id, label, act)| LastKey {
                id,
                label,
                row: id / 4 + 1,
                col: id % 4 + 1,
                act,
            }),
            hub_uptime_s: g.started.elapsed().as_secs(),
        }
    }
}
