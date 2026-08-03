# 客户端适配器

各家 agent 客户端拦截工具调用的机制不同，但需要人做的判断是同一件事。
统一消息体与决策通道对照见 [docs/client-protocol.md](../client-protocol.md)
（仓库内路径：`docs/client-protocol.md`）。

这些客户端的钩子都是**本机子进程**，没有 webhook 形态，所以每台跑 agent 的机器上
都要装一个 `kiboard-ask`。它是通用实现，各客户端目录下只放薄壳与配置片段。

## 装 kiboard-ask

```bash
cd hub && cargo build --release
sudo install -m 755 target/release/kiboard-ask /usr/local/bin/
kiboard-ask --help
```

## 配置（所有客户端共用一份）

```bash
mkdir -p ~/.kiboard
cat > ~/.kiboard/config <<'EOF'
KIBOARD_URL=http://your-hub:26041
KIBOARD_API_KEY=你的 api key
KIBOARD_ON_FAILURE=closed
KIBOARD_TIMEOUT_S=120
EOF
chmod 600 ~/.kiboard/config
```

API key 别写进 agent 配置 JSON —— 那个文件通常会进 git。

写 shell 薄壳时注意：**变量后面跟中文标点一定写 `${VAR}`**。
`"code=$rc）"` 在 UTF-8 locale 下会被 bash 当成变量名 `rc）`，`set -u` 下直接中止，
退出码 1 —— 而那正好是「警告后照样执行」，闸门静默失效。详见 `docs/client-protocol.md`。

## 逃逸阀

键盘不在身边、或者要跑一批不想逐条确认的活：

```bash
touch ~/.kiboard/bypass    # 跳过审批
rm ~/.kiboard/bypass       # 恢复
```

这是**显式**开关。故意不做成「连不上 hub 就放行」——那等于把闸门的开关交给网络状况。

## 已有适配器

| 目录 | 客户端 | 状态 |
|---|---|---|
| [kiro-cli/](kiro-cli/) | Kiro CLI 2.16 | 可用，已端到端实测 |
| [claude-code/](claude-code/) | Claude Code | 已写，**未在真实 CC 上实测**（本机没装）。自检 5 项全过，待验的三件事见其 README |
| codex/ | Codex | 待调研（有自己的 approval_policy / sandbox 模型） |

## 加一个新客户端

1. 在 `docs/client-protocol.md` 的「决策通道对照」表里加一行
2. 如果它的钩子载荷格式固定，在 `hub/src/bin/ask.rs` 的 `build_request` 里加一个分支；
   否则用 `--client raw`，自己在外面拼好统一消息体喂进 stdin
3. `hub/rules.toml` 里为它的工具名补规则组
4. 这个目录下建子目录放配置片段与 README
5. 写一个不依赖该客户端就能跑的 `selftest.sh`（参考 `claude-code/selftest.sh`）。
   没装客户端也能验载荷映射、退出码、决策输出形状和降级路径——
   这四样占了适配器的绝大部分逻辑

hub 侧不需要改代码。如果需要改，说明契约漏了东西，先回去改文档。
