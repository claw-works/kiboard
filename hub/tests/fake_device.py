#!/usr/bin/env python3
"""假设备：连到 hub 的 /device 端点，按脚本发按键事件。

用途是在没有实体键盘、或不想反复插拔时验证 hub 的审批逻辑。
它照着真固件的协议说话：上报 hello / key / pong，接收并打印 hub 下发的指令。

用法：
    python3 tests/fake_device.py ws://127.0.0.1:26044/device?token=kiboard-dev-token \\
        --press 1 --after 1.0
    # --press 用丝印标签（1/2/3/A/.../D），--after 是连上后等几秒再按
"""

from __future__ import annotations

import argparse
import asyncio
import json

import websockets

# 丝印标签 -> 键 id，与 hub/src/keymap.rs 的 LABELS 保持一致
LABELS = ["1", "2", "3", "A", "4", "5", "6", "B", "7", "8", "9", "C", "*", "0", "#", "D"]


def key_id(label: str) -> int:
    if label not in LABELS:
        raise SystemExit(f"未知按键标签: {label}，可用: {' '.join(LABELS)}")
    return LABELS.index(label)


def wrapped_lines(text: str, max_w: int) -> int:
    """按固件的折行规则数总行数：ASCII 6px、汉字 12px，超宽换行，\n 强制换行。"""
    lines = 1
    x = 0
    for ch in text:
        if ch == "\n":
            lines += 1
            x = 0
            continue
        w = 6 if ord(ch) < 0x80 else 12
        if x + w > max_w:
            lines += 1
            x = 0
        x += w
    return lines


async def press(ws, label: str, hold_s: float) -> None:
    """模拟一次按压：press -> (超过 600ms 则 long) -> release，与固件一致"""
    kid = key_id(label)
    row, col = kid // 4 + 1, kid % 4 + 1
    base = {"t": "key", "id": kid, "row": row, "col": col}
    await ws.send(json.dumps({**base, "act": "press"}))
    print(f"[fake] press {label} (id={kid})")
    if hold_s >= 0.6:
        await asyncio.sleep(0.6)
        await ws.send(json.dumps({**base, "act": "long"}))
        print(f"[fake] long {label}")
        await asyncio.sleep(hold_s - 0.6)
    else:
        await asyncio.sleep(hold_s)
    await ws.send(json.dumps({**base, "act": "release"}))
    print(f"[fake] release {label}")


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("--press", action="append", default=[], help="要按的键，可重复")
    ap.add_argument(
        "--hold",
        type=float,
        default=0.1,
        help="按住时长秒。>=0.6 会像固件那样先发 long；高危请求的实际阈值由 hub 定（默认 1.2s）",
    )
    ap.add_argument("--after", type=float, default=1.0, help="连上后等几秒开始按")
    ap.add_argument("--listen", type=float, default=3.0, help="按完再监听几秒")
    args = ap.parse_args()

    async with websockets.connect(args.url) as ws:
        print("[fake] connected")
        await ws.send(
            json.dumps(
                {
                    "t": "hello",
                    "fw": "fake-0.4.0",
                    "keys": 16,
                    "leds": 3,
                    "disp": "ssd1306-128x64",
                    "ip": "0.0.0.0",
                }
            )
        )

        async def reader() -> None:
            async for raw in ws:
                try:
                    msg = json.loads(raw)
                except json.JSONDecodeError:
                    print(f"[hub->] {raw}")
                    continue
                # 像真固件那样回 pong，否则 hub 会以为设备掉了
                if msg.get("t") == "ping":
                    await ws.send(json.dumps({"t": "pong", "uptime_ms": 1234}))
                    continue
                # status 的回执要带折行总行数，hub 靠它夹滚动范围。
                # 这里按固件的度量粗算：一行 128-2-7 像素，ASCII 6px、汉字 12px
                if msg.get("t") == "disp" and msg.get("op") == "status":
                    total = wrapped_lines(msg.get("text", ""), 119)
                    await ws.send(
                        json.dumps({"t": "disp", "op": "status", "lines": total})
                    )
                    print(
                        f"[fake] status skip={msg.get('skip', 0)} lines={total} "
                        f"mode={msg.get('mode')!r} text={msg.get('text')!r}"
                    )
                    continue
                print(f"[hub->] {raw}")

        task = asyncio.create_task(reader())
        await asyncio.sleep(args.after)
        for label in args.press:
            await press(ws, label, args.hold)
            await asyncio.sleep(0.3)
        await asyncio.sleep(args.listen)
        task.cancel()


if __name__ == "__main__":
    asyncio.run(main())
