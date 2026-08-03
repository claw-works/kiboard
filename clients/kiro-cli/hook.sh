#!/usr/bin/env bash
# Kiro CLI 的 preToolUse 钩子：把工具调用的审批送到 kiboard 实体键盘
#
# 这个壳故意做得很薄——JSON 解析、失败策略、超时余量全在 kiboard-ask 里。
# 它只负责：找到 kiboard-ask，把 stdin 原样递过去，把退出码原样传回去。
#
# 退出码语义（Kiro CLI 2.16）：
#   0  放行
#   2  阻止，stderr 回给模型
#   其他  显示警告后【照样执行】—— 所以这里绝不能让非 0/2 的退出码漏出去
#
# 装法见同目录 README.md

set -uo pipefail

# 兜底：任何未预期的中止都必须落到 exit 2。
#
# 这里真踩过一次，且**只在 UTF-8 locale 下复现**：写成 "code=$rc）" 时，
# bash 把紧跟的中文标点算进变量名（C locale 下不会），set -u 于是报
# unbound variable 直接中止、退出码 1；而 Kiro 对非 0/2 的码是"警告后照样执行"——
# 本该 fail-closed 阻止的分支变成了静默放行。
# 教训：变量后面跟中文一律写 ${VAR}。但写对变量只修好了那一处，
# 这个 trap 修的是一类——闸门脚本自己崩了也不能让操作漏过去。
trap 'code=$?; if [ "${code}" != 0 ] && [ "${code}" != 2 ]; then
        echo "kiboard: 钩子自身异常退出（code=${code}），已按 fail-closed 阻止。" >&2
        exit 2
      fi' EXIT

# 找 kiboard-ask：优先 PATH，其次几个常见位置，最后仓库内的 debug 产物（开发时方便）
find_ask() {
    if command -v kiboard-ask >/dev/null 2>&1; then
        command -v kiboard-ask
        return 0
    fi
    local candidates=(
        "/usr/local/bin/kiboard-ask"
        "$HOME/.local/bin/kiboard-ask"
        "$HOME/.cargo/bin/kiboard-ask"
        "${KIBOARD_ASK:-}"
    )
    local c
    for c in "${candidates[@]}"; do
        [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
    done
    return 1
}

ASK="$(find_ask)" || {
    # 找不到闸门程序：这是配置错误，不能当成“批准”
    echo "kiboard: 找不到 kiboard-ask（装一下或设 KIBOARD_ASK 指向它）。已按 fail-closed 阻止。" >&2
    exit 2
}

"$ASK" --client kiro-cli
rc=$?

# 只把 0 和 2 透出去。任何别的退出码在 Kiro 那边等于“放行”，
# 而它通常意味着闸门自己出了问题，那种情况应该阻止而不是放过。
case "$rc" in
    0) exit 0 ;;
    2) exit 2 ;;
    *)
        echo "kiboard: kiboard-ask 异常退出（code=${rc}）。已按 fail-closed 阻止。" >&2
        exit 2
        ;;
esac
