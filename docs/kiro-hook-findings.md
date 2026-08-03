# Kiro CLI 2.16 hook 实测结论

文档没写明的几件事，实测跑了一遍。**结论直接影响 kiboard 闸门的安全性**，
尤其是第 2 条 —— 它决定了整个 fail-closed 设计能不能成立。

实测环境：`kiro-cli 2.16.0` / macOS。方法：临时 agent 配置 + `kiro-cli chat --no-interactive`。

## 1. hooks 写在哪：只认 agent 配置，不读 `.kiro/hooks/*.json`

`.kiro/hooks/kbprobe.json`（3.0 的独立文件布局）放好后**完全不触发**；
写在 agent 配置 JSON 的 `hooks` 字段里则正常工作。

```json
"hooks": { "preToolUse": [ { "matcher": "execute_bash", "command": "...", "timeout_ms": 180000 } ] }
```

## 2. hook 超时 = fail-open，工具照样执行 ⚠️

设 `timeout_ms: 3000`，hook 里 `sleep 12`。结果：

```
✗ preToolUse "..." timed out after 3000 ms
...
TIMEOUT-MARKER-DID-RUN          ← 命令执行了
```

hook 日志只留下 `hook-started`，没有 `hook-finished` —— hook 被杀了，
**而工具被放行**。

对 kiboard 的含义：**只要 hook 超时，闸门就静默打开**。所以

- `timeout_ms` 必须显著大于 hub 的审批超时（我们用 hub 120s / hook 180s）
- 让 hub 先超时返回 `decision=timeout`，由 `kiboard-ask` 主动 `exit 2`
- 绝不能把"等人按键"的时间压在 hook 的超时边界上

默认 `timeout_ms` 只有 30 秒，等人按键根本不够，不改就会常态性地 fail-open。

## 3. `exit 2` 确实阻止，且 stderr 会进模型上下文

```
[38;5;9mPreToolHook blocked the tool execution: KIBOARD-DENY: the physical keyboard rejected this command
```

模型随后自己说："命令被本地的 PreToolHook 拦截了，未能执行，返回信息是 ..."。

所以拒绝不是单向阻断，而是**闭环**：设备上按下"拒绝"的理由能传到模型，
它可以据此换方案而不是重试。`kiboard-ask` 因此把 hub 返回的 `reason`
写成引导性的措辞（"不要重试同一操作，去问用户或换个方案"）。

## 3.5 子 agent 完全绕过父 agent 的 hook ⚠️

父 agent 挂一个**不带 matcher**（= 匹配全部工具）的 hook，然后让它委派子 agent 执行命令：

```
hook 只记录到一次：tool_name = use_subagent
子 agent 实际执行的 execute_bash —— 一次都没触发
```

子 agent 跑的是 `kiro_default`，用它自己的配置，没有父 agent 的 hooks。
**所以把危险命令丢给子 agent 就能绕过整套审批。**

好消息：`use_subagent` 本身拦得住（`matcher: "use_subagent"` + `exit 2` 实测有效，
终端显示 `blocked the tool execution: ...`）。

对策：把**委派本身**纳入闸门，并判为高危 —— 无法预知子 agent 会做什么，
委派等于交出控制权，这个动作本身就该由人批。`hub/rules.toml` 里有对应规则。

## 3.6 matcher 是**整串字面比较**工具名，`|` 不支持

```
matcher "execute_bash"            → 触发 ✓
matcher "execute_bash|fs_write"   → 不触发 ✗
matcher "execute"                 → 不触发（前缀不算）
matcher ".*"                      → 不触发
省略 matcher                      → 匹配全部工具
```

3.0 的文档说 matcher 是 regex、还给了 `"fs_write|str_replace"` 这样的示例，
**2.16 完全不是**：它把 matcher 当成一个完整的工具名去比较，`|` 只是普通字符。

所以要拦多个工具，必须**每个工具名单独一条 hook**：

```json
"preToolUse": [
  { "matcher": "execute_bash", "command": "...", "timeout_ms": 180000 },
  { "matcher": "fs_write",     "command": "...", "timeout_ms": 180000 },
  { "matcher": "use_subagent", "command": "...", "timeout_ms": 180000 }
]
```

### 这条是怎么踩出来的，值得记着

第一次实测时我只发现 `matcher "execute_bash|subagent"` 匹配不到 `use_subagent`，
就推断成"`|` 是精确名之间的或"——**从一个否定结果反推，没验证肯定的那一半**。
于是配置模板里写了 `"execute_bash|fs_write|use_aws"`，看着合理、`agent validate` 也通过，
**但 hook 一次都没触发过**：闸门配好了、界面正常、什么都不报错，而所有命令照常执行。

这就是安全配置最危险的失效模式：**配了、看着对、但从未生效**。
它比"没配"糟得多，因为你以为自己被保护着。

判断 hook 到底有没有跑，看会话输出里有没有 `hooks finished` 那一行最直接。
更可靠的是**故意让 hook `exit 2`，确认操作真的被拦住**——能拦住才说明匹配上了。

## 4. hook 先跑，内置权限判断在后

## 4. hook 先跑，内置权限判断在后

用一个 `allowedTools: []` 的 agent（工具需要逐次批准）+ 非交互模式：

```
ORDER-HOOK-RAN 1785587931        ← hook 先跑了
...
Command execute_bash is rejected because it matches one or more rules on the denied list:
  - non-interactive mode (no user to approve)
```

顺序是 **preToolUse hook → 内置权限判断 → 执行**。

**这条有个重要的配置后果**：交互模式下如果工具没被 trust，你会先在设备上按一次键，
终端里还要再确认一次 —— 变成"批两次"。所以要走设备审批的工具应该加进
`allowedTools`，把审批权完整交给设备，别让两套机制叠加。

## 5. `preToolUse` 载荷的真实字段

```json
{"hook_event_name":"preToolUse",
 "cwd":"/private/tmp/kbprobe",
 "tool_name":"execute_bash",
 "tool_input":{"command":"echo hello-kiboard","summary":"echo hello-kiboard"}}
```

两点和文档不一致：

- **没有 `session_id`**（文档示例里有）。所以 `source.session` 会是空的，
  多会话并发时只能靠 `cwd` 区分
- `tool_input` 里多一个 **`summary`** —— 模型自己写的意图说明

`summary` 拿来当 `detail` 很合适，但**绝不能当标题**：屏幕上必须先显示真正要执行的命令。
一个措辞良善、内容危险的 summary 会让人在错误的前提下批准。`kiboard-ask` 就是这么处理的。

## 6. 顺带一条：规则正则不要依赖 JSON 键序

`serde_json` 默认按**字母序**输出对象键。`{"command":..,"summary":..}` 里 `command` 恰好在前，
但客户端将来多一个排在它之前的字段（比如 `background`）就会让 `^\{"command"` 这种锚定失效。
规则应锚在字段名上（`"command"\s*:\s*"\s*`），与键序无关。

（`hub/rules.toml` 已按此修正，并用带 `summary` 的真实载荷验证过 `git status`
仍然命中 allow 档。）

## 7. 长按 600ms 对人手来说太短

固件在 `LONG_PRESS_MS = 600` 发 `long` 事件。实测让人"短按"一下时，
他按到了 600ms 以上，系统判成长按并批准了 —— 人以为在点，系统以为在按住。

原因不难理解：手指点一下通常 80~150ms，但换成"我在按一个危险按钮"的心态，
手会不自觉地按稳一点。600ms 落在这个区间里，区分不开。

对策没有改固件，而是**把计时挪到 hub**：hub 收到 `press` 记时间、`release` 时算真实时长，
要求 ≥ `KIBOARD_HIGH_HOLD_MS`（默认 1200ms）。固件的 600ms `long` 事件保留，
降级成"继续按住"的中途提示。

这么分的好处：阈值以后改配置就行，不用为一个常量重新烧板子；
而且能给出过程反馈（到点时黄灯转常亮 + 屏幕 `release to accept`），
人不必靠猜按够了没有 —— 那正是会不自觉多按或早松的根源。

顺带确认固件不会吞掉 `release`：`EVENT_COOLDOWN_MS = 250` 只是把 250ms 内的
状态切换推迟上报，不是丢弃，所以极快的点按也一定会有 `release` 事件到达 hub。

## 复现方法

```bash
mkdir -p /tmp/kbprobe && cd /tmp/kbprobe
cat > ~/.kiro/agents/kbprobe-log.json <<'JSON'
{ "name": "kbprobe-log", "tools": ["execute_bash"], "allowedTools": ["execute_bash"],
  "hooks": { "preToolUse": [ { "matcher": "execute_bash",
    "command": "{ echo '--- preToolUse ---'; cat; echo; } >> /tmp/kbprobe/payload.log; exit 0" } ] } }
JSON
kiro-cli agent validate --path ~/.kiro/agents/kbprobe-log.json
kiro-cli chat --no-interactive --trust-all-tools --agent kbprobe-log \
  "run exactly this shell command and then stop: echo hello-kiboard"
cat /tmp/kbprobe/payload.log
```

把 `command` 换成 `exit 2` / `sleep 12`（配 `timeout_ms: 3000`）就能复现第 2、3 条。
macOS 没有 `timeout` 命令，别在脚本里用。
