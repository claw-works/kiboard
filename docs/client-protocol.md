# 客户端接入契约

kiboard 要给多种 agent 客户端（Kiro CLI、Claude Code、OpenClaw…）当审批闸门。
这些客户端拦截工具调用的机制各不相同，但**需要人做的判断是同一件事**。
所以这里定一条规矩：

> **消息体统一，差异只落在「决策通道」这一层。**

风险分级、排队、自动接受、审计全部在 hub 侧，客户端适配器只做三件事：
读本机格式 → 转成统一消息体 → 把 hub 返回的 decision 翻译成本客户端能懂的信号。

**分工**：语义层（消息体长什么样、字段什么意思）的唯一真源是
[`../protocol/README.md`](../protocol/README.md)；**本文管执行层**——各家客户端的拦截点、
退出码语义、失败倾向，以及接一个新客户端要验什么。

## 钩子只有否决权，不是授权权

接任何客户端之前必须先想清楚这件事，而且它**不是 Kiro 的怪癖**：

- Kiro CLI：`exit 2` = 阻止，`exit 0` = **不阻止**（不等于已授权）。
  v3 多一个 `permissionDecision: "ask"`，作用是强制弹一次询问，仍不是代为批准
- Claude Code：官方文档写明 hook 返回 allow **不会**跳过后面的 deny 与 ask 规则，
  那些照样评估（[Claude 官方文档](https://code.claude.com/docs/en/agent-sdk/permissions)）

所以设备上按「接受」的真实含义是"**我不否决**"，接下来宿主自己的权限模型还会再判一次。
两个很实际的后果：

1. **不预先 trust 就会批两次**：设备上按一次键，终端里还要再确认一次。要让设备成为
   唯一审批点，受管工具必须在宿主侧预先放通（Kiro 的 `allowedTools`）
2. **合并成一道门之后，那道门必须验证过真的会触发**。宿主侧已经 trust、而 hook 因为
   matcher 写法 / 缓存 / 超时没生效，等于**零道门**。这三种失效方式都实际踩过，
   见 [`../clients/kiro-cli/hook-findings.md`](../clients/kiro-cli/hook-findings.md)

例外是 **OpenClaw**：它原生就有 exec approvals（policy + allowlist + 可选人工批准），
在它上面该把 kiboard 接成一个**审批通道**而不是否决钩子——那样能拿到真正的「批准」语义。

## 执行力档位（`enforcement`）

客户端接入时要如实声明属于哪一档，hub 把它写进审计：

| 档位 | 含义 | 例子 |
|---|---|---|
| `block` | 能在执行前同步阻止，但不能代替用户批准 | Kiro CLI、Claude Code 的 PreToolUse |
| `approve` | 能表达真正的批准 | OpenClaw 的原生审批通道 |
| `observe` | 没有拦截点，只能记录，**拦不住任何东西** | 无钩子机制的运行时 |

**这个字段不加，审计会撒谎**：一条 `observe` 客户端上的 `reject` 看起来像"拦住了"，
其实那条命令照样跑了。出事复盘会得出完全错误的结论。

## 接一个新客户端，先回答这五个问题

这些文档往往不写、或者写反，所以每条都要**亲手验**：

| # | 问题 | 答错的后果 |
|---|---|---|
| 1 | 有没有**同步的执行前拦截点**？ | 没有就只能做通知和审计，别当闸门用 |
| 2 | 只能否决，还是能**代替用户批准**？ | 决定要不要在宿主侧预先 trust，否则批两次 |
| 3 | **超时是 fail-open 还是 fail-closed**？ | Kiro 实测 fail-open——超时后工具照样执行，闸门静默失效 |
| 4 | **子 agent / 委派路径继承钩子吗**？ | Kiro 2.x 实测不继承，危险命令丢给子 agent 就绕过整套审批 |
| 5 | 钩子结果**会不会被缓存**？ | Kiro 的 `cache_ttl_seconds` 会让同一条命令第二次不再问人 |

可靠的验证方法只有一个：**故意让钩子拒绝，确认操作真的被拦住**。看配置对不对没用——
「配了、看着对、但从未生效」是安全配置最危险的失效模式，比没配更糟，
因为你以为自己被保护着。

## 为什么必须在客户端跑一个进程

这些客户端的钩子都是**本机子进程**，没有 webhook 形态：客户端不会主动往外发 HTTP，
而是起一个进程、用 stdin 喂数据、看退出码或 stdout 决定放不放行。所以链路必然是：

```
客户端(本机) → 子进程 → HTTP → hub → WiFi → 设备 → 人按键 → 原路返回
```

`hub/src/bin/ask.rs` 编出的 `kiboard-ask` 就是这个子进程的通用实现，
`clients/<客户端>/` 下是各家的薄壳。

## 统一请求体

`POST /approve`，需要 `X-Api-Key`。

```json
{
  "source": {
    "client":  "kiro-cli",
    "version": "2.16.0",
    "agent":   "kiboard-dev",
    "session": "3B68A769-...",
    "cwd":     "/Users/x/projects/kiboard",
    "host":    "mac-mini",
    "user":    "wellxie"
  },
  "tool": {
    "name":  "execute_bash",
    "input": { "command": "rm -rf build" }
  },
  "intent": {
    "title":  "可选。客户端能给出人类可读的意图就给",
    "detail": "可选"
  },
  "policy": {
    "timeout_s":  120,
    "on_failure": "closed"
  }
}
```

字段规则：

| 字段 | 必填 | 说明 |
|---|---|---|
| `source.client` | 是 | 客户端标识，用于选规则组、上屏、审计。新客户端接入先在这里注册一个名字 |
| `source.cwd` | 建议 | 屏幕上显示项目名（取 basename），多项目并发时靠它区分 |
| `source.session` | 建议 | 同一客户端多会话并发时用于区分 |
| `tool.name` | 是 | **原样透传客户端的工具名**，不要翻译。`execute_bash` 和 Claude Code 的 `Bash` 是两个名字，规则表里分别写 |
| `tool.input` | 是 | **原样透传，hub 不解析结构**。各家字段名不同，规则匹配作用在它的扁平化文本上（见下） |
| `intent.*` | 否 | 缺省时 hub 自己从 `tool` 生成标题 |
| `policy.timeout_s` | 否 | 缺省用 hub 的 `KIBOARD_APPROVE_TIMEOUT_S` |
| `policy.on_failure` | 否 | 仅供审计记录，真正的失败处置在客户端脚本里（见「失败语义」） |

**为什么 `tool.input` 不做结构化**：每加一个客户端就要为它写一遍字段映射，是没完的活。
规则匹配统一作用在 `tool.input` 的 JSON 序列化文本上 —— 对
`{"command":"rm -rf build"}` 就是匹配 `{"command":"rm -rf build"}` 这个字符串。
正则写 `rm\s+-[rf]` 一样命中，且对任何客户端都成立。

旧的简单形态仍然接受（手工触发、调试用），两者可混用：

```json
{ "title": "deploy to prod", "detail": "...", "risk": "high", "timeout_s": 60 }
```

## 统一响应体

```json
{ "ok": true, "id": 9, "decision": "accept", "approved": true,
  "reason": "user pressed 1 on kiboard", "risk": "normal", "rule": "默认" }
```

`decision` 是一个**超集**，客户端按自己的表达力降级：

| decision | 含义 | approved |
|---|---|---|
| `accept` | 人按键批准 | true |
| `auto_accept` | 处于「全部接受」窗口内，自动批准 | true |
| `reject` | 人按键拒绝 | false |
| `timeout` | 超时无人处理 | false |
| `cancelled` | 被 `/cancel` 取消，或设备离线无法展示 | false |

设备离线时 hub 返回 `503` + `decision: "cancelled"`，**不会把调用方吊死**。

## 决策通道对照

这是各客户端适配器唯一需要各写一遍的地方。

| 客户端 | 档位 | 钩子 | 批准 | 拒绝 | 默认失败倾向 |
|---|---|---|---|---|---|
| **Kiro CLI 2.16** | `block` | agent 配置的 `preToolUse` | `exit 0`（=不否决） | `exit 2`，stderr 回给模型 | 其他退出码 = **放行**；超时也 **fail-open** |
| **Kiro CLI v3** | `block` | 工作区 `.kiro/hooks/*.json`，对所有会话生效 | `exit 0` | `exit 2` | 同上。matcher 是**真正则**，与 2.x 相反 |
| **Claude Code** | `block` | `PreToolUse` hook | `exit 0`（CC 仍会自己再问）；或 stdout `hookSpecificOutput.permissionDecision="allow"`（但 deny/ask 规则照样评估） | `exit 2`，stderr 回给模型 | 同上。适配器已写，**未在真实 CC 上实测** |
| **OpenClaw** | `approve` | 不用钩子：原生 exec approvals + Plugin Approval Hooks | 走它的审批通道（真批准） | 拒绝该 exec | 待实测 |
| **hermes** | ? | 未调研 | ? | ? | 按上面五问逐条验过再填 |

拒绝时 **stderr 的内容会进模型上下文**（Kiro 与 Claude Code 都是），
所以适配器要把 hub 返回的 `reason` 写到 stderr —— agent 由此知道"被人拒了、原因是什么"，
可以换方案而不是傻等或重试。这是把审批做成闭环而非单向阻断的关键。

### Claude Code 的三个坑（都有对应 issue）

1. **`permissionDecision` 必须包在 `hookSpecificOutput` 里。** 扁平写成
   `{"permissionDecision":"allow"}` 会被
   [静默丢弃](https://github.com/anthropics/claude-code/issues/48760)：hook 正常退出、
   日志正常打印，但权限系统收不到裁决。
2. **不用 `ask` 档。** 它看着最贴合"问人"，实际有多个已知 bug
   （[不被强制执行](https://github.com/anthropics/claude-code/issues/79356)、
   [静默失效](https://github.com/anthropics/claude-code/issues/81041)、
   [bypassPermissions 下被静默批准](https://github.com/anthropics/claude-code/issues/77212)）。
   而 kiboard 根本不需要它——人已经在实体键盘上答过了，要表达的是**结论**不是提问。
3. **拒绝用 `exit 2` 而不是 `permissionDecision="deny"`。** exit 2 在各版本上被证实可靠。

### 「静默放行」不等于「放行」

这是 Claude Code 相对 Kiro 最容易踩空的一点：`exit 0` 且不输出，只表示 hook 不反对，
**CC 自己的权限系统照旧会再弹一次确认**——实体键盘按完还要在终端再点一次，
设备就白按了。要免掉第二次必须显式回 `allow`，但那等于用 kiboard 的 `rules.toml`
顶替 CC 自己的权限系统。所以做成开关 `KIBOARD_CC_DECISION=passthrough|explicit`，
默认 `passthrough`：**悄悄关掉宿主的安全机制不该是默认行为**。
细节见 `clients/claude-code/README.md`。

### 写 shell 适配器的一个陷阱（真踩过）

变量后面紧跟中文标点时一定写 `${VAR}`：

```bash
echo "异常退出（code=$rc）" >&2   # ✗ UTF-8 locale 下 bash 把「）」算进变量名
echo "异常退出（code=${rc}）" >&2  # ✓
```

`set -u` 下前者报 unbound variable **直接中止脚本、退出码 1**，而 Kiro 对非 0/2
的退出码是「警告后照样执行」——本该 fail-closed 阻止的分支于是变成静默放行。
它**只在 UTF-8 locale 下复现**（C locale 下不触发），所以很难被随手测出来。

写对变量只能修好一处，所以闸门脚本还应加一道兜底，把一类问题一起挡掉：

```bash
trap 'code=$?; if [ "${code}" != 0 ] && [ "${code}" != 2 ]; then
        echo "kiboard: 钩子自身异常退出（code=${code}），已按 fail-closed 阻止。" >&2
        exit 2
      fi' EXIT
```

## 失败语义：必须显式 fail-closed

Kiro CLI 的语义是「非 0/2 退出码 = 显示警告后**照样执行**」，
而**超时后的行为文档没有写明**。这意味着默认倾向是 fail-open：
hub 挂了、网络断了、脚本崩了，工具就直接放行 —— 闸门形同虚设。

所以适配器必须做到：

1. **所有异常分支显式 `exit 2`**（连不上 hub、HTTP 非 2xx、JSON 解析失败、配置缺失）
2. **超时的裁决权留在自己手里**：hook 的 `timeout_ms` 要 **大于** hub 的 `approve_timeout`
   （例如 hub 120s / hook 150s），让 hub 先超时返回 `decision: "timeout"`，脚本再决定。
   绝不能让 hook 自己先超时 —— 那条路的行为未知
3. **逃逸阀要显式**：`~/.kiboard/bypass` 文件存在则放行，并在 hub 审计里记一条
   `bypassed`。不要用"连不上就放行"这种隐式退化 —— 那等于把开关交给网络状况

## 客户端配置

所有适配器共读 `~/.kiboard/config`（权限 600，不进 git）：

```
KIBOARD_URL=http://home.abig.fun:26041
KIBOARD_API_KEY=...
KIBOARD_ON_FAILURE=closed      # closed | open
KIBOARD_TIMEOUT_S=120
KIBOARD_RULES_TTL_S=3600       # 本地规则缓存多久刷一次
```

`kiboard-ask --refresh-rules` 可手动拉取规则。

API key 不要写进 agent 配置 JSON —— 那个文件通常会进 git。

## 风险规则

规则在 hub 侧，见 `hub/rules.toml`。三档：

| risk | 行为 |
|---|---|
| `allow` | 直接放行，**不打扰人**。只读命令走这一档。规则短路发生在设备在线检查之前，所以设备离线也照样放行 |
| `normal` | 上屏等按键，短按 `1` 即可批准 |
| `high` | 上屏 + 黄灯快闪，**必须按住 `1` 达 `KIBOARD_HIGH_HOLD_MS`（默认 900ms）**，且不被「全部接受」放行 |

按住时长由 hub 从 `press` 到 `release` 计时，不依赖固件的 `long` 阈值——
固件在 600ms 就发 `long`，而 600ms 对人手来说区分不开"点一下"和"按住"（实测踩过）。
到点时 hub 会把黄灯转常亮并提示 `release to accept`，给出过程反馈。

阈值取值是在两件事之间平衡：太短则和刻意按稳的点按区分不开、保护形同虚设；
太长则每次批准都要干等，实际用起来嫌烦（1200ms 实测偏长）。默认 900ms。
低于 600ms 会在启动时告警，因为那已经落进点按的区间了。
调这个值**不需要改代码**，重启时给环境变量就行。

优先级：请求里显式给的 `risk` > 规则表首个命中 > 默认 `normal`。

规则按 `client` 分组，因此不同客户端的工具名和参数格式差异被吸收在规则表里，
而不是散落到各个适配器脚本中。

### 规则下发与本地 allow 短路

规则**在 hub 上维护**（中心管理、可审计），但客户端会缓存一份到
`~/.kiboard/rules.cache.toml`，用于**本地判断"这条要不要问人"**：

```
GET /rules  ->  {"etag": "...", "toml": "<规则原文>"}
```

命中 `allow` 档的调用**本地直接放行、不联网**；其余照常发请求，由 hub 定 `normal`/`high`。

为什么要这么拆：规则判断原本全在 hub，意味着每次工具调用都要一个公网 RTT，
而 hub 一挂、网络一抖，fail-closed 会把**所有**命令拦下 —— 包括 `git status`
这种根本不需要问的。把"要不要问"这一半放到客户端，可用性就不再整体绑在 hub 上；
"问得多严"仍然由 hub 定，中心管理没丢。

退化行为（方向都是安全的）：

| 情况 | 行为 |
|---|---|
| 缓存新鲜 | 直接用，`allow` 不联网 |
| 缓存过期 | 先尝试刷新，失败则用旧缓存 |
| 拉不到且有旧缓存 | 用旧缓存（降级但能用） |
| **没有缓存且拉不到** | 不做本地判断，照常走联网审批 → fail-closed 阻止 |

最后一行是关键：**不知道规则时不能自己决定放行**。

客户端能本地判 `allow`，是否削弱了安全性？不。客户端**本来就不是信任边界** ——
能改缓存文件的人也能直接删掉 hook 配置。这个闸门防的是 agent 判断失误或手滑，
不是防有本机写权限的攻击者。

**已知缺口**：本地放行的调用**不会进审计日志**（没联网，写不了）。
所以审计里看到的是"需要人裁决过的"完整记录，而不是"所有工具调用"的全量记录。
要补的话得让客户端把本地放行的计数在下次联网时捎回去。

## 审计

hub 把每一次裁决按行写入 JSONL（`KIBOARD_AUDIT`，默认 `~/.kiboard/audit.jsonl`）：

```json
{"ts":"2026-08-01T12:00:00Z","id":9,"client":"kiro-cli","host":"mac-mini",
 "cwd":"/x/kiboard","tool":"execute_bash","input":"{\"command\":\"rm -rf build\"}",
 "risk":"high","rule":"递归删除","decision":"reject","key":"2","elapsed_ms":4200}
```

一个审批设备没有可回溯的批准记录是不完整的 —— 出事之后要能回答
「这条命令当时是谁批的、什么时候、按的哪个键」。

查询用 `GET /audit`：

```bash
curl -H "X-Api-Key: $KEY" 'http://hub:26041/audit?limit=20'
curl -H "X-Api-Key: $KEY" 'http://hub:26041/audit?decision=reject'
curl -H "X-Api-Key: $KEY" 'http://hub:26041/audit?client=kiro-cli&limit=5'
```

返回里除了明细还有 `summary`（各类裁决的计数）——计数比明细更常用，
看一眼就知道最近是不是一直在被拒，或者自动放行的占比是不是高得不正常。

从文件尾往前读：审计只增不删，而人想看的几乎总是最近发生的事。

响应里带一句 `note` 提醒本地放行的调用不在其中。**不说清楚的话，
看日志的人会以为 agent 只跑了这么几条命令。**

## 状态上报

`POST /state`，和审批是两条完全不同的路径。

```json
{ "source": {...}, "state": "working", "detail": "execute_bash" }
```

`state` 取值：`start` / `working` / `your_turn` / `error` / `idle`。
取值刻意少 —— 设备只有一块小屏和三个灯，分得太细人也看不出差别。

| | 审批 `/approve` | 上报 `/state` |
|---|---|---|
| 语义 | 要人做决定 | 让人知道现在什么情况 |
| 阻塞 | 是 | 否 |
| 失败处置 | fail-closed 阻止操作 | 忽略，**客户端永远 exit 0** |
| 有待批请求时 | 就是它自己 | **不碰屏幕** |

最后一行是刻意的：屏幕在问一个需要决定的问题时，用状态信息去覆盖它是本末倒置。

`GET /state` 可查最近一次上报。只保留一份 —— 设备只有一块屏，
同时显示多个 agent 的状态没有意义。

## 任务列表

`POST /tasks`，语义是**全量替换调用方名下的列表**：

```json
{"source": {"client": "kiro-cli", "host": "...", "user": "..."},
 "tasks": [{"title": "编译固件", "status": "doing"},
           {"title": "还没开始的", "status": "todo"}]}
```

设备上**只显示 `doing`**。待办不上屏：待办是计划，计划随时会变，
而站在设备前的人关心的是"此刻在动什么"。已完成的同理——屏幕只有 4 行，
历史和计划都没有位置。

每行带客户端 tag：`[kiro] 编译固件`。一个 hub 会同时接 kiro / cc / openclaw，
不标出来就不知道这条是谁在做，多客户端下这是必要信息而不是装饰。

### 分桶：api key + agent

桶 id 是 `{api key 指纹}/{client@host}`。

只按 api key 分不行，这是实测撞到的：同一个人的 kiro 和 cc 用的是同一个 key，
后报的会把先报的整份覆盖掉——而这两个恰恰是最需要同时看到的。
api key 只代表**租户**，桶内还要按 agent 分。

key 存的是 FNV-1a 指纹而不是明文：它会出现在 `GET /tasks` 的响应里，
而响应可能被贴进日志或聊天窗口。

租户级上限 100 条，超了淘汰最久没上报的 agent（淘汰整桶而不是截断条目——
半截的任务列表比没有更让人误解）。正常上报是全量替换、不会累积，
这个上限只为防一个写错循环的客户端把 hub 内存打爆。

### 不落盘

hub 重启后任务表为空。这是刻意的：重启后的旧任务大概已经过时，
**显示过时的进度比显示空的更误导**。代价是重启后要等下一次上报才有内容。

### 自动触发点

Kiro CLI 上已经解决：**agent 的待办清单本身是一个工具调用**（`todo_list`），
所以挂 `postToolUse` + `matcher: "todo_list"` 就能在每次清单变更时触发，
而 `tool_response` 带的是整份清单快照，hook 不需要维护状态。
见 `clients/kiro-cli/tasks-hook.sh`。

清单只有 `completed` 布尔值、没有"进行中"，所以约定
**第一个未完成的就是此刻在做的那件**。

`source.session` 必须传：hub 按 session 分桶。同一个 agent 换个工作目录会算成
两个 agent，屏幕上出现重复行（实测撞到过）。

其他客户端还没有对应实现。Claude Code 的 TodoWrite 工具在 `PostToolUse` 上
应该同理可行，但**未实测**。

### 过期

一个跑完就退出的 session 不会来说"我结束了"，它最后那件"正在做"的事会永远挂在
屏幕上。所以 30 分钟没上报的桶不再上屏——待机屏的价值全在于可信，
**显示一件半小时前的事比显示空的更糟**。

## 未来：多租户与持久化（尚未实现）

现在 hub 是单机单用户的：一个 api key、状态与任务全在内存、审计是本地 JSONL。
如果要做成能给多人用的服务，缺口按依赖顺序是：

1. **数据库**。任务、状态、审计、规则都从内存/文件搬到库里。
   审计是唯一已经落盘的，但格式是追加式 JSONL，不支持按租户查。
2. **api key 作为租户身份**。任务表已经按 api key 指纹分桶，这个接缝是现成的；
   规则表（`GET /rules`）和审计还是全局的，要按租户切开。
3. **session 与任务的状态跟踪**。现在 `source.session` 只是透传上屏和写审计，
   没有生命周期。要能回答"这个 session 做了哪些事、每件事结果如何"，
   需要把审批记录、状态变迁、任务变更都挂到 session 上。
4. **设备与租户的绑定**。现在 `/device` 只有一个 token，任何持有它的设备都能收全部请求。
   多租户下要一台设备只看到自己租户的东西。

刻意先不做：单用户场景下这些只增加复杂度。但上面第 2 条的分桶接缝已经留好，
第 4 条是唯一有安全含义的——多租户之前必须先解决设备与租户的绑定。

## 加一个新客户端要做什么

1. **先跑上面的[五问核对表](#接一个新客户端先回答这五个问题)**，尤其第 3、5 条要亲手验，
   并把结论填进「决策通道对照」表
2. 定它的 `enforcement` 档位（`block` / `approve` / `observe`），随请求上报
3. `clients/<name>/` 下写适配器：读它的输入格式 → 调 `kiboard-ask` → 翻译 decision
4. `hub/rules.toml` 里为它的工具名补规则组
5. `source.client` 用一个新名字，便于上屏和审计区分
6. **确认它的"委派/子 agent"类工具也被纳入闸门**。Kiro CLI 实测确认子 agent 的工具调用
   不经过父 agent 的 hook，别的客户端很可能同理——不拦委派的话，把危险命令交给子 agent
   就绕过了整套审批

hub 侧不需要改代码。如果需要改，说明契约设计漏了东西，先回来改这份文档
和 [`../protocol/`](../protocol/)。
