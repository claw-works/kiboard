#!/usr/bin/env bash
# kiboard 审批闸门 —— Claude Code PreToolUse hook
#
# ⚠️ 未在真实 Claude Code 上实测（本机没装）。载荷字段与决策通道依官方文档和
#    claude-code 仓库 issue 编写，逻辑与已实测的 kiro-cli 适配器共用同一个
#    kiboard-ask 二进制。首次使用请按 README 的"上线前自检"跑一遍。
#
# 契约：stdin 收 PreToolUse JSON，exit 0 放行、exit 2 拦下（stderr 进模型上下文）。
# 放行时是否额外输出 permissionDecision=allow 由 KIBOARD_CC_DECISION 决定，见 README。
set -uo pipefail

# 注意 bash 的坑：'$VAR' 后面紧跟中文标点时，非 ASCII 字节会被算进变量名，
# set -u 下直接中止。凡是变量后面跟中文，一律写 ${VAR}。

ASK="${KIBOARD_ASK_BIN:-$HOME/.local/bin/kiboard-ask}"

if [[ ! -x "$ASK" ]]; then
    # 找不到闸门程序时的取舍：这里选择**放行并告警**，而不是 fail-closed 把
    # Claude Code 卡死。理由是"装了 hook 但没装 kiboard-ask"是配置错误，
    # 不是攻击场景；把它变成硬失败会让人在不相干的地方排查半天。
    # 真正的 fail-closed 在 kiboard-ask 内部（联不上 hub 才是安全事件）。
    echo "kiboard: 找不到 ${ASK}，本次未经审批放行。装好或设 KIBOARD_ASK_BIN。" >&2
    exit 0
fi

exec "$ASK" --client claude-code
