//! 串口链路：开发调试用，断线自动重连
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, info, warn};

use crate::approval::Approvals;
use crate::device;
use crate::protocol::{HostMsg, HubEvent};
use crate::state::{Shared, Transport};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub async fn run(
    port_pattern: String,
    baud: u32,
    shared: Shared,
    mut outbox: mpsc::Receiver<HostMsg>,
    events: broadcast::Sender<HubEvent>,
    approvals: Approvals,
) {
    loop {
        // 支持 glob：macOS 上 usbmodem 后面的数字会随 USB 口变化
        let port = match resolve_port(&port_pattern) {
            Some(p) => p,
            None => {
                debug!("no serial port matching {port_pattern}");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        match tokio_serial::new(&port, baud).open_native_async() {
            Ok(stream) => {
                info!("serial connected: {port}");
                if let Err(e) = pump(stream, &shared, &mut outbox, &events, &approvals).await {
                    warn!("serial link lost: {e}");
                }
            }
            Err(e) => debug!("serial open failed: {e}"),
        }
        if shared.mark_transport_down(Transport::Serial).await {
            let _ = events.send(HubEvent::DeviceDown);
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// 把 /dev/cu.usbmodem* 之类的模式解析成真实路径；不含 * 时原样返回
fn resolve_port(pattern: &str) -> Option<String> {
    if !pattern.contains('*') {
        return Some(pattern.to_string());
    }
    let (dir, prefix) = pattern.rsplit_once('/')?;
    let prefix = prefix.trim_end_matches('*');
    let mut hits: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(prefix))
        .map(|name| format!("{dir}/{name}"))
        .collect();
    hits.sort();
    hits.into_iter().next()
}

async fn pump(
    stream: tokio_serial::SerialStream,
    shared: &Shared,
    outbox: &mut mpsc::Receiver<HostMsg>,
    events: &broadcast::Sender<HubEvent>,
    approvals: &Approvals,
) -> anyhow::Result<()> {
    let (rx, mut tx) = tokio::io::split(stream);
    let mut lines = BufReader::new(rx).lines();

    send(&mut tx, &HostMsg::Ping).await?;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    anyhow::bail!("device closed the port");
                };
                let line = line.trim();
                if line.is_empty() { continue; }
                device::handle_line(line, Transport::Serial, shared, events, approvals).await;
            }
            Some(msg) = outbox.recv() => {
                send(&mut tx, &msg).await?;
            }
        }
    }
}

async fn send<W: AsyncWriteExt + Unpin>(tx: &mut W, msg: &HostMsg) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    tx.write_all(&buf).await?;
    tx.flush().await?;
    debug!("-> serial {}", String::from_utf8_lossy(&buf).trim());
    Ok(())
}
