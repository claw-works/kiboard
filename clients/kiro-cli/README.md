# Kiro CLI 适配器

把 Kiro CLI 的工具调用审批路由到 kiboard 实体键盘：agent 要执行危险操作时，
设备亮灯+屏显，你按实体键决定放不放行。

适用 **Kiro CLI 2.16**（`kiro-cli --version`）。

## 机制

Kiro CLI 2.16 有 5 个 hook 触发器，只有 `preToolUse` 能拦下工具调用，
而它**只有退出码一条控制通道**：

| 退出码 | 行为 |
|---|---|
| `0` | 放行 |
| `2` | **阻止**，stderr 内容回给模型 |
| 其他 | 显示警告，**然后照样放行** |

没有 `permissionDecision: ask` 这种东西（那是 Claude Code 的机制）。
所以设备上只能表达批准 / 拒绝两态。

拒绝时 stderr 会进模型上下文，所以 `kiboard-ask` 会把 hub 返回的理由写到 stderr ——
agent 由此知道"被人拒了、为什么"，可以换方案而不是傻等或重试。

## 装

1. 装 `kiboard-ask` 并写好 `~/.kiboard/config`，见 [../README.md](../README.md)

2. 要么直接用现成的完整配置 [`agent-kiboard-gated.json`](agent-kiboard-gated.json)：

   ```bash
   cp clients/kiro-cli/agent-kiboard-gated.json ~/.kiro/agents/kiboard-gated.json
   # 把里面 hook.sh 的路径改成你的仓库位置
   ```

   要么把 [`agent-snippet.json`](agent-snippet.json) 里的 `hooks` 与 `allowedTools`
   合并进你已有的 agent 配置（`~/.kiro/agents/<name>.json`，或项目级 `.kiro/agents/`）。
   **注意 2.16 的 hooks 写在 agent 配置里**，不是独立的 `.kiro/hooks/*.json`（那是 3.0 的布局）。

3. 把 `command` 改成 `hook.sh` 的绝对路径，并确保可执行：

   ```bash
   chmod +x clients/kiro-cli/hook.sh
   kiro-cli agent validate ~/.kiro/agents/<name>.json
   ```

4. 用这个 agent 开会话（**不需要 `-a`**）：

   ```bash
   kiro-cli chat --agent kiboard-gated
   ```

   hooks 的作用域是 agent，不是全局 —— 别的 session 不指定 `--agent` 就不带闸门。
   想让它默认生效：`kiro-cli agent set-default kiboard-gated`。

5. **验证 hook 真的触发了**，别只看配置对不对：

   ```bash
   kiro-cli chat --no-interactive --agent kiboard-gated \
     "run exactly this shell command and then stop: git --version"
   ```

   `git --version` 不在 allow 规则里，所以设备上应该出现待批请求、会话卡住等你按键。
   如果它直接跑完了，说明 hook 没匹配上 —— 「配了但从未生效」是安全配置最危险的
   失效模式，因为你以为自己被保护着。

## 四个必须知道的配置项

**`timeout_ms` 必须设得比 hub 的超时更长。** 实测确认：**hook 超时是 fail-open —— 超时后
工具照样执行**（见 [hook-findings.md](hook-findings.md) 第 2 条）。
默认 `timeout_ms` 只有 30 秒，等人按键根本不够，不改就会常态性地静默放行。
设成 `180000`，让 hub 的 120s 先到、返回 `decision=timeout`，由 `kiboard-ask` 主动 `exit 2`。

**要走设备审批的工具必须列进 `allowedTools`。** 实测顺序是
`preToolUse hook → 内置权限判断 → 执行`，所以工具若没被 trust，你会先在设备上按一次键、
终端里还要再确认一次，变成"批两次"。把审批权完整交给设备，别让两套机制叠加。

**`cache_ttl_seconds` 保持 0。** 它会缓存成功的 hook 结果 ——
同一条命令第二次就不问了，审批被静默跳过。这是个容易埋雷的默认值。

**`matcher` 只拦会改东西的工具**：`execute_bash|fs_write|use_aws`。
`fs_read`/`grep`/`glob` 不进来 —— 只读操作没有审批价值，全拦会让 agent 没法用。
真正的收窄靠 hub 侧 `rules.toml`：只读命令走 `allow` 档直接放行，不打扰人。

## 验证

```bash
# 1. 不经过 Kiro，直接喂一条 preToolUse 载荷
echo '{"hook_event_name":"preToolUse","cwd":"'"$PWD"'","session_id":"test",
       "tool_name":"execute_bash","tool_input":{"command":"rm -rf /tmp/kiboard-test"}}' \
  | ./hook.sh; echo "exit=$?"
# 设备上应该出现 !! APPROVE（rm -rf 命中高危规则），需要长按 1 才放行

# 2. 只读命令应该直接放行、不打扰人
echo '{"hook_event_name":"preToolUse","cwd":"'"$PWD"'","session_id":"test",
       "tool_name":"execute_bash","tool_input":{"command":"git status"}}' \
  | ./hook.sh; echo "exit=$?"   # 期望 exit=0 且设备无反应

# 3. 逃逸阀
touch ~/.kiboard/bypass && echo '{"tool_name":"execute_bash","tool_input":{"command":"rm -rf x"}}' \
  | ./hook.sh; echo "exit=$?"   # 期望 exit=0
rm ~/.kiboard/bypass
```

## 实测结论

文档没写明的几件事已经跑过一遍，见 [hook-findings.md](hook-findings.md)：

| 结论 | 影响 |
|---|---|
| hooks 只认 agent 配置，不读 `.kiro/hooks/*.json` | 配置放对地方 |
| **`matcher` 是整串字面比较，`\|` 不支持** | 每个工具名单独一条 hook，否则一次都不触发 |
| **hook 超时 = fail-open，工具照样执行** | `timeout_ms` 必须加大，否则闸门静默失效 |
| `exit 2` 确实阻止，且 stderr 进模型上下文 | 拒绝能形成闭环，agent 会换方案 |
| hook 先跑，内置权限判断在后 | 要走设备审批的工具得进 `allowedTools`，否则批两次 |
| 载荷里**没有 `session_id`**，但多一个 `tool_input.summary` | 会话只能靠 `cwd` 区分；`summary` 当 detail 用，不当标题 |

`tool_input.summary` 是模型自己写的意图说明。它只当 `detail`，
**标题永远显示真正要执行的命令** —— 一个措辞良善、内容危险的 summary
会让人在错误的前提下批准。

## 状态上报（让设备平时也有用）

审批是"要人做决定"的时刻，但那只占很小一部分时间。其余时候设备应该能回答一个更日常的问题：
**现在轮到我了吗？** 抬头看一眼灯就知道，不用切回终端。

`agent-kiboard-gated.json` 里已经挂好了四个触发器：

| 触发器 | 上报 | 设备表现 |
|---|---|---|
| `agentSpawn` | `start` | 蓝灯慢闪 |
| `userPromptSubmit` | `working` | 蓝灯慢闪 + 屏显 `kiro@项目 working...` |
| `postToolUse` | `working` | 同上，带工具名 |
| `stop` | `your_turn` | **蓝灯常亮** + 屏显 `YOUR TURN`（反色） |

蓝灯（板载）表示"agent 在忙还是在等你"，和黄灯的审批语义分开：
慢闪 = 在干活，常亮 = 轮到你了，灭 = 空闲。红灯只在出错时亮 4 秒就灭 ——
常亮的红灯会变成背景噪音让人无视。

**有待批请求时，状态上报不会碰屏幕。** 那时屏幕在问一个需要决定的问题。

和审批钩子的关键区别：

| | 审批 `hook.sh` | 上报 `state-hook.sh` |
|---|---|---|
| 阻塞 | 是，等人按键 | 否 |
| 超时 | hook 180s / hub 120s | 3 秒 |
| 失败 | **fail-closed 阻止操作** | **永远 exit 0，忽略** |

一个"看看 agent 在干什么"的功能不能变成新的失败模式 —— hub 挂了不该让 agent 卡住。
另外 `agentSpawn` 与 `userPromptSubmit` 的 stdout 会进模型上下文，
所以 `state-hook.sh` 绝不往 stdout 写东西。

## 相关

- 协议契约：[`docs/client-protocol.md`](../../docs/client-protocol.md)
- 风险规则：[`hub/rules.toml`](../../hub/rules.toml)

## 任务上报（实测得到的做法）

`agent 现在在做什么` 这一屏由 `tasks-hook.sh` 自动喂，挂在 `postToolUse` 上：

```json
"postToolUse": [
  {"matcher": "todo_list", "command": ".../tasks-hook.sh",
   "timeout_ms": 8000, "cache_ttl_seconds": 0}
]
```

能这么做是因为 **agent 的待办清单本身就是一个工具调用**（`todo_list`），
所以它的每次变更都会经过 `postToolUse`，而 `tool_response` 里带的是
**整份清单的快照**而不只是这次的改动 —— 于是 hook 不需要维护任何状态。

清单只有 `completed` 布尔值、没有"进行中"。映射规则：
**第一个未完成的就是此刻在做的那件**，其余未完成的是计划。
设备上只显示进行中的那件，加上进度：`[kiro] 跑测试 2/3`。

### 三个实测结论（文档没写或写反了）

这三条都是拿 dump 载荷的临时 hook 实测出来的，都会让人写出静默失效的配置：

**1. `matcher` 是字面量比较，不是正则**（Kiro CLI 2.16）。

同一次 `fs_read` 调用：`matcher: "fs_read"` 触发，`"fs_.*"`、`"^fs_read$"`、
`"fs_read|todo_list"` **全都不触发**。官方文档说是 regex，实测不是。

这条最危险：`"execute_bash|fs_write"` 会得到一个**永不触发的 hook**，
而且没有任何报错。对审批闸门来说，那等于闸门根本不存在却以为受保护。
所以本目录的配置一律**一个工具一条**。

**2. 不写 `matcher` = 匹配全部。** 状态上报那几条就是这样挂的。

**3. 工具名是 `todo_list`，不是 `todo`。**
agent 配置的 `tools` 数组里写的是 `todo`，但 hook 载荷里的 `tool_name` 是 `todo_list`。
matcher 要用后者。

### 前提

todo 是 experimental 特性，要先开：

```bash
kiro-cli settings chat.enableTodoList true
```

没开的话 agent 根本没有这个工具，hook 自然永远不触发。

### 载荷长什么样

```json
{"hook_event_name": "postToolUse", "cwd": "...", "tool_name": "todo_list",
 "tool_input": {"command": "complete", "completed_indices": [0], ...},
 "tool_response": {"success": true, "result": [
   "TODO LIST STATE: {\"tasks\":[{\"task_description\":\"编译固件\",\"completed\":true}],
     \"id\":\"...\",\"session_id\":\"...\"}\n\n ID: ..."]}}
```

注意 `session_id` 要透传给 `kiboard-ask --session`：hub 按 session 分桶。
不传的话同一个 agent 换个工作目录会被当成两个 agent，屏幕上出现两行重复的任务
（这个实测撞到过）。

## v3（`kiro-cli --v3`）：作用范围完全不同

`--v3` 不读 `~/.kiboard/agents/*.json`，所以 `kiro-cli --agent kiboard-gated --v3`
会报 `agent "kiboard-gated" not found, using "default"`。

但这条路本来就不必走。v3 有个**更适合审批闸门**的机制：工作区级的
`.kiro/hooks/*.json`，**对该工作区所有会话生效，不需要 `--agent`**。

```bash
clients/kiro-cli/install-v3-hooks.sh            # 装到当前仓库
clients/kiro-cli/install-v3-hooks.sh --uninstall
```

### 三层作用范围（别搞混）

| 机制 | 范围 | matcher 语义 |
|---|---|---|
| 2.x agent 配置的 `hooks` | 只有用该 agent 起的会话 | **字面量比较** |
| v3 `.kiro/hooks/*.json` | 该工作区的**所有**会话 | **真正则** |
| 上报内容的分桶 | 按 `session_id` | — |

matcher 语义两边相反，这是实测的：

- 2.16 agent 配置：`fs_read` 命中，`fs_.*` / `^fs_read$` / `fs_read|todo_list` **全不命中**
- v3 工作区钩子：`execute_bash|fs_write` 和 `exec.*` **都命中**

在两边之间搬配置时必须改 matcher 写法，否则会得到一个静默永不触发的 hook。
对闸门来说那等于闸门不存在，而你以为受着保护。

### 载荷形状一致

v3 的 `PreToolUse` 载荷和 2.x 一样有 `session_id` / `hook_event_name` / `cwd` /
`tool_name` / `tool_input`，退出码语义也一样（0 放行、2 阻止且 stderr 回给模型），
所以 `hook.sh` 一行都不用改。触发器名从 camelCase 变成 PascalCase。

### 为什么不管普通文件编辑

装上的 matcher 只覆盖 `execute_bash` / `delete_file` / 委派子 agent。
`fs_write`、`str_replace` 这些**刻意不管**：`rules.toml` 的 `default = "normal"`，
没命中规则就要问人，而一次改动往往几十次编辑 —— 那会变成几十次按键。

**审批疲劳是真实的失败模式**：人开始条件反射按 1，闸门就成了摆设，
而且比没有更糟，因为它给人一种被保护的错觉。
仓库内的文件编辑有 git 兜底；`execute_bash` 才会伸到仓库外面（装包、发网络请求、
动别的机器），那是真正需要人看一眼的地方。

`delete_file` 单独列进来，因为它删的可能是未跟踪文件，git 救不回来。
