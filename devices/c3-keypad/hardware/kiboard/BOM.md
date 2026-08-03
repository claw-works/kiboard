# kiboard 载板 BOM（原理图 v4：OLED I2C + 矩阵键盘版）

原理图文件：`kiboard.kicad_sch`（KiCad 10，用 Konnect MCP 生成并校验，ERC 0 error / 1 warning）。
引脚以 `../../docs/pinmap.md` 的 **v4** 为准，v4 全部经面包板实测。
这是一块**载板**——ESP32-C3 SuperMini 以模组形式通过排母插入，载板不焊裸芯片。

## 元件清单

| 标号 | 类型 | 值/规格 | 封装 | 数量 | 说明 |
|---|---|---|---|---|---|
| J1 | 排母 1×8 | SuperMini_Left | PinHeader_1x08_P2.54mm_Vertical | 1 | 对应 SuperMini 左侧 8 脚：5V/GND/3V3/GPIO4~0 |
| J2 | 排母 1×8 | SuperMini_Right | PinHeader_1x08_P2.54mm_Vertical | 1 | 右侧 8 脚：GPIO5~7/8/9/10/20/21（GPIO8 悬空，它是板载蓝灯） |
| J3 | 排母 1×2 | OLED_I2C | PinHeader_1x02_P2.54mm_Vertical | 1 | OLED 屏 I2C 接口：SDA, SCL（VCC/GND 另接） |
| J4 | 排母 1×8 | Matrix_Keypad_4x4 | PinHeader_1x08_P2.54mm_Vertical | 1 | 16 键矩阵：R1-4/C1-4 |
| R1 | 电阻 | 330Ω | R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal | 1 | LED0（黄，需批准）限流，**接 3V3 一侧**（灌电流接法） |
| R2 | 电阻 | 330Ω | R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal | 1 | LED1（红，出错）限流，接 GPIO2 一侧（拉电流接法） |
| D1 | LED | 黄色 5mm | LED_D5.0mm | 1 | 需批准指示灯。**K 阴极接 GPIO9，A 阳极经 R1 接 3V3 → 低电平点亮** |
| D2 | LED | 红色 5mm | LED_D5.0mm | 1 | 出错指示灯。A 阳极经 R2 接 GPIO2，K 阴极接 GND → 高电平点亮 |

SuperMini 模组、OLED+矩阵键盘一体板均为外购成品，通过排针/排母连接，不计入载板 BOM。

## 网络对照

见 `../../docs/pinmap.md`，含矩阵扫描原理和 GPIO9 strapping 脚说明。

## 装配说明

1. J1/J2 是 SuperMini 模组插座，用两条 1×8 排母
2. J3 只接 SDA/SCL 两根信号线；OLED 模块的 VCC/GND 直接接 SuperMini 的 3V3/GND（不经过载板走线，或在 J3 旁另加 2 pin 电源直连）
3. J4 引脚顺序：接线**以一体板自己的丝印为准**。
   采购图 `../bom/00.webp` 上标签是 `R4 R3 R2 R1 | C4 C3 C2 C1`，
   看着和本文档假设的 `R1..R4 | C1..C4` 相反，但那是**从正面拍的**，
   左右方向和俯视接线时相反。板子上每个引脚都有丝印文字，照字接不会错。
4. LED 引脚号：KiCad `Device:LED` 的**引脚 1 = K 阴极，引脚 2 = A 阳极**（和直觉相反，
   别凭经验假设，用 Konnect 查实际引脚）
5. **两颗 LED 的接法故意不同，不是笔误**：
   - D1（LED0，GPIO9）是**灌电流**：3V3 → R1 → A 阳极 → K 阴极 → GPIO9，GPIO 输出低才亮
   - D2（LED1，GPIO2）是**拉电流**：GPIO2 → R2 → A 阳极 → K 阴极 → GND，GPIO 输出高才亮

   原因是 GPIO9 是 boot 模式选择脚。若按拉电流接（GPIO9 → R1 → LED → GND），
   这条 DC 通路会把 GPIO9 压到约 1.7V，落进 VIL(0.83V)/VIH(2.48V) 之间的不确定带，
   可能被判成低而进入下载模式，表现为**时好时坏的启动失败**。灌电流接法下
   GPIO9 被 R1 拉向 3V3，稳定为高。代价是固件里 LED0 逻辑取反（已实现）。
   完整推导见 `../../docs/pinmap.md` 的「LED0 为什么必须反接」。

   GPIO2 没这个问题：它作**输出**时板载上拉无害，上电即为高对 strapping 反而有利，
   而且拉电流接法省掉一条到 R2 的 3V3 走线。
6. 两层板，无 RF/高速信号，标准 1.6mm 板厚、6/6mil 线宽间距足够（JLCPCB 默认工艺）

## 生成方式

本原理图完全通过 Konnect MCP 工具生成（非手写 S-expression），流程：
`add_schematic_component` 放置 → `batch_get_schematic_pin_locations` 核实真实引脚坐标
→ `batch_connect_to_net` / `connect_pins` / `connect_to_net` 接线 → `validate_wire_connections`
+ `validate_component_connections` 自检 → `kicad-cli sch erc` 终验。

## ERC 验证

```
kicad-cli sch erc --severity-all kiboard.kicad_sch
```

预期：**0 error，1 个 warning**——`5V` 网络仅在 J1 单点连接（板子靠 USB 供电，
5V 引脚只做引出、无人消费），属预期提示。

v3 有 2 个 warning，v4 少一个：3V3 原来也是孤立的，现在 R1 要用它，自然接上了。

### 和固件交叉校验

ERC 只查电气规则，它不知道 SuperMini 的 GPIO2/8/9 带板载上拉——**v3 原理图 ERC 全过，
但照着打板会重现"GPIO2 当行线读不出按键"的硬件 bug**。所以另加一道对照，
拿 netlist 去比**已经实测通过的固件源码**：

```
kicad-cli sch export netlist --format kicadxml -o /tmp/v4.xml kiboard.kicad_sch
python3 ../check_netlist.py /tmp/v4.xml
```

逐条检查矩阵 8 根线、3 个 LED 的极性与有效电平、I2C 两根线是否和
`../../firmware/src/main.cpp` 里的 `ROW_PINS` / `COL_PINS` / `leds[]` 一致。退出码非 0 就是分叉了。
