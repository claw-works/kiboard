#!/usr/bin/env python3
"""假设备：连到 hub 的 /device 端点，按脚本发语义裁决。

用途是在没有实体键盘、或不想反复插拔时验证 hub 的审批逻辑。
它照着真固件的协议说话——**语义裁决，不是按键**：

    设备 -> hub   hello(caps) / decision / query / pong
    hub  -> 设备  request / request_done / led / disp

键位映射和屏幕排版都在真固件里，hub 不认识"第几号键"，所以这个脚本也不发按键。
它按丝印标签接受参数只是为了顺手，内部自己翻成语义。

用法：
    uvx --from websockets python tests/fake_device.py \\
        'ws://127.0.0.1:26041/device?token=kiboard-dev-token' --press 1 --hold 1.0

    --press 1     接受        --press 2  拒绝      --press 3  全部接受
    --press C     取消全部    --press D  关自动
    --press 0/5/6 查询屏（info / recent / last）
    --hold        按住多久（秒）。高危请求的阈值由 hub 定，默认 900ms
"""

from __future__ import annotations

import argparse
import asyncio
import json
import time

import websockets

# 丝印标签 -> 语义。A/B/4/* 不在这里：那几个键真固件本地就处理完了，从不发给 hub
VERDICTS = {"1": "accept", "2": "reject", "3": "accept_window", "C": "cancel_all", "D": "clear_auto"}
QUERIES = {"0": "info", "5": "recent", "6": "last"}


def wrapped_lines(text: str, max_w: int) -> int:
    """按固件的折行规则数总行数：ASCII 6px、汉字 12px，超宽换行，\\n 强制换行。

    真固件是把折行结果画出来并回报行数，这里只是模拟——留着是为了让 hub 的日志里
    有个可比对的数字，也提醒读代码的人：**排版是设备的事**。
    """
    lines, x = 1, 0
    for ch in text:
        if ch == "\n":
            lines, x = lines + 1, 0
            continue
        w = 6 if ord(ch) < 0x80 else 12
        if x + w > max_w:
            lines, x = lines + 1, 0
        x += w
    return lines


class FakeDevice:
    def __init__(self, ws) -> None:
        self.ws = ws
        self.req_id: int | None = None
        self.req_high = False
        self.hold_ms = 0

    async def hello(self) -> None:
        await self.ws.send(
            json.dumps(
                {
                    "t": "hello",
                    "fw": "fake-0.5.0",
                    "keys": 16,
                    "leds": 3,
                    "disp": "ssd1306-128x64",
                    "ip": "0.0.0.0",
                    # 如实声明：这个假设备也自己排版（其实是不排，但它不要 hub 代劳）
                    "caps": {
                        "render": "self",
                        "input": "matrix16",
                        "confirm": ["tap", "hold"],
                        "confirm_verifiable": True,
                    },
                }
            )
        )

    async def act(self, label: str, hold_s: float) -> None:
        """按一次键：翻成语义发出去。高危时带上原始 press/release 时间戳"""
        if label in QUERIES:
            await self.ws.send(json.dumps({"t": "query", "what": QUERIES[label]}))
            print(f"[fake] query {QUERIES[label]}")
            return
        verdict = VERDICTS.get(label)
        if verdict is None:
            raise SystemExit(
                f"{label} 不发给 hub：A/B/4/* 由固件本地处理。可用：{' '.join(VERDICTS)} {' '.join(QUERIES)}"
            )

        msg: dict[str, object] = {"t": "decision", "verdict": verdict}
        if self.req_id is not None:
            msg["id"] = self.req_id
        if verdict == "accept" and self.req_high:
            # 报原始事件，让 hub 自己算时长——判定权在 hub，不在设备
            press_ts = int(time.monotonic() * 1000)
            await asyncio.sleep(hold_s)
            release_ts = int(time.monotonic() * 1000)
            msg["confirm"] = {
                "method": "hold",
                "events": [
                    {"ev": "press", "device_ts": press_ts},
                    {"ev": "release", "device_ts": release_ts},
                ],
            }
            print(f"[fake] {verdict} held {release_ts - press_ts}ms (hub 要 {self.hold_ms}ms)")
        else:
            print(f"[fake] {verdict} for #{self.req_id}")
        await self.ws.send(json.dumps(msg))

    async def reader(self) -> None:
        async for raw in self.ws:
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                print(f"[hub->] {raw}")
                continue
            t = msg.get("t")
            if t == "ping":
                await self.ws.send(json.dumps({"t": "pong", "uptime_ms": 1234}))
                continue
            if t == "request":
                self.req_id = msg.get("id")
                self.req_high = msg.get("risk") == "high"
                self.hold_ms = msg.get("hold_ms", 0)
                total = wrapped_lines(msg.get("verbatim", ""), 119)
                await self.ws.send(json.dumps({"t": "disp", "op": "request", "lines": total}))
                print(
                    f"[fake] request #{self.req_id} risk={msg.get('risk')} "
                    f"hold_ms={self.hold_ms} verbatim={msg.get('verbatim')!r} "
                    f"cwd={msg.get('cwd')!r} summary={msg.get('summary')!r}"
                )
                continue
            if t == "request_done":
                print(f"[fake] request #{msg.get('id')} done: {msg.get('verdict')}")
                self.req_id = None
                self.req_high = False
                continue
            print(f"[hub->] {raw}")


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("--press", action="append", default=[], help="要按的键，可重复")
    ap.add_argument("--hold", type=float, default=0.1, help="按住时长秒（只对高危的接受有意义）")
    ap.add_argument("--after", type=float, default=1.0, help="连上后等几秒开始按")
    ap.add_argument("--listen", type=float, default=3.0, help="按完再监听几秒")
    args = ap.parse_args()

    async with websockets.connect(args.url) as ws:
        print("[fake] connected")
        dev = FakeDevice(ws)
        await dev.hello()
        task = asyncio.create_task(dev.reader())
        await asyncio.sleep(args.after)
        for label in args.press:
            await dev.act(label, args.hold)
            await asyncio.sleep(0.3)
        await asyncio.sleep(args.listen)
        task.cancel()


if __name__ == "__main__":
    asyncio.run(main())
