#!/usr/bin/env bash
# 不装 Claude Code 也能跑的上线前自检。
#
# 它能验证什么：载荷解析、退出码语义、stdout 决策 JSON 的形状、以及
# 「找不到 kiboard-ask 时不把会话卡死」这条降级路径。
# 它验证不了什么：**Claude Code 到底认不认这份输出**。那必须装了才知道。
#
# 用法：
#   clients/claude-code/selftest.sh            # 只查不联网的部分
#   KIBOARD_LIVE=1 clients/claude-code/selftest.sh   # 连 hub 真跑一次审批
set -uo pipefail
cd "$(dirname "$0")/../.."

ASK="${KIBOARD_ASK_BIN:-$HOME/.local/bin/kiboard-ask}"
pass=0
fail=0

ok()   { echo "  OK   $1"; pass=$((pass + 1)); }
bad()  { echo "  FAIL $1"; echo "       $2"; fail=$((fail + 1)); }

# Claude Code PreToolUse 的真实载荷形状（依官方文档）
payload() {
    cat <<JSON
{
  "session_id": "selftest-0001",
  "transcript_path": "/tmp/transcript.jsonl",
  "cwd": "$(pwd)",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": $1, "description": "自检" }
}
JSON
}

echo "1) 载荷解析：统一消息体里该出现 tool_name / tool_input / cwd / session_id"
out=$(echo "$(payload '"git status"')" | "$ASK" --client claude-code --dump-request 2>&1)
if echo "$out" | grep -q '"client": *"claude-code"' &&
   echo "$out" | grep -q '"name": *"Bash"' &&
   echo "$out" | grep -q 'selftest-0001'; then
    ok "字段映射正确"
else
    bad "字段映射" "$out"
fi

echo "2) 本地规则放行：只读命令不联网、静默 exit 0（passthrough 默认）"
out=$(echo "$(payload '"git status"')" | KIBOARD_CC_DECISION=passthrough "$ASK" --client claude-code 2>&1 >/tmp/cc_stdout.txt)
code=$?
stdout_content=$(cat /tmp/cc_stdout.txt)
if [[ $code -eq 0 && -z "$stdout_content" ]]; then
    ok "exit 0 且 stdout 为空（不干扰 CC 自己的权限系统）"
else
    bad "passthrough 模式" "exit=$code stdout='$stdout_content' stderr=$out"
fi

echo "3) explicit 模式：放行时输出 hookSpecificOutput.permissionDecision=allow"
echo "$(payload '"git status"')" | KIBOARD_CC_DECISION=explicit "$ASK" --client claude-code >/tmp/cc_stdout.txt 2>/dev/null
code=$?
if [[ $code -eq 0 ]] && python3 - <<'PY'
import json,sys
try:
    d=json.load(open('/tmp/cc_stdout.txt'))
except Exception as e:
    print("  stdout 不是合法 JSON:",e); sys.exit(1)
h=d.get("hookSpecificOutput")
assert isinstance(h,dict), "permissionDecision 必须包在 hookSpecificOutput 里（扁平写法会被静默丢弃，见 claude-code#48760）"
assert h.get("hookEventName")=="PreToolUse", h
assert h.get("permissionDecision")=="allow", h
assert h.get("permissionDecisionReason"), "要带理由，否则日志里看不出为什么放的"
PY
then
    ok "决策 JSON 形状正确"
else
    bad "explicit 模式" "exit=$code stdout=$(cat /tmp/cc_stdout.txt)"
fi

echo "4) 拒绝走 exit 2 而不是 stdout 决策（exit 2 各版本都可靠；deny/ask 档行为变过）"
# 把 hub 指到一个死端口，让非 allow 的命令走 fail-closed，看它怎么表达"拒绝"
echo "$(payload '"npm install left-pad"')" | \
    KIBOARD_URL=http://127.0.0.1:1 KIBOARD_CC_DECISION=explicit \
    "$ASK" --client claude-code >/tmp/cc_stdout.txt 2>/tmp/cc_stderr.txt
code=$?
stdout_content=$(cat /tmp/cc_stdout.txt)
if [[ $code -eq 2 && -z "$stdout_content" ]]; then
    ok "exit 2 且 stdout 为空（连 explicit 模式下也不用 deny/ask 档）"
elif [[ $code -ne 2 ]]; then
    bad "fail-closed 失效" "exit=$code（应为 2）stderr=$(cat /tmp/cc_stderr.txt)"
else
    bad "拒绝时往 stdout 写了决策" "$stdout_content"
fi

echo "5) 降级：找不到 kiboard-ask 时放行并告警，不把会话卡死"
out=$(echo "$(payload '"git status"')" | KIBOARD_ASK_BIN=/nonexistent/kiboard-ask clients/claude-code/hook.sh 2>&1 >/dev/null)
code=$?
if [[ $code -eq 0 ]] && echo "$out" | grep -q "找不到"; then
    ok "exit 0 且 stderr 有告警"
else
    bad "降级路径" "exit=$code stderr=$out"
fi

if [[ "${KIBOARD_LIVE:-}" == "1" ]]; then
    echo "6) 真连 hub：危险命令应上屏等人按键"
    echo "   （设备会亮灯，请按 2 拒绝）"
    echo "$(payload '"rm -rf /tmp/kiboard-selftest-victim"')" | "$ASK" --client claude-code >/tmp/cc_stdout.txt 2>/tmp/cc_stderr.txt
    code=$?
    if [[ $code -eq 2 ]]; then
        ok "拒绝走 exit 2（可靠通道），理由进 stderr：$(head -c 80 /tmp/cc_stderr.txt)"
    else
        bad "危险命令未被拦下" "exit=$code $(cat /tmp/cc_stderr.txt)"
    fi
fi

echo
echo "通过 ${pass}，失败 ${fail}"
[[ $fail -eq 0 ]] || exit 1
echo
echo "注意：以上只证明适配器自身行为符合文档，**没有证明 Claude Code 认这份输出**。"
echo "装好 Claude Code 后请按 README「装好之后要验的三件事」实测。"
