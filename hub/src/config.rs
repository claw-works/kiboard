//! 运行配置：全部可用环境变量覆盖
//!
//! hub 要部署到另一台机器上（设备走 WiFi 连过来），所以串口路径、监听地址、
//! token 都不能写死。串口在没有设备直连时打不开是正常的，不影响服务。
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址。绑 0.0.0.0 让设备能从局域网连入，因此 /device 必须校验 token
    pub listen: String,
    /// 设备接入 /device 用的 token（烧在固件里）
    pub token: String,
    /// 调用 HTTP 接口与订阅 /ws 用的密钥。None 表示不校验（仅适合只绑 127.0.0.1 的场景）
    pub api_key: Option<String>,
    /// 串口路径；设为空字符串则完全不启用串口链路
    pub serial_port: String,
    pub baud: u32,
    /// 审批请求的默认超时
    pub approve_timeout: Duration,
    /// 「全部接受」生效多久
    pub auto_accept_ttl: Duration,
    /// 高危请求要按住多久才算接受。由 hub 计时而不是固件——阈值以后在这里改就行，
    /// 不用为一个常量重新烧板子
    pub high_hold: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let listen = env_or("KIBOARD_LISTEN", "0.0.0.0:26041");
        let token = env_or("KIBOARD_TOKEN", "kiboard-dev-token");
        // 故意不给默认值：默认的 API key 等于没有 API key
        let api_key = std::env::var("KIBOARD_API_KEY").ok().filter(|s| !s.is_empty());
        // 默认值按 macOS 开发机给；Linux 上通常是 /dev/ttyACM0，用环境变量覆盖。
        // 设为 "off" 或空则不启用串口。
        let serial_port = env_or("KIBOARD_SERIAL", "/dev/cu.usbmodem*");
        let baud = env_or("KIBOARD_BAUD", "115200").parse().unwrap_or(115_200);
        let approve_timeout =
            Duration::from_secs(env_or("KIBOARD_APPROVE_TIMEOUT_S", "120").parse().unwrap_or(120));
        let auto_accept_ttl =
            Duration::from_secs(env_or("KIBOARD_AUTO_ACCEPT_TTL_S", "600").parse().unwrap_or(600));

        // 定这个值是在两件事之间取平衡：
        //   太短 —— 和"点一下"区分不开，长按保护形同虚设。实测有人以为在短按，
        //           系统按固件 600ms 的阈值判成了长按并批准
        //   太长 —— 每次批准都要干等，实际用起来嫌烦（1200ms 用户反馈偏长）
        // 900ms 明显超出手指点按的范围（80~150ms，刻意按稳也很少到 700ms），
        // 又不至于让人觉得在罚站。到点时黄灯转常亮，有明确的"可以松手"信号，
        // 有反馈的等待比无反馈的短等待更好受。
        let high_hold_ms: u64 = env_or("KIBOARD_HIGH_HOLD_MS", "900").parse().unwrap_or(900);
        if high_hold_ms < 600 {
            // 低于 600ms 就落进了"刻意按稳的点按"区间，防手滑的作用会消失
            tracing::warn!(
                "KIBOARD_HIGH_HOLD_MS={high_hold_ms}ms 偏低：低于 600ms 就和点按区分不开，\
                 高危请求的长按保护会形同虚设"
            );
        }
        let high_hold = Duration::from_millis(high_hold_ms);

        Self {
            listen,
            token,
            api_key,
            serial_port,
            baud,
            approve_timeout,
            auto_accept_ttl,
            high_hold,
        }
    }

    pub fn serial_enabled(&self) -> bool {
        !self.serial_port.is_empty() && self.serial_port != "off"
    }

    /// 绑在非本机地址却没有 API key，就是把接口暴露给整个局域网（可能还有公网）
    pub fn exposed_without_api_key(&self) -> bool {
        self.api_key.is_none() && !self.listen.starts_with("127.0.0.1")
            && !self.listen.starts_with("localhost")
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
