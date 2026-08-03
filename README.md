# kiboard

agent 要执行危险操作时，**把决定挪到 agent 碰不到的地方**：亮灯、屏显、等人按实体键
决定接受 / 拒绝 / 全部接受。

## 结构

```
hub/         Rust 服务端。通用，不认识具体设备：记录事件、标注风险、呈现给人、持久审计
protocol/    三方消息契约（唯一真源）+ JSON Schema + 回归脚本
devices/     设备端方案，每个方案自成一体（固件 + 硬件设计 + 方案文档）
clients/     agent 侧适配器：kiro-cli / claude-code，各自处理宿主运行时的差异
docs/        跨方案文档：客户端接入契约、威胁模型
scripts/     部署核对
```

三条边界，越界就是设计错误：**hub 不裁决**（拦不拦的动作在客户端）、
**hub 不排版**（像素和键位是设备的事）、**设备不理解命令语义**。

## 三方怎么配合

```
agent 客户端            hub                      设备
─────────────────      ─────────────────        ─────────────────
拦截工具调用       →   记录、标注、呈现     →   显示、收人的意思
翻译裁决为运行时       持久审计、会话态          回 accept / reject
能懂的信号         ←   回 verdict           ←   声明自己的能力
```

**钩子只有否决权。** Kiro CLI 与 Claude Code 的 `exit 0` 都只表示"我不否决"，
不等于已授权，宿主自己的权限模型还会再判一次。所以要让设备成为唯一审批点，
受管工具必须在宿主侧预先放通——而那意味着**只剩一道门，那道门必须验证过真的会触发**。
详见 [docs/client-protocol.md](docs/client-protocol.md)。

## 设备方案

| 方案 | 状态 | 定位 |
|---|---|---|
| [c3-keypad](devices/c3-keypad/) | 面包板实测通过 | ESP32-C3 + 4×4 矩阵键盘 + OLED。哑终端，保证最强 |
| [s3-touch](devices/s3-touch/) | 未开始 | ESP32-S3 + 触摸屏软键盘 |
| [mobile](devices/mobile/) | 未开始 | Android / iOS，离席也能批 |

**三者强度不等价**：`c3-keypad > s3-touch > mobile`，便利性正好相反。
不要把它们的裁决当成同一强度记进审计，理由见 [docs/threat-model.md](docs/threat-model.md)。

下一个做的建议是 **mobile 而不是 s3-touch**：它不用等 PCB，而且是纯自渲染设备，
能验证协议抽象到底对不对。

## 装闸门

```bash
# Kiro CLI v3：对整个工作区所有会话生效，不需要 --agent
clients/kiro-cli/install-v3-hooks.sh
# 2.x：把 hooks 段合并进 agent 配置，只对用该 agent 起的会话生效
#   见 clients/kiro-cli/README.md
```

逃生阀 `touch ~/.kiboard/bypass`（删掉即恢复）。闸门是 fail-closed 的：
设备离线或 hub 连不上时，非白名单命令一律被拦，**不会**悄悄放行。

**装完必须验证它真的会触发**——故意让它拒绝一次，确认操作被拦住。
「配了、看着对、但从未生效」是这套东西最危险的失效模式，比没装更糟，
因为你以为自己被保护着。

### ⚠️ 公司管控的机器：会触发 EDR 告警

这套架构在装了 CrowdStrike 这类 EDR 的机器上**大概率会报警**，已经实际发生过一次
（主机被网络隔离）：每次工具调用都执行一个新落地的未签名可执行文件，
向公司网络外的非标准端口发小 POST，频率高、包小、目标固定——正是 C2 beaconing 的特征。

首选应对是 **hub 放本机**（`KIBOARD_URL=http://127.0.0.1:26041`）、设备走 USB 串口，
没有外联就没有这个问题。其余选项和另一条教训（测试用例别写像真实攻击的载荷）
见 [docs/threat-model.md](docs/threat-model.md)。

## 协议

消息契约、字段语义、认证角色、强确认分级、自动放行边界都在
**[protocol/README.md](protocol/README.md)**，schema 带回归脚本：

```bash
uvx --from jsonschema python protocol/validate.py
```

三条容易被忽略但必须遵守的约束：

- **`verbatim` 是 required-to-display**。逐字原文必须显示，不得被 agent 自己写的
  `summary` 取代——措辞良善、内容危险的 summary 会让人在错误前提下批准
- **`client` 角色不得提交裁决**。否则 agent 拿着客户端凭据就能自己批准自己
- **只有确定性规则表能自动放行，模型不行**。被判定的文本本身就是攻击面

设备与 hub 之间当前的线格式（串口 JSON Lines / WebSocket）见
[devices/c3-keypad/README.md](devices/c3-keypad/README.md) 与 protocol 的实现状态表。

## hub

```bash
cd hub && cargo run                            # 起服务，默认端口 26041
curl localhost:26041/status                    # 设备/wifi/模式状态
curl -H "X-Api-Key: $KEY" 'localhost:26041/audit?limit=20'   # 审批历史
```

WebSocket `ws://localhost:26041/ws`：连上先收 `{"event":"snapshot",...}`，之后实时推
`device_up/device_down/key/wifi/log`；也可反向发 `{"t":"msg"|"led"|"ping",...}`。

风险规则在 [hub/rules.toml](hub/rules.toml)，三档 `allow` / `normal` / `high`。
`allow` 档会下发到客户端本地缓存、不联网直接放行，所以 hub 挂了也不会把 `git status`
这种命令一起拦死。

hub 运行时占用串口，刷固件前先 `pkill -f kiboard-hub`。

## 现在的主要缺口

按优先级：

1. **角色化认证还没做**。`/device` 用单一共享 token，`client` 与 `device` 权限没分开
2. **hub 还在替设备排版**。`keymap.rs` 和 `approval.rs` 里有 16 键 + 128×64 单色屏的
   假设，第二个设备方案进来之前必须先把呈现层下沉
3. **本地放行的调用不进审计**。审计是"人裁决过的"完整记录，不是全量工具调用记录
