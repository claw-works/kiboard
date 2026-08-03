#!/usr/bin/env bash
# 把 kiboard 闸门装成 Kiro CLI v3 的工作区钩子。
#
# 装完对**该工作区的所有会话**生效，不需要 --agent —— 这是 v3 工作区钩子
# 和 2.x agent 钩子最大的区别（后者只对用那个 agent 起的会话生效）。
#
# 用法：
#   clients/kiro-cli/install-v3-hooks.sh              # 装到当前仓库
#   clients/kiro-cli/install-v3-hooks.sh /path/to/repo
#   clients/kiro-cli/install-v3-hooks.sh --uninstall   # 卸掉
#
# 装之前请确认设备在线：闸门是 fail-closed 的，设备离线时**所有**非白名单命令
# 都会被拦下。逃生阀是 `touch ~/.kiboard/bypass`。
set -uo pipefail

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SELF_DIR}/../.." && pwd)"
INSTALL_DIR="${HOME}/.kiboard/clients/kiro-cli"

target_repo="${REPO_ROOT}"
uninstall=0
for arg in "$@"; do
    case "${arg}" in
        --uninstall) uninstall=1 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) target_repo="${arg}" ;;
    esac
done

hooks_file="${target_repo}/.kiro/hooks/kiboard.json"

if [[ ${uninstall} -eq 1 ]]; then
    if [[ -f "${hooks_file}" ]]; then
        rm -f "${hooks_file}"
        echo "已移除 ${hooks_file}"
        echo "该工作区的会话不再经过 kiboard 审批。"
    else
        echo "没装过：${hooks_file} 不存在"
    fi
    exit 0
fi

# 钩子脚本装到 ~/.kiboard 下而不是直接指向仓库：
# 仓库可能被 checkout 到别的分支或临时删掉，而闸门不该跟着一起消失
mkdir -p "${INSTALL_DIR}"
install -m755 "${SELF_DIR}/hook.sh" "${SELF_DIR}/state-hook.sh" \
    "${SELF_DIR}/tasks-hook.sh" "${INSTALL_DIR}/"

mkdir -p "$(dirname "${hooks_file}")"

# 写绝对路径而不是 $HOME：v3 配置里 command 会不会做变量展开没有文档，不赌
cat > "${hooks_file}" <<EOF
{
  "version": "v1",
  "hooks": [
    {
      "name": "kiboard 审批闸门",
      "trigger": "PreToolUse",
      "matcher": "^(execute_bash|delete_file|orchestrate_subagent|use_subagent)\$",
      "action": { "type": "command", "command": "${INSTALL_DIR}/hook.sh" },
      "timeout": 180
    },
    {
      "name": "kiboard 任务上报",
      "trigger": "PostToolUse",
      "matcher": "^todo_list\$",
      "action": { "type": "command", "command": "${INSTALL_DIR}/tasks-hook.sh" },
      "timeout": 8
    },
    {
      "name": "kiboard 状态-开始",
      "trigger": "SessionStart",
      "action": { "type": "command", "command": "${INSTALL_DIR}/state-hook.sh start" },
      "timeout": 5
    },
    {
      "name": "kiboard 状态-干活",
      "trigger": "UserPromptSubmit",
      "action": { "type": "command", "command": "${INSTALL_DIR}/state-hook.sh working" },
      "timeout": 5
    },
    {
      "name": "kiboard 状态-轮到你",
      "trigger": "Stop",
      "action": { "type": "command", "command": "${INSTALL_DIR}/state-hook.sh your_turn" },
      "timeout": 5
    }
  ]
}
EOF

python3 -c "import json,sys; json.load(open('${hooks_file}'))" 2>/dev/null \
    || { echo "生成的 JSON 不合法，已中止" >&2; rm -f "${hooks_file}"; exit 1; }

echo "已装到 ${hooks_file}"
echo "脚本在 ${INSTALL_DIR}"
echo
echo "作用范围：${target_repo} 下的【所有】kiro-cli v3 会话，不需要 --agent。"
echo "被管住的工具：execute_bash / delete_file / 委派子 agent。"
echo "普通文件编辑刻意不管——一次改动几十个编辑，那会变成几十次按键，"
echo "而审批疲劳会让人条件反射按 1，闸门就成了摆设。"
echo
echo "自检："
if [[ -f "${HOME}/.kiboard/config" ]]; then
    url=$(sed -n 's/^KIBOARD_URL=//p' "${HOME}/.kiboard/config" | head -1)
    echo "  hub 地址 ${url:-未配置}"
    if [[ -n "${url}" ]]; then
        code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "${url}/health" 2>/dev/null)
        echo "  /health HTTP ${code:-连不上}"
    fi
else
    echo "  ⚠️  没有 ~/.kiboard/config，闸门会因为不知道 hub 地址而 fail-closed 拦下一切"
fi
if [[ -f "${HOME}/.kiboard/bypass" ]]; then
    echo "  ⚠️  ~/.kiboard/bypass 存在 —— 闸门当前被逃生阀跳过，删掉它才会真正生效"
fi
echo
echo "卸掉：$0 --uninstall"
echo "临时跳过：touch ~/.kiboard/bypass（删掉即恢复）"
