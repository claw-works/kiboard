#!/usr/bin/env bash
# Kiro CLI 的状态上报钩子：让设备平时也有用，不只是审批时才亮
#
# 用法（在 agent 配置里挂给不同的触发器）：
#   agentSpawn        -> state-hook.sh start
#   userPromptSubmit  -> state-hook.sh working
#   postToolUse       -> state-hook.sh working
#   stop              -> state-hook.sh your_turn
#
# 和审批钩子的区别：这个**永远 exit 0**。
# 审批失败要 fail-closed 阻止操作，而状态上报失败只该被忽略——
# 一个"看看 agent 在干什么"的功能不能变成新的失败模式。
# 注意 agentSpawn 与 userPromptSubmit 的 stdout 会进模型上下文，
# 所以这里绝不往 stdout 写东西，只用 stderr。

set -uo pipefail

STATE="${1:-working}"
# --client 必须显式传：kiboard-ask 的默认是 raw，不传的话设备上会显示成 [raw]
CLIENT="${KIBOARD_CLIENT:-kiro-cli}"

find_ask() {
    if command -v kiboard-ask >/dev/null 2>&1; then
        command -v kiboard-ask
        return 0
    fi
    local c
    for c in "/usr/local/bin/kiboard-ask" "$HOME/.local/bin/kiboard-ask" \
             "$HOME/.cargo/bin/kiboard-ask" "${KIBOARD_ASK:-}"; do
        [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
    done
    return 1
}

ASK="$(find_ask)" || exit 0   # 没装就静默跳过，不打扰 agent

"$ASK" --state "$STATE" --client "$CLIENT" >/dev/null 2>&1 || true
exit 0
