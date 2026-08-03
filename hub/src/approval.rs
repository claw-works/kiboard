//! 审批中心：agent 提交请求 -> 设备亮灯+屏显 -> 用户按键 -> 返回决定
//!
//! 这是 kiboard 的核心用途。设计取舍：
//! - 同一时刻只展示一个请求，其余排队。设备只有一块小屏，同时展示多个没有意义。
//! - 「全部接受」在 TTL 内自动裁决后续请求，避免连续确认的疲劳；但它必须
//!   全程可见（顶栏角标 + 剩余秒数）且随手可关（D 键），否则是个静默放行一切的陷阱。
//! - 高危请求（risk=high）必须长按接受，短按只给提示。防手滑比少按一下重要。
//! - 请求方用一个阻塞的 HTTP 调用等结果，超时自动返回 timeout，不需要轮询。
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{info, warn};

/// 设备上"审批过的"那一屏最多回看多少条。屏幕放得下 4 行，留 10 条够翻两页
const RECENT_KEEP: usize = 10;
/// 查询屏（0/5/6）停留多久自动回首屏。8 秒够读完 4 行，又不会把设备长期占住
const TRANSIENT_SCREEN_TTL: Duration = Duration::from_secs(8);
/// 查询屏一屏放几条。这是 hub 仍然知道的最后一点屏幕细节，
/// 因为这几屏的内容还由 hub 排版（见 protocol/README.md 的实现状态）
const QUERY_LINES: usize = 4;

/// 把时长压成 4 个字符以内。设备一行只有 21 个 ASCII，
/// 时间戳占的每一个字符都是从命令那里抢来的
fn ago_short(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}
use crate::protocol::{Confirm, DispOp, HostMsg, HubEvent, LedMode, RequestMsg, Verdict};
use crate::state::{Mode, Shared};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accept,
    Reject,
    /// 被自动接受（此前按过「全部接受」）
    AutoAccept,
    /// 规则表判定为 allow，没有打扰人。审计里要能和人工批准区分开
    RuleAllow,
    Timeout,
    /// 请求被取消，或设备离线无法展示
    Cancelled,
}

impl Decision {
    pub fn approved(self) -> bool {
        matches!(self, Decision::Accept | Decision::AutoAccept | Decision::RuleAllow)
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    #[default]
    Normal,
    /// 需要长按才能接受；也不会被「全部接受」自动放行
    High,
}

/// 提交一个审批请求需要的全部信息。打包成结构体而不是一长串参数：
/// 参数多了以后位置传参很容易搞错，而且这些字段大多是给屏幕用的，本来就属于一组。
/// 一次已落定的裁决。设备上两屏用它，所以只留能显示得下的字段
#[derive(Debug, Clone)]
pub struct Settled {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub label: String,
    pub cwd: String,
    pub risk: Risk,
    pub decision: Decision,
    /// 裁决来源：device / api。不再记键号——键号只有物理键盘才有
    pub by: Option<&'static str>,
    pub at: Instant,
}

pub struct RequestSpec {
    /// 真正要执行的东西（命令/路径），屏幕第一行
    pub title: String,
    /// 补充说明。客户端给的话通常是模型写的意图，可信度最低，排最后
    pub detail: String,
    /// 来源短标签 kiro@kiboard，普通请求用
    pub label: String,
    /// 客户端简称，高危请求放标题条
    pub client: String,
    /// 缩短后的工作目录，高危请求单独一行
    pub cwd: String,
    pub risk: Risk,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestView {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub label: String,
    pub client: String,
    pub cwd: String,
    pub risk: Risk,
    pub waiting_s: u64,
    pub timeout_s: u64,
}

struct Request {
    id: u64,
    title: String,
    detail: String,
    /// 上屏用的来源短标签，形如 kiro@kiboard
    label: String,
    /// 客户端简称（kiro / cc），高危请求放在标题条里
    client: String,
    /// 缩短后的工作目录，高危请求单独占一行
    cwd: String,
    risk: Risk,
    created: Instant,
    timeout: Duration,
    /// 回给调用方的不只是裁决，还有**谁裁的**。
    /// 以前 API 侧是去问"最后一次按键是哪个"，那在设备自己翻译语义之后就没有按键事件了；
    /// 更根本的是：手机方案上根本没有键号。来源只能由裁决本身带回来。
    reply: oneshot::Sender<(Decision, Option<&'static str>)>,
}

impl Request {
    fn view(&self) -> RequestView {
        RequestView {
            id: self.id,
            title: self.title.clone(),
            detail: self.detail.clone(),
            label: self.label.clone(),
            client: self.client.clone(),
            cwd: self.cwd.clone(),
            risk: self.risk,
            waiting_s: self.created.elapsed().as_secs(),
            timeout_s: self.timeout.as_secs(),
        }
    }
}

struct Inner {
    active: Option<Request>,
    queue: VecDeque<Request>,
    /// 「全部接受」的截止时刻
    auto_until: Option<Instant>,
    next_id: u64,
    /// 上次被设备裁决的时刻，用于抑制紧随其后的重复上报
    last_key_decision: Option<Instant>,
    /// 最近若干次裁决，供设备上的"审批过的"与"最近详情"两屏使用。
    ///
    /// 刻意独立于审计日志：审计可以被关掉（KIBOARD_AUDIT=off），而这两屏
    /// 不该因为运维开关而失效——它是给站在设备前的人看的，不是给事后查账用的。
    /// 只留 10 条，多了屏幕也放不下。
    recent: VecDeque<Settled>,
    /// 屏幕是否亮着（* 键切换）
    screen_on: bool,
    /// 信息屏的代数，用来丢弃过期的自动返回任务
    info_gen: u64,
}

impl Inner {
    fn auto_active(&self) -> bool {
        self.auto_until.is_some_and(|t| Instant::now() < t)
    }
}

#[derive(Clone)]
pub struct Approvals {
    inner: Arc<Mutex<Inner>>,
    shared: Shared,
    events: broadcast::Sender<HubEvent>,
    auto_ttl: Duration,
    /// 高危请求要按住多久。由 hub 计时，不依赖固件的 long 阈值
    high_hold: Duration,
}

#[derive(Debug, Serialize)]
pub struct AutoView {
    pub mode: &'static str,
    pub remaining_s: u64,
}

impl Approvals {
    pub fn new(
        shared: Shared,
        events: broadcast::Sender<HubEvent>,
        auto_ttl: Duration,
        high_hold: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                queue: VecDeque::new(),
                auto_until: None,
                next_id: 1,
                last_key_decision: None,
                recent: VecDeque::new(),
                screen_on: true,
                info_gen: 0,
            })),
            shared,
            events,
            auto_ttl,
            high_hold,
        }
    }

    /// 提交一个请求并等到有结果。这是给 agent 用的主入口。
    pub async fn request(&self, spec: RequestSpec) -> (u64, Decision, Option<&'static str>) {
        let RequestSpec { title, detail, label, client, cwd, risk, timeout } = spec;
        let (tx, rx) = oneshot::channel();
        let id;
        let auto_hit;
        {
            let mut g = self.inner.lock().await;
            id = g.next_id;
            g.next_id += 1;
            // 高危请求不吃「全部接受」这条捷径，必须当面按
            auto_hit = risk == Risk::Normal && g.auto_active();
            if !auto_hit {
                let req = Request {
                    id,
                    title: title.clone(),
                    detail: detail.clone(),
                    label: label.clone(),
                    client: client.clone(),
                    cwd: cwd.clone(),
                    risk,
                    created: Instant::now(),
                    timeout,
                    reply: tx,
                };
                if g.active.is_none() {
                    g.active = Some(req);
                } else {
                    g.queue.push_back(req);
                }
            }
        }

        if auto_hit {
            info!("request #{id} auto-accepted (auto mode active)");
            let _ = self.events.send(HubEvent::Decision {
                id,
                decision: Decision::AutoAccept,
                by: None,
            });
            // 自动放行也要让人看见，否则「静默批准」就没人知道发生过什么
            self.shared
                .send(HostMsg::msg(format!("auto ok: {}", trim(&title, 17)), "white"))
                .await;
            return (id, Decision::AutoAccept, None);
        }

        let _ = self.events.send(HubEvent::Request {
            id,
            title: title.clone(),
            detail: detail.clone(),
            risk,
        });
        self.present().await;

        let (decision, by) = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => (Decision::Cancelled, None),
            Err(_) => {
                self.expire(id).await;
                (Decision::Timeout, None)
            }
        };
        (id, decision, by)
    }

    /// 设备（重）上线时把当前界面重推一遍
    pub async fn repaint(&self) {
        self.present().await;
        self.push_badge().await;
    }

    /// 把当前请求推到设备上：全屏审批界面 + 底部提示 + 黄灯闪
    async fn present(&self) {
        let (view, depth) = {
            let g = self.inner.lock().await;
            match g.active.as_ref() {
                Some(r) => (Some(r.view()), g.queue.len()),
                None => (None, 0),
            }
        };

        match view {
            Some(v) => {
                self.shared.set_mode(Mode::AwaitingAction).await;
                let _ = self.events.send(HubEvent::Mode { name: "awaiting_action".into() });
                // 只发字段，不拼字符串。怎么折行、哪行放哪个、滚到第几行，
                // 全是"这块屏多大"决定的事——那是设备的知识，不是 hub 的。
                // 高危请求要按住多久由 hub 给（改配置就生效），设备拿它做本地进度反馈。
                self.shared
                    .send(HostMsg::Request(RequestMsg {
                        id: v.id,
                        verbatim: v.title.clone(),
                        summary: v.detail.clone(),
                        label: v.label.clone(),
                        client: v.client.clone(),
                        cwd: v.cwd.clone(),
                        risk: v.risk,
                        hold_ms: if v.risk == Risk::High {
                            self.high_hold.as_millis() as u64
                        } else {
                            0
                        },
                        queued: depth,
                    }))
                    .await;
                // 高危用快闪，普通用慢闪：不看屏也能从灯的节奏感到差别
                let hz = if v.risk == Risk::High { 6.0 } else { 2.0 };
                self.shared.send(HostMsg::Led { id: 0, mode: LedMode::Blink, hz: Some(hz) }).await;
            }
            None => {
                self.shared.set_mode(Mode::Idle).await;
                let _ = self.events.send(HubEvent::Mode { name: "idle".into() });
                self.shared.send(HostMsg::Led { id: 0, mode: LedMode::Off, hz: None }).await;
                self.shared.send(HostMsg::Disp(DispOp::Clock)).await;
            }
        }
    }

    /// 设备裁决进来了。**hub 不知道人按了哪个键**，只知道人的意思。
    ///
    /// 高危请求的规则很硬：必须带够时长的 `confirm` 才算接受。设备已经在本地做过
    /// 进度反馈（灯转常亮、提示松手），但**门槛由 hub 复核**——阈值放 hub 才能
    /// 改配置就生效，也才不会各设备实现各判一套。
    ///
    /// `id` 用来绑定请求：一条隔夜的 accept 不能落到新请求上。不匹配就丢掉。
    pub async fn on_decision(&self, id: Option<u64>, verdict: Verdict, confirm: Option<Confirm>) {
        // 队列控制与请求无关，先处理，否则空闲时按下去只会得到一句 no request
        match verdict {
            Verdict::ClearAuto => {
                let was = {
                    let mut g = self.inner.lock().await;
                    let was = g.auto_active();
                    g.auto_until = None;
                    was
                };
                info!("auto-accept cleared by device (was_active={was})");
                self.shared
                    .send(HostMsg::msg(if was { "auto OFF" } else { "auto not on" }, "white"))
                    .await;
                self.push_badge().await;
                let _ = self.events.send(HubEvent::Auto { mode: "off", remaining_s: 0 });
                return;
            }
            Verdict::CancelAll => {
                let n = self.cancel_all().await;
                self.shared
                    .send(HostMsg::msg(
                        if n > 0 { format!("cancelled {n}") } else { "nothing to cancel".into() },
                        "white",
                    ))
                    .await;
                return;
            }
            _ => {}
        }

        let (active_id, is_high, recent_decision) = {
            let g = self.inner.lock().await;
            let recent =
                g.last_key_decision.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
            match g.active.as_ref() {
                Some(r) => (Some(r.id), r.risk == Risk::High, recent),
                None => (None, false, recent),
            }
        };

        let Some(active_id) = active_id else {
            // 没有待批请求还收到裁决：给一句反馈，别静默吞掉。
            // recent_decision 吃掉刚裁决完紧随而来的重复上报
            if !recent_decision {
                self.shared.send(HostMsg::msg("no request", "white")).await;
            }
            return;
        };

        // 绑定校验：设备说的那条必须就是当前这条
        if let Some(rid) = id {
            if rid != active_id {
                warn!("decision for #{rid} ignored: active is #{active_id}");
                return;
            }
        }

        match verdict {
            // 拒绝是安全方向，不设门槛，越顺手越好
            Verdict::Reject => self.finish(Decision::Reject, Some("device")).await,
            Verdict::Accept if is_high => {
                let held = confirm.as_ref().map(Confirm::held_ms).unwrap_or(0);
                let need = self.high_hold.as_millis() as u64;
                if held >= need {
                    self.finish(Decision::Accept, Some("device")).await;
                } else {
                    // 证据不足就是没按够。设备本地反馈可能已经提示过，
                    // 这里仍要拦——门槛的最终判定权在 hub
                    info!("high-risk hold too short: {held}ms < {need}ms");
                    self.shared
                        .send(HostMsg::msg(format!("too short ({held}ms)"), "yellow"))
                        .await;
                    self.shared
                        .send(HostMsg::Led { id: 0, mode: LedMode::Blink, hz: Some(6.0) })
                        .await;
                }
            }
            Verdict::Accept => self.finish(Decision::Accept, Some("device")).await,
            Verdict::AcceptWindow => {
                self.enable_auto().await;
                let released = self.drain_auto().await;
                if is_high {
                    // 高危请求绝不因为「全部接受」而放行，它继续留在屏上等长按。
                    // 这是修过的一个真 bug：按 3 能一下批掉高危请求，长按保护成了摆设
                    info!("auto-accept enabled; released {released}, high-risk request kept");
                    self.shared.send(HostMsg::msg("auto ON - hold 1 for this", "yellow")).await;
                    self.push_badge().await;
                    self.present().await;
                } else {
                    self.finish(Decision::Accept, Some("device")).await;
                }
            }
            Verdict::CancelAll | Verdict::ClearAuto => unreachable!("已在上面处理"),
        }
    }

    /// 设备要一屏只有 hub 知道的数据。设备不知道内容，但它知道人想看什么。
    pub async fn on_query(&self, what: &str) {
        match what {
            "info" => self.show_info().await,
            "recent" => self.show_recent().await,
            "last" => self.show_last_detail().await,
            "screen" => self.toggle_screen().await,
            other => warn!("unknown query from device: {other}"),
        }
    }

    /// 息屏 / 唤醒。OLED 能真正断电，离开工位时按一下，顺带减轻烧屏
    async fn toggle_screen(&self) {
        let on = {
            let mut g = self.inner.lock().await;
            g.screen_on = !g.screen_on;
            g.screen_on
        };
        self.shared.send(HostMsg::Disp(DispOp::Backlight { on })).await;
        if on {
            // 亮回来时重画，否则停在息屏前那一帧
            self.present().await;
            self.push_badge().await;
        }
        info!("screen {}", if on { "on" } else { "off" });
    }

    /// "审批过的"：最近几次裁决，一行一条。
    ///
    /// 一行只有 21 个 ASCII 的宽度，所以格式压到极简：`✓12:03 git push`。
    /// 决定用符号而不是 ACCEPT/REJECT 单词——省下的宽度全给命令本身，
    /// 而命令才是人要认的东西。
    async fn show_recent(&self) {
        let items = { self.inner.lock().await.recent.clone() };
        if items.is_empty() {
            self.transient_screen("REVIEWED", "还没有裁决过的请求".into()).await;
            return;
        }
        let mut body = String::new();
        for s in items.iter().take(QUERY_LINES) {
            if !body.is_empty() {
                body.push('\n');
            }
            let mark = if s.decision.approved() { "+" } else { "-" };
            let ago = ago_short(s.at.elapsed());
            body.push_str(&format!("{mark}{ago} {}", s.title));
        }
        self.transient_screen("REVIEWED", body).await;
    }

    /// 最近一次审批的详情。列表那屏一行放不下的东西在这里摊开：
    /// 来源、目录、风险等级、按了哪个键
    async fn show_last_detail(&self) {
        let last = { self.inner.lock().await.recent.front().cloned() };
        let Some(s) = last else {
            self.transient_screen("LAST", "还没有裁决过的请求".into()).await;
            return;
        };
        let mut body = String::new();
        body.push_str(&s.title);
        body.push('\n');
        let verdict = if s.decision.approved() { "允许" } else { "拒绝" };
        let by = match s.by {
            Some("device") => "设备".to_string(),
            Some(other) => other.to_string(),
            None => "接口".to_string(),
        };
        body.push_str(&format!("{verdict} {} {}前", by, ago_short(s.at.elapsed())));
        if !s.cwd.is_empty() {
            body.push('\n');
            body.push_str(&format!("@{}", s.cwd));
        }
        if !s.detail.is_empty() {
            body.push('\n');
            body.push_str(&s.detail);
        }
        let head = if s.risk == Risk::High { "LAST !!" } else { "LAST" };
        self.transient_screen(head, body).await;
    }

    /// 画一屏临时视图，几秒后自动回首屏。
    ///
    /// 有待批请求时一律不画：屏幕正在问一个需要决定的问题，
    /// 用查询结果去顶掉它是本末倒置。这条规则对 0/5/6 三屏都一样。
    async fn transient_screen(&self, head: &str, body: String) {
        if self.pending().await.0.is_some() {
            self.shared.send(HostMsg::msg("busy: request on screen", "white")).await;
            return;
        }
        self.shared
            .send(HostMsg::Disp(DispOp::Status {
                mode: head.into(),
                text: body,
                color: "white".into(),
                skip: 0,
                transient: true,
            }))
            .await;
        self.schedule_home_return().await;
    }

    /// 信息屏：链路、Wi-Fi、自动接受剩余、队列深度。
    ///
    /// 存在的理由是"不用切回终端就能知道设备连没连上"。有待批请求时不显示——
    /// 那时屏幕在问一个需要决定的问题，不该被状态查询顶掉。
    async fn show_info(&self) {
        let (pending, depth) = self.pending().await;
        if pending.is_some() {
            self.shared.send(HostMsg::msg("busy: request on screen", "white")).await;
            return;
        }
        let st = self.shared.status().await;
        let auto = self.auto_view().await;
        let link = match st.transport {
            Some(crate::state::Transport::WebSocket) => "wifi",
            Some(crate::state::Transport::Serial) => "usb",
            None => "-",
        };
        let mut body = format!("link {link}\n");
        body.push_str(&format!(
            "wifi {} {}dBm\n",
            st.wifi_ssid.as_deref().unwrap_or("-"),
            st.wifi_rssi.unwrap_or(0)
        ));
        body.push_str(&format!("auto {} {}s\n", auto.mode, auto.remaining_s));
        body.push_str(&format!("queue {depth}  up {}s", st.hub_uptime_s));

        self.shared
            .send(HostMsg::Disp(DispOp::Status {
                mode: "INFO".into(),
                text: body,
                color: "white".into(),
                skip: 0,
                transient: true,
            }))
            .await;
        self.schedule_home_return().await;
    }

    /// 看几秒就自动回首屏，别把设备停在某个查询屏上。
    ///
    /// 用代数计数丢弃过期的返回任务：连按两次 5，第一次的定时器不该把第二次画的屏收走。
    async fn schedule_home_return(&self) {
        // gen 在 2024 edition 是保留字，用 generation
        let generation = {
            let mut g = self.inner.lock().await;
            g.info_gen += 1;
            g.info_gen
        };
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(TRANSIENT_SCREEN_TTL).await;
            let stale = { this.inner.lock().await.info_gen != generation };
            if stale || this.pending().await.0.is_some() {
                return;
            }
            this.shared.send(HostMsg::Disp(DispOp::Clock)).await;
        });
    }

    /// 开启「全部接受」
    async fn enable_auto(&self) {
        let until = Instant::now() + self.auto_ttl;
        self.inner.lock().await.auto_until = Some(until);
        let _ = self
            .events
            .send(HubEvent::Auto { mode: "accept", remaining_s: self.auto_ttl.as_secs() });
    }

    /// 结掉当前请求：回复调用方、广播、反馈、把队列里下一个顶上来、重画
    async fn finish(&self, decision: Decision, by: Option<&'static str>) {
        let taken = {
            let mut g = self.inner.lock().await;
            if by.is_some() {
                g.last_key_decision = Some(Instant::now());
            }
            g.active.take()
        };
        let Some(req) = taken else { return };
        let id = req.id;
        let _ = req.reply.send((decision, by));
        info!("request #{id} decided by {}: {decision:?}", by.unwrap_or("api"));
        let _ = self.events.send(HubEvent::Decision { id, decision, by });
        // 让设备收屏并显示结果条。文案由设备自己定——"ACCEPTED" 该怎么写、
        // 用不用反色，是那块屏的事
        self.shared
            .send(HostMsg::RequestDone { id, verdict: decision_name(decision) })
            .await;

        {
            let mut g = self.inner.lock().await;
            g.recent.push_front(Settled {
                id,
                title: req.title.clone(),
                detail: req.detail.clone(),
                label: req.label.clone(),
                cwd: req.cwd.clone(),
                risk: req.risk,
                decision,
                by,
                at: Instant::now(),
            });
            g.recent.truncate(RECENT_KEEP);
        }

        self.feedback(decision).await;
        {
            let mut g = self.inner.lock().await;
            if g.active.is_none() {
                g.active = g.queue.pop_front();
            }
        }
        self.push_badge().await;
        self.present().await;
    }

    /// 裁决后的即时反馈：屏幕结果条 + 灯
    async fn feedback(&self, decision: Decision) {
        if decision.approved() {
            self.shared.send(HostMsg::Led { id: 0, mode: LedMode::Off, hz: None }).await;
        } else {
            // 拒绝亮一下红灯，手感上和接受区分开
            self.shared.send(HostMsg::Led { id: 0, mode: LedMode::Off, hz: None }).await;
            self.shared.send(HostMsg::Led { id: 1, mode: LedMode::On, hz: None }).await;
            let shared = self.shared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(800)).await;
                shared.send(HostMsg::Led { id: 1, mode: LedMode::Off, hz: None }).await;
            });
        }
    }

    /// 打开自动接受后，队列里排着的普通请求直接放行，不必逐个按。
    /// 高危请求留在队列里等人按。
    async fn drain_auto(&self) -> usize {
        let mut released = Vec::new();
        {
            let mut g = self.inner.lock().await;
            if !g.auto_active() {
                return 0;
            }
            let mut keep = VecDeque::new();
            while let Some(r) = g.queue.pop_front() {
                if r.risk == Risk::High {
                    keep.push_back(r);
                } else {
                    released.push(r);
                }
            }
            g.queue = keep;
        }
        let n = released.len();
        for req in released {
            let id = req.id;
            let _ = req.reply.send((Decision::AutoAccept, None));
            let _ = self.events.send(HubEvent::Decision {
                id,
                decision: Decision::AutoAccept,
                by: None,
            });
        }
        if n > 0 {
            info!("auto-accepted {n} queued request(s)");
        }
        n
    }

    /// 顶栏角标：自动接受期间必须始终可见，剩多久也要写出来
    async fn push_badge(&self) {
        let text = {
            let g = self.inner.lock().await;
            match g.auto_until {
                Some(until) if Instant::now() < until => {
                    let left = (until - Instant::now()).as_secs();
                    if left >= 60 {
                        format!("AUTO {}m", left / 60 + 1)
                    } else {
                        format!("AUTO {left}s")
                    }
                }
                _ => String::new(),
            }
        };
        self.shared.send(HostMsg::Disp(DispOp::Badge { text })).await;
    }

    /// 后台任务：刷新角标剩余时间，到期自动清掉并广播
    pub async fn badge_ticker(self) {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        let mut was_active = false;
        loop {
            tick.tick().await;
            let active = self.inner.lock().await.auto_active();
            if active || was_active {
                self.push_badge().await;
                if was_active && !active {
                    info!("auto-accept expired");
                    self.shared.send(HostMsg::msg("auto expired", "white")).await;
                    let _ = self.events.send(HubEvent::Auto { mode: "off", remaining_s: 0 });
                }
            }
            was_active = active;
        }
    }

    /// 由 API 直接裁决（无硬件时可用，也方便自动化测试）
    pub async fn decide(&self, decision: Decision) -> Option<u64> {
        let id = { self.inner.lock().await.active.as_ref().map(|r| r.id) }?;
        self.finish(decision, None).await;
        Some(id)
    }

    /// 超时清理：只有当前 active 仍是该请求时才动它
    async fn expire(&self, id: u64) {
        {
            let mut g = self.inner.lock().await;
            let is_active = g.active.as_ref().map(|r| r.id) == Some(id);
            if is_active {
                g.active = None;
                warn!("request #{id} timed out");
            } else {
                g.queue.retain(|r| r.id != id);
            }
            if g.active.is_none() {
                g.active = g.queue.pop_front();
            }
        }
        let _ = self.events.send(HubEvent::Decision { id, decision: Decision::Timeout, by: None });
        // 必须显式告诉设备这条请求没了。只发一个"收屏"不够：设备那边还留着
        // 请求态（id、是否高危、滚动位置），下一次按 A/B 会被当成滚动而不是翻页。
        // 实测撞到过：超时后屏幕回了首屏，但设备仍以为有请求在等
        self.shared.send(HostMsg::RequestDone { id, verdict: "timeout" }).await;
        self.present().await;
    }

    pub async fn pending(&self) -> (Option<RequestView>, usize) {
        let g = self.inner.lock().await;
        (g.active.as_ref().map(Request::view), g.queue.len())
    }

    pub async fn auto_view(&self) -> AutoView {
        let g = self.inner.lock().await;
        match g.auto_until {
            Some(until) if Instant::now() < until => {
                AutoView { mode: "accept", remaining_s: (until - Instant::now()).as_secs() }
            }
            _ => AutoView { mode: "off", remaining_s: 0 },
        }
    }

    pub async fn clear_auto(&self) {
        self.inner.lock().await.auto_until = None;
        self.push_badge().await;
        let _ = self.events.send(HubEvent::Auto { mode: "off", remaining_s: 0 });
    }

    /// 取消当前请求（以及队列里全部）
    pub async fn cancel_all(&self) -> usize {
        let mut drained = Vec::new();
        {
            let mut g = self.inner.lock().await;
            if let Some(r) = g.active.take() {
                drained.push(r);
            }
            while let Some(r) = g.queue.pop_front() {
                drained.push(r);
            }
        }
        let n = drained.len();
        for req in drained {
            let id = req.id;
            let _ = req.reply.send((Decision::Cancelled, None));
            let _ = self
                .events
                .send(HubEvent::Decision { id, decision: Decision::Cancelled, by: None });
            self.shared.send(HostMsg::RequestDone { id, verdict: "cancelled" }).await;
        }
        self.present().await;
        n
    }
}

/// 裁决的短名，发给设备让它自己决定怎么显示。
/// hub 不再拼 "ACCEPTED" 这种字面量——那是屏幕文案，属于设备
fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Accept => "accept",
        Decision::Reject => "reject",
        Decision::AutoAccept => "auto_accept",
        Decision::RuleAllow => "rule_allow",
        Decision::Timeout => "timeout",
        Decision::Cancelled => "cancelled",
    }
}

/// 屏幕一行装不下就截断，末尾留个省略号提示还有内容
fn trim(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('~');
    out
}

