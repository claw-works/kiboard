//! 设备消息处理：串口和 WS 两种链路共用这套逻辑
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::approval::Approvals;
use crate::keymap;
use crate::protocol::{DeviceMsg, HubEvent};
use crate::state::{Shared, Transport};

/// 处理设备发来的一行 JSON（或非 JSON 日志）
pub async fn handle_line(
    line: &str,
    transport: Transport,
    shared: &Shared,
    events: &broadcast::Sender<HubEvent>,
    approvals: &Approvals,
) {
    if !line.starts_with('{') {
        debug!("device log ({transport:?}): {line}");
        let _ = events.send(HubEvent::Log { text: line.to_string() });
        return;
    }

    match serde_json::from_str::<DeviceMsg>(line) {
        Ok(DeviceMsg::Hello { fw, keys, leds, disp, ip }) => {
            info!("device up via {transport:?}: fw={fw} keys={keys} leds={leds} disp={disp:?} ip={ip:?}");
            shared.set_device_online(true, transport).await;
            shared.set_firmware(fw.clone()).await;
            shared.set_keys_total(keys).await;
            let _ = events.send(HubEvent::DeviceUp { fw, keys });
            // 告诉设备 hub 是哪一版：logo 页显示，一眼看出连的是哪一版
            shared
                .send(crate::protocol::HostMsg::Disp(crate::protocol::DispOp::HubInfo {
                    version: crate::version::VERSION.to_string(),
                }))
                .await;
            // 任务列表补推，否则设备重启后任务页一直空着
            crate::tasks::repaint(shared).await;
            // 设备刚上线，把当前待批请求重新推一遍（可能是设备重启前就在等了）
            approvals.repaint().await;
        }
        Ok(DeviceMsg::Repaint {}) => {
            // 设备唤醒后要求补画。有待批请求就重画请求，没有就把任务列表推一遍
            debug!("device asked for repaint via {transport:?}");
            approvals.repaint().await;
            crate::tasks::repaint(shared).await;
        }
        Ok(DeviceMsg::Pong { uptime_ms }) => {
            debug!("pong uptime={uptime_ms}ms via {transport:?}");
            if shared.set_device_online(true, transport).await {
                let fw = shared.firmware().await;
                let _ = events.send(HubEvent::DeviceUp { fw, keys: 0 });
            }
            shared.note_pong(uptime_ms).await;
        }
        Ok(DeviceMsg::Key { id, row, col, act }) => {
            let row = row.unwrap_or(id / 4 + 1);
            let col = col.unwrap_or(id % 4 + 1);
            let label = keymap::label(id);
            let action = keymap::action(id);
            info!("key {id} [{label}] R{row}C{col} {act:?} -> {action:?}");
            shared.note_key(id, act).await;
            let _ = events.send(HubEvent::Key { id, label, row, col, act });

            // 三种都要传进去：高危请求的按住时长由 hub 从 press 到 release 计时
            approvals.on_action(action, id, act).await;
        }
        Ok(DeviceMsg::Wifi { status, ssid, ip, rssi, reason }) => {
            info!("wifi {status} ssid={ssid:?} ip={ip:?} rssi={rssi:?} reason={reason:?}");
            shared.set_wifi(status.clone(), ssid.clone(), rssi).await;
            let _ = events.send(HubEvent::Wifi { status, ssid, rssi });
        }
        Ok(DeviceMsg::Keys { matrix, idle_cols }) => {
            info!("matrix scan: {matrix:?} idle_cols={idle_cols:?}");
            if idle_cols.iter().any(|v| *v != 0) {
                warn!("列脚空闲时不为低：{idle_cols:?} —— 检查接线，可能接到了电源轨或行线");
            }
        }
        Ok(DeviceMsg::Ok { cmd }) => debug!("device ok: {cmd}"),
        Ok(DeviceMsg::Disp { op, lines }) => {
            // status 的回执带折行总行数，滚动范围要靠它来夹
            if let Some(n) = lines {
                approvals.note_total_lines(n as usize).await;
            }
            debug!("device disp ok: {op} lines={lines:?}");
        }
        Ok(DeviceMsg::Err { msg }) => warn!("device err: {msg}"),
        Ok(DeviceMsg::Unknown) => debug!("device unknown msg: {line}"),
        Err(e) => warn!("bad json from device: {e}: {line}"),
    }
}
