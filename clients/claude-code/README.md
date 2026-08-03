# Claude Code 适配器

把 Claude Code 的工具调用路由到 kiboard 实体键盘审批。

> ## ⚠️ 未经实测
>
> 本机没装 Claude Code，**这套适配器没有在真实 Claude Code 上跑过**。
> 载荷字段和决策通道依官方文档与 claude-code 仓库的 issue 编写，
> 核心逻辑（HTTP、规则、失败策略、超时）复用的是已经实测通过的
> `kiboard-ask` 二进制，与 kiro-cli 适配器完全同一套。
>
> 能确认的部分已经用 `selftest.sh` 覆盖（载荷映射、退出码、决策 JSON 形状、
> 降级路径，5 项全过）。**没被证明的是 Claude Code 认不认这份输出**——
> 那必须装了才知道，见文末「装好之后要验的三件事」。

## 装

```bash
# 1. 闸门二进制
cd hub && cargo build --release
install -m755 target/release/kiboard-ask ~/.local/bin/

# 2. 适配器脚本
mkdir -p ~/.kiboard/clients/claude-code
install -m755 clients/claude-code/{hook.sh,state-hook.sh} ~/.kiboard/clients/claude-code/

# 3. hub 地址与密钥
cat > ~/.kiboard/config <<'EOF'
KIBOARD_URL=http://your-hub:26041
KIBOARD_API_KEY=你的密钥
EOF
chmod 600 ~/.kiboard/config

# 4. 把 settings-snippet.json 的 hooks 段合并进 ~/.claude/settings.json
```

## 决策通道：和 Kiro 不一样的地方

Kiro CLI 只看退出码。Claude Code 的 stdout 也是决策通道，这带来一个必须做的选择。

| | Kiro CLI | Claude Code |
|---|---|---|
| 放行 | `exit 0` | `exit 0`；**静默 exit 0 只是"不反对"**，CC 自己的权限系统照旧会再问一次 |
| 拒绝 | `exit 2`，stderr 进模型上下文 | 同样 `exit 2` + stderr |
| 免掉宿主二次确认 | 无此概念 | 需显式输出 `hookSpecificOutput.permissionDecision = "allow"` |

于是有个开关 `KIBOARD_CC_DECISION`：

- **`passthrough`（默认）**：放行时静默 `exit 0`。Claude Code 的权限提示照旧，
  代价是同一条命令**问两遍**（先在设备上按，再在终端确认）。
- **`explicit`**：放行时输出 `permissionDecision=allow`，CC 不再问。
  代价是 **kiboard 的 `rules.toml` 顶替了 Claude Code 自己的权限系统**，
  成为唯一放行依据。

默认选 `passthrough`，理由是"悄悄关掉宿主的安全机制"不该是默认行为。
想要"实体键盘按一次就算数"的体验就设 `explicit`，但要清楚这时 `rules.toml`
写错就没有第二道防线了。

## 三个刻意的取舍

**1. 不用 `"ask"` 档。** Claude Code 的 `permissionDecision` 有 allow/deny/ask 三档，
`ask` 看起来最贴合"问人"这件事，但它有多个已知 bug：
[不被强制执行](https://github.com/anthropics/claude-code/issues/79356)、
[permissions.ask 规则静默失效](https://github.com/anthropics/claude-code/issues/81041)、
[bypassPermissions 下被静默批准](https://github.com/anthropics/claude-code/issues/77212)。
而 kiboard 根本不需要它——人已经在实体键盘上答过了，我们要表达的是结论而不是提问。

**2. 拒绝走 `exit 2` 而不是 `permissionDecision="deny"`。**
`exit 2` 在各版本上被证实可靠，且 stderr 会进模型上下文，能引导它换个方案；
而 `deny` 的行为随版本变过。既然两条路都能拦，选被验证得更充分的那条。

**3. `permissionDecision` 必须包在 `hookSpecificOutput` 里。**
扁平写成 `{"permissionDecision":"allow"}` 会被
[静默丢弃](https://github.com/anthropics/claude-code/issues/48760)——hook 正常退出、
日志正常打印，但权限系统收不到裁决。`selftest.sh` 第 3 项专门盯这个形状。

## matcher 为什么包含 Task

`Task` 是子 agent 工具。**子 agent 不继承父 agent 的闸门**——这一点在 kiro-cli 上
已实测证实：委派出去的子 agent 完全不经过父 agent 的 hook。漏掉 `Task`
等于留一个"让子 agent 去执行"的后门。

## timeout 为什么是 180

要留够人从座位走到设备前按键的时间，并且**必须大于 hub 的 `KIBOARD_TIMEOUT_S`**。
如果 hook 先超时，裁决权就落到 Claude Code 手里了（它的超时行为文档没写明），
而 kiboard 的设计前提是"超时算拒绝"这件事由自己决定。

## 上线前自检

不用装 Claude Code 就能跑：

```bash
clients/claude-code/selftest.sh              # 不联网的 5 项
KIBOARD_LIVE=1 clients/claude-code/selftest.sh   # 再加一项真连 hub（设备会亮灯，按 2 拒绝）
```

## 装好之后要验的三件事

这三件是**目前唯一没被证明**的部分，装好 Claude Code 后请依次确认：

1. **hook 到底触发没有。** 让它跑一条 `npm install`（规则表里是 normal 档），
   看设备是否亮灯上屏。不亮就是 hook 没挂上或 matcher 没匹配到 —— 用
   `claude --debug` 看 hook 有没有被调用。
2. **`explicit` 模式下 CC 认不认这份 allow。** 设 `KIBOARD_CC_DECISION=explicit`，
   在设备上按 1 批准，看终端**有没有再弹一次确认**。还弹就说明输出格式没被接受
   （对照上面第 3 条那个 issue 检查 JSON 形状）。
3. **拒绝真的拦住了。** 让它跑一条 `rm -rf` 之类（high 档），在设备上按 2，
   确认命令没有被执行、且 Claude 收到了拒绝理由并改口。**这条最关键**——
   前两条只影响体验，这条决定闸门是不是真的存在。

验完请把结果补进本文件，把开头那个「未经实测」的警告改掉。
