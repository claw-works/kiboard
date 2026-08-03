# 方案 A：ESP32-C3 矩阵键盘

ESP32-C3 SuperMini + 0.96" SSD1306 OLED（I2C）+ 4×4 矩阵键盘一体板 + 三颗状态 LED。

**这是保证最强的方案**：固件是哑终端，没有通用计算能力，可以只走 USB 串口不联网，
agent 完全触碰不到它。代价是显示面积小、只能显示纯文本、不能离席使用。

```
firmware/   PlatformIO + Arduino C++ 固件，含 tools/ 下的硬件探测环境
hardware/   KiCad 载板设计、BOM、机械尺寸、面包板记录、netlist 核对脚本
docs/       引脚分配（pinmap.md）与选型/踩坑记录（hardware.md）
```

固件与硬件放一起是刻意的：改引脚必须同时动原理图和固件，分开放会分叉——
v3 原理图和实测固件就分叉过一次，`hardware/check_netlist.py` 是为此加的。

## 引脚速查（v4，面包板实测通过）

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

完整接线、分配依据和推导见 **[docs/pinmap.md](docs/pinmap.md)** 与 **[docs/hardware.md](docs/hardware.md)**。

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

按键 id = `(行-1)*4 + (列-1)`，行 1→4 为上→下，列 1→4 为左→右。

## 键位映射在固件，不在 hub

固件把按键翻成语义再上报：hub 收到的是 `{"t":"decision","verdict":"accept"}`，
不是"第 0 号键被按下"。理由是键位由这块板子的形状决定，而 hub 要同时服务
触摸屏和手机——那些方案上根本没有"键号"这个概念。

分工仍然按**数据在谁手里**：

| 键 | 处理方 | 消息 |
|---|---|---|
| 1 / 2 / 3 | 固件翻译后交 hub | `decision: accept / reject / accept_window` |
| C / D | 固件翻译后交 hub | `decision: cancel_all / clear_auto` |
| 5 / 6 / 0 | 问 hub 要数据 | `query: recent / last / info` |
| A / B | **全在固件** | 审批界面上是正文滚动，待机时是首屏翻页 |
| 4 / * | **全在固件** | 任务屏 / 退一层 |
| 7 8 9 # | — | 未绑定，只上报 key 事件供诊断 |

审批界面的**排版也在固件**：hub 只下发 `{"t":"request","verbatim":...,"risk":...}` 这样的
字段，怎么折行、滚到第几行是这块屏才知道的事。

## 强确认

高危请求要求**按住 `1` 达 900ms**（hub 的 `KIBOARD_HIGH_HOLD_MS`），且不被「全部接受」放行。

分工是刻意的：**阈值和判定在 hub，过程反馈在固件**。

- hub 把 `hold_ms` 写在 `request` 里下发。固件据此本地做反馈：到点黄灯转常亮 +
  屏幕提示 `release to accept`。不用等网络往返，人也不必靠猜按够了没有
- 松手时固件把**原始 press/release 时间戳**随 `decision` 报上去，由 hub 算真实时长并复核。
  设备不报"我确认过了"这个结论——那样阈值就散落到各设备实现里了

为什么不信固件的 `long` 事件：它在 600ms 就触发，而 600ms 对人手区分不开"点一下"和
"按住"。实测有人想短按却按到了 600ms 以上，系统判成长按并批准了。阈值放 hub 侧还有个
好处：改配置就生效，不用为一个常量重新烧板。

**注意上报时长有个下限**：矩阵去抖的事件冷却是 `EVENT_COOLDOWN_MS = 250`，所以一次快点
也会报成约 250ms（实测确认）。阈值定在这个量级以下就没有意义，hub 在低于 600ms 时会告警。

## 链路

设备同时支持两条链路，固件优选无线、串口兜底（只走一条，避免 hub 收到重复消息）：

| 链路 | 用途 | 说明 |
|---|---|---|
| USB 串口 | 开发调试 | 能刷固件、看 ESP-IDF 日志 |
| WiFi WebSocket | 日常使用 | 设备连 `ws://<hub>:26041/device?token=...` |

**在公司管控的机器上优先走串口 + 本机 hub**，原因见根 README 的 EDR 一节。

Wi-Fi 凭据与 hub 地址放 `firmware/src/secrets.h`（不进 git，见 `secrets.h.example`
的 `HUB_HOST` / `HUB_PORT` / `HUB_WS_PATH`）。

## 开发

```bash
cd devices/c3-keypad/firmware
pio run -e supermini -t upload      # 刷正式固件

# 硬件探测（各自独立的环境，不含正式固件）
pio run -e oledprobe   -t upload    # I2C 扫描 + 屏幕测试图
pio run -e matrixprobe -t upload    # 矩阵键盘，串口打印 R?C? + 屏上点阵
pio run -e ledprobe    -t upload    # LED 接线与极性
pio run -e matrixdiag  -t upload    # 引脚电平诊断
```

`pio device monitor` 需要真 tty，在无终端环境下会报 `termios.error(102)`；
这种情况直接用 PlatformIO 自带的 python + pyserial 读串口。

hub 运行时占用串口，刷固件前先 `pkill -f kiboard-hub`。

## 打板前的核对

```bash
export PATH="/Applications/KiCad/KiCad.app/Contents/MacOS:$PATH"
kicad-cli sch export netlist --format kicadxml -o /tmp/v4.xml \
    hardware/kiboard/kiboard.kicad_sch
python3 hardware/check_netlist.py /tmp/v4.xml    # 退出码非 0 就是原理图和固件分叉了
```

ERC 查不出 v3 那个 bug——它不知道 SuperMini 的 GPIO2/8/9 带板载上拉。
"原理图自洽但和现实不符"只能靠和**实测通过的固件**对比来抓。

## 已知坑

- **GPIO2/8/9 带板载上拉，只能当输出。** 当矩阵输入时节点被顶在 2.3~2.7V 的不确定带里，
  不同引脚各判各的，症状是"某一排只有部分键出错"，极易误判成键位映射问题
- SuperMini 天线缺陷：Wi-Fi 必须 `setTxPower(WIFI_POWER_8_5dBm)`，否则 WPA2 握手失败（reason 2/202）
- C3 原生 USB 是 Serial/JTAG，**不能做 USB HID 键盘**，所以走串口 + 主机侧服务的架构
- C3 的 strapping 脚是 GPIO2/8/9，**不含 GPIO0**（GPIO0 是 ESP32 经典款的 boot 脚）
- 矩阵扫描时非扫描行要设 `INPUT_PULLDOWN`：纯 `INPUT` 会悬空为高，`OUTPUT` 拉低会在同列双键时短路
- 改接线务必断电，别热插拔
