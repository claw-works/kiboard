#!/usr/bin/env python3
"""校验 schema 本身合法，并用样例回归它真的拦得住该拦的东西。

存在的理由：一份"看着对但从不拒绝任何东西"的 schema 比没有更糟——它给人一种
契约被强制执行的错觉。所以这里同时喂合法样例和**故意写坏的样例**，
后者一旦通过就是 schema 漏了约束。

用法（jsonschema 不必装到系统里）：
    uvx --from jsonschema python protocol/validate.py

退出码 0 = 通过，1 = 有问题。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

SCHEMA = Path(__file__).resolve().parent / "schema" / "kiboard.schema.json"

# 合法样例：四种消息各一条，字段尽量填满
VALID: list[dict[str, Any]] = [
    {
        "t": "hello",
        "protocol_version": "1.0",
        "device": {"id": "kb-01", "kind": "c3-keypad", "firmware": "v4.2.0"},
        "caps": {
            "render": "self",
            "display": {"kind": "mono", "cols": 21, "rows": 4},
            "input": "matrix16",
            "confirm": ["tap", "hold"],
            "confirm_verifiable": True,
        },
    },
    {
        "t": "request",
        "id": "r_1",
        "verbatim": "rm -rf build",
        "tool": "execute_bash",
        "summary": "清理构建产物",
        "client": {"name": "kiro-cli", "enforcement": "block", "cwd": "/x/kiboard"},
        "risk": "high",
        "confirm_required": "hold",
        "hold_ms": 900,
        "queued": 2,
        "rule": "递归删除",
        "expires_at": "2026-08-03T12:02:00Z",
    },
    {
        "t": "decision",
        "id": "r_1",
        "verdict": "accept",
        "confirm": {
            "method": "hold",
            "events": [
                {"ev": "press", "device_ts": 81234},
                {"ev": "release", "device_ts": 82180},
            ],
        },
    },
    {
        "ok": True,
        "id": "r_1",
        "decision": "accept",
        "approved": True,
        "reason": "held 946ms",
        "risk": "high",
        "rule": "递归删除",
        "decided_by": {
            "principal": "device:kb-01",
            "kind": "c3-keypad",
            "confirm": "hold",
            "verified_by": "hub",
        },
    },
    # 手机方案：生物识别只能自述，asserted 必须为 true
    {
        "t": "decision",
        "id": "r_2",
        "verdict": "accept",
        "confirm": {"method": "biometric", "asserted": True},
    },
    # 队列控制与具体请求无关，可以不带 id
    {"t": "decision", "verdict": "clear_auto"},
    {"t": "decision", "verdict": "cancel_all"},
]

# 故意写坏的样例：(说明, 载荷)。每条都必须被拦下。
INVALID: list[tuple[str, dict[str, Any]]] = [
    ("request 缺 verbatim —— 逐字原文是必显字段，缺了设备就只能显示 summary",
     {"t": "request", "id": "r_2", "tool": "execute_bash", "risk": "high",
      "expires_at": "2026-08-03T12:02:00Z"}),
    ("accept 缺 id —— 没有 id 就无法绑定请求，隔夜的 accept 能落到新请求上",
     {"t": "decision", "verdict": "accept"}),
    ("reject 缺 id —— 同理，裁决必须绑定到具体请求",
     {"t": "decision", "verdict": "reject"}),
    ("hello 的 caps 缺 render —— hub 不知道该不该代为排版",
     {"t": "hello", "protocol_version": "1.0",
      "device": {"id": "d", "kind": "mobile"}, "caps": {}}),
    ("verdict 取值不在枚举内",
     {"t": "decision", "id": "r_1", "verdict": "maybe"}),
    ("risk 取值不在枚举内",
     {"t": "request", "id": "r_3", "verbatim": "ls", "risk": "kinda",
      "expires_at": "2026-08-03T12:02:00Z"}),
    ("protocol_version 不是 major.minor",
     {"t": "hello", "protocol_version": "v1",
      "device": {"id": "d", "kind": "mobile"}, "caps": {"render": "self"}}),
    ("request 混入未定义字段 —— 打错字的字段名会被静默忽略，那是最难查的一类 bug",
     {"t": "request", "id": "r_4", "verbatim": "ls", "risk": "normal",
      "expires_at": "2026-08-03T12:02:00Z", "verbatimm": "typo"}),
]


def main() -> int:
    try:
        import jsonschema
    except ModuleNotFoundError:
        print("需要 jsonschema：uvx --from jsonschema python protocol/validate.py", file=sys.stderr)
        return 1

    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
    validator_cls = jsonschema.Draft202012Validator
    validator_cls.check_schema(schema)
    validator = validator_cls(schema)

    failures = 0
    for i, payload in enumerate(VALID):
        errors = list(validator.iter_errors(payload))
        if errors:
            print(f"✗ 合法样例 {i} 被误拦：{errors[0].message}", file=sys.stderr)
            failures += 1

    for note, payload in INVALID:
        if not list(validator.iter_errors(payload)):
            print(f"✗ 该拦却放过了：{note}", file=sys.stderr)
            failures += 1

    if failures:
        print(f"\n{failures} 项不符合预期。", file=sys.stderr)
        return 1

    print(f"通过：{len(VALID)} 条合法样例全过，{len(INVALID)} 条非法样例全被拦。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
