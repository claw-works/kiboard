//! 设备消息处理：串口和 WS 两种链路共用这套逻辑
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::approval::Approvals;
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
        Ok(DeviceMsg::Key { id, row, col, label, act }) => {
            let row = row.unwrap_or(id / 4 + 1);
            let col = col.unwrap_or(id % 4 + 1);
            // 标签由设备给。hub 不再有键位表——它只是把事件透出去给 WS 订阅者看，
            // 审批语义走 Decision 那条路
            let label = label.unwrap_or_else(|| format!("id{id}"));
            debug!("key {id} [{label}] R{row}C{col} {act:?}");
            shared.note_key(id, label.clone(), act).await;
            let _ = events.send(HubEvent::Key { id, label, row, col, act });
        }
        Ok(DeviceMsg::Decision { id, verdict, confirm }) => {
            info!("decision from device: {verdict:?} for {id:?}");
            approvals.on_decision(id, verdict, confirm).await;
        }
        Ok(DeviceMsg::Query { what }) => {
            debug!("query from device: {what}");
            approvals.on_query(&what).await;
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
            // 折行总行数以前是 hub 用来夹滚动范围的。滚动现在完全在设备侧，
            // 这个回执只留作调试信息
            debug!("device disp ok: {op} lines={lines:?}");
        }
        Ok(DeviceMsg::Err { msg }) => warn!("device err: {msg}"),
        Ok(DeviceMsg::Unknown) => debug!("device unknown msg: {line}"),
        Err(e) => warn!("bad json from device: {e}: {line}"),
    }
}
