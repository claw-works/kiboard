"""PlatformIO 构建前把 git 版本注入固件。

和 hub/build.rs 同一个理由：手工维护的版本号迟早漏改一次，而漏改那次
恰好是最需要它的时候——你以为刷了新固件，实际板子上跑的是旧的。

注入 -DKIBOARD_FW_GIT="<describe>"。拿不到 git 时给 "unknown"，不让构建失败。
"""

import subprocess

Import("env")  # noqa: F821  PlatformIO 注入的全局


def git(args: list[str]) -> str | None:
    try:
        out = subprocess.run(
            ["git", *args], capture_output=True, text=True, timeout=5, check=False
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    value = out.stdout.strip()
    return value or None


describe = git(["describe", "--always", "--dirty=+dirty", "--tags"]) or "unknown"
# 提交时间而非编译时间：同一份代码必须给出同一个版本号，
# 否则版本号就没法用来判断"板子上是不是这份代码"
date = git(["log", "-1", "--format=%cd", "--date=format:%m-%d %H:%M"]) or "unknown"

env.Append(  # noqa: F821
    CPPDEFINES=[
        ("KIBOARD_FW_GIT", env.StringifyMacro(describe)),  # noqa: F821
        ("KIBOARD_FW_DATE", env.StringifyMacro(date)),  # noqa: F821
    ]
)
print(f"[version] 固件版本 {describe} ({date})")
