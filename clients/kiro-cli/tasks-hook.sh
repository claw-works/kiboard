#!/usr/bin/env bash
# Kiro CLI 的任务上报钩子：agent 每次改自己的待办清单，设备上就跟着变
#
# 挂法（agent 配置的 hooks 段）：
#   "postToolUse": [
#     {"matcher": "todo_list", "command": ".../tasks-hook.sh", "timeout_ms": 5000,
#      "cache_ttl_seconds": 0}
#   ]
#
# 为什么能这么做：todo_list 就是个普通工具，所以它的调用会走 postToolUse。
# 而 postToolUse 的 tool_response 里带着**整份清单的状态**（不只是这次的改动），
# 所以每次触发都能拿到完整快照，不需要在这边维护任何状态。
#
# 实测出来的三件事（官方文档没写或写反了，都在 clients/kiro-cli/README.md 记着）：
#   1. 工具名是 todo_list，不是 agent 配置里 tools 数组写的那个 todo
#   2. matcher 是【字面量比较，不是正则】。写 "todo_list|execute_bash" 会得到一个
#      静默永不触发的 hook —— 这也是为什么这里必须单独挂一条
#   3. 完整状态在 tool_response.result[0] 里，形如 "TODO LIST STATE: {json}\n\n ID: ..."
#
# 和审批钩子的区别：这个**永远 exit 0**。观测功能不该变成失败模式。
set -uo pipefail

# 任何未预期的中止都不该影响 agent
trap 'exit 0' ERR

find_ask() {
    if command -v kiboard-ask >/dev/null 2>&1; then
        command -v kiboard-ask
        return 0
    fi
    local c
    for c in "/usr/local/bin/kiboard-ask" "${HOME}/.local/bin/kiboard-ask" \
             "${HOME}/.cargo/bin/kiboard-ask" "${KIBOARD_ASK:-}"; do
        [ -n "${c}" ] && [ -x "${c}" ] && { echo "${c}"; return 0; }
    done
    return 1
}

ASK="$(find_ask)" || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# 把 todo_list 的状态转成 kiboard 的任务列表。
#
# 状态映射是这里唯一有判断的地方：todo_list 只有 completed 布尔值，没有"进行中"。
# 但清单是自上而下做的，所以【第一个未完成的就是此刻在做的那件】，
# 其余未完成的是计划。设备上只显示 doing，正好是"此刻在动什么"。
# 脚本必须走 python3 -c：写成 `python3 - <<EOF` 的话 stdin 被脚本本身占用，
# sys.stdin.read() 就读不到 hook 载荷了（这里踩过一次，hook 静默什么都不做）。
PARSE='
import json, re, subprocess, sys

try:
    payload = json.loads(sys.stdin.read())
except ValueError:
    sys.exit(0)

# session 决定 hub 那边怎么分桶，务必传过去
session = str(payload.get("session_id") or "")

result = (payload.get("tool_response") or {}).get("result") or []
text = "\n".join(x for x in result if isinstance(x, str))
m = re.search(r"TODO LIST STATE:\s*(\{.*\})", text, re.S)
if not m:
    sys.exit(0)
try:
    state = json.loads(m.group(1))
except ValueError:
    sys.exit(0)

out = []
first_open = True
for t in state.get("tasks") or []:
    title = (t.get("task_description") or "").strip()
    if not title:
        continue
    if t.get("completed"):
        status = "done"
    elif first_open:
        status = "doing"
        first_open = False
    else:
        status = "todo"
    out.append({"title": title, "status": status})

subprocess.run(
    [sys.argv[1], "--tasks", "--client", "kiro-cli", "--session", session],
    input=json.dumps(out, ensure_ascii=False),
    text=True, timeout=5, check=False,
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
'
python3 -c "$PARSE" "$ASK" || exit 0

exit 0
