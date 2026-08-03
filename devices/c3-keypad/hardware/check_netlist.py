#!/usr/bin/env python3
"""拿 KiCad 导出的 netlist 去对固件源码，防止原理图和实际跑着的代码悄悄分叉。

存在的理由：v3 那版原理图看起来没问题、ERC 也过，但照着打板会重现
"GPIO2 当行线读不出来"的硬件 bug —— 因为 ERC 只查电气规则，
不知道 SuperMini 的 GPIO2/8/9 带板载上拉。这类"原理图自洽但和现实不符"
的错误只能靠和**实测通过的固件**对比来抓。

用法（在 devices/c3-keypad/ 下执行）：
    export PATH="/Applications/KiCad/KiCad.app/Contents/MacOS:$PATH"
    kicad-cli sch export netlist --format kicadxml -o /tmp/v4.xml \
        hardware/kiboard/kiboard.kicad_sch
    python3 hardware/check_netlist.py /tmp/v4.xml

退出码 0 = 一致，1 = 有分叉。
"""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIRMWARE = ROOT / "firmware" / "src" / "main.cpp"

# SuperMini 排针的物理引脚顺序，依 bom/01.webp（官方引脚图）。
# 改板子才需要改这里。
J1_PINS: dict[int, str | int] = {1: "5V", 2: "GND", 3: "3V3", 4: 0, 5: 1, 6: 2, 7: 3, 8: 4}
J2_PINS: dict[int, int] = {1: 5, 2: 6, 3: 7, 4: 8, 5: 9, 6: 10, 7: 20, 8: 21}

# 键盘排线：J4 的 1..4 是行、5..8 是列，以一体板丝印为准。
# （采购图 00.webp 上看着是反的，那是正面视角；板上每脚都有丝印文字，照字接不会错。）
LED_SYMBOL = {0: "D1", 1: "D2"}
ONBOARD_LED_GPIO = 8  # 板载蓝灯，不该出现在原理图里


class Netlist:
    def __init__(self, path: Path) -> None:
        self.nets: dict[str, list[tuple[str, str]]] = {}
        for net in ET.parse(path).getroot().find("nets"):  # type: ignore[union-attr]
            name = net.get("name", "").lstrip("/")
            self.nets[name] = [(n.get("ref", ""), n.get("pin", "")) for n in net.findall("node")]

    def of_gpio(self, gpio: int) -> tuple[str | None, list[tuple[str, str]]]:
        """找到接在某个 MCU 引脚上的网络"""
        for name, nodes in self.nets.items():
            for ref, pin in nodes:
                table = J1_PINS if ref == "J1" else J2_PINS if ref == "J2" else None
                if table and table.get(int(pin)) == gpio:
                    return name, nodes
        return None, []

    def net_with(self, ref: str, pin: str) -> str | None:
        for name, nodes in self.nets.items():
            if (ref, pin) in nodes:
                return name
        return None


def read_firmware() -> tuple[list[int], list[int], list[tuple[int, bool]]]:
    src = FIRMWARE.read_text()

    def pin_array(name: str) -> list[int]:
        m = re.search(rf"{name}\[4\] = \{{([^}}]+)\}}", src)
        if not m:
            raise SystemExit(f"固件里找不到 {name}")
        return [int(v) for v in m.group(1).split(",")]

    leds = [(int(g), flag == "true") for g, flag in re.findall(r"\{(\d+), (true|false), \d+, \d+\}", src)]
    if not leds:
        raise SystemExit("固件里找不到 leds[] 定义")
    return pin_array("ROW_PINS"), pin_array("COL_PINS"), leds


def main() -> int:
    xml_path = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/v4.xml")
    if not xml_path.exists():
        raise SystemExit(f"没有 {xml_path}，先用 kicad-cli 导出 netlist（见本文件开头注释）")

    nl = Netlist(xml_path)
    rows, cols, leds = read_firmware()
    fails: list[str] = []

    def check(cond: bool, label: str, detail: str) -> None:
        print(f"  {'OK  ' if cond else 'FAIL'} {label:34s} {detail}")
        if not cond:
            fails.append(label)

    print("矩阵行线（固件 ROW_PINS）")
    for i, gpio in enumerate(rows):
        name, nodes = nl.of_gpio(gpio)
        got = [f"{r}.{p}" for r, p in nodes if r == "J4"]
        check(got == [f"J4.{i + 1}"], f"R{i + 1} = GPIO{gpio}", f"{name} -> {got}")

    print("矩阵列线（固件 COL_PINS）")
    for i, gpio in enumerate(cols):
        name, nodes = nl.of_gpio(gpio)
        got = [f"{r}.{p}" for r, p in nodes if r == "J4"]
        check(got == [f"J4.{i + 5}"], f"C{i + 1} = GPIO{gpio}", f"{name} -> {got}")

    print("LED 极性（activeLow 决定 GPIO 该接阴极还是接电阻）")
    for idx, (gpio, active_low) in enumerate(leds):
        if gpio == ONBOARD_LED_GPIO:
            name, _ = nl.of_gpio(gpio)
            # 悬空脚的网络名是 kicad 自动生成的 unconnected-(...)
            unconnected = name is None or name.startswith("unconnected-")
            check(unconnected, f"LED{idx} = GPIO{gpio} 板载灯", f"原理图不应接出：{name}")
            continue

        sym = LED_SYMBOL[idx]
        name, nodes = nl.of_gpio(gpio)
        if active_low:
            # 灌电流：GPIO 直接接阴极 pin1(K)，阳极经电阻上 3V3
            check((sym, "1") in nodes, f"LED{idx} = GPIO{gpio} 接阴极", f"{name} -> {nodes}")
            anode = nl.net_with(sym, "2")
            res = next((r for r, _ in nl.nets.get(anode or "", []) if r.startswith("R")), None)
            top = nl.net_with(res or "", "1")
            check(top == "3V3", f"LED{idx} 阳极经 {res} 上拉", f"{sym}.2 -> {anode} -> {res}.1 -> {top}")
        else:
            # 拉电流：GPIO 经电阻到阳极，阴极下地
            res = next((r for r, _ in nodes if r.startswith("R")), None)
            check(res is not None, f"LED{idx} = GPIO{gpio} 经电阻", f"{name} -> {nodes}")
            check(nl.net_with(sym, "1") == "GND", f"LED{idx} 阴极下地", f"{sym}.1 -> {nl.net_with(sym, '1')}")

    print("I2C")
    for gpio, sig, j3pin in ((4, "SDA", "1"), (5, "SCL", "2")):
        name, nodes = nl.of_gpio(gpio)
        check(("J3", j3pin) in nodes, f"{sig} = GPIO{gpio}", f"{name} -> {nodes}")

    print()
    if fails:
        print(f"原理图与固件不一致，{len(fails)} 项：" + "，".join(fails))
        return 1
    print("原理图与固件一致（矩阵 8 线 + 3 LED 极性 + I2C 全部对上）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
