# 协作总线（实时协作）设计

状态：第 1 阶段原型已实现（`crates/sa-bus`，Claude 与 Codex 在完全访问 / 自动档位挂载），尚未做真实模型实测。设置环境变量 `SA_COLLAB_BUS=0` 可关闭。本文记录目标、协议、各执行端挂载方式、已验证事实与未决问题。

## 目标

现有协作是“派工—整轮执行—轮末回报”：子任务在一整轮结束前无法与主控或其他协作者交流；轮中留言只有 Codex 能收到（`turn/steer`）。协作总线让同一工单内的所有 agent 在**工作途中**互相发消息、查看彼此状态、发布中间成果，不必等对方结束一轮。

非目标：不替代 `SA_SEND` 派工与轮末回报；不改变权限档位、写入目录协调和暂停门的语义。

## 为什么用 MCP

协作层必须对任意 agent CLI 通用。MCP 是各执行端都已支持的工具接口：Codex、Claude Code、agy、DeepSeek Harness 都能挂载 MCP 服务，ACP 协议的 `session/new` 也能直接传入 MCP 服务。平台自己提供一个 MCP 服务，就不必为每种 CLI 单独实现协作协议。

## 架构

```
工作台进程
├─ Engine（调度、信箱、事件表）
└─ CollabBus（rmcp Streamable HTTP 服务，只监听 127.0.0.1）
     ▲  每轮准入时为该任务签发令牌 → RunRequest.bus
     │
  执行端子进程（Claude / Codex / …）以 MCP 客户端身份连接，携带 Bearer 令牌
```

- 服务随工作台启动，监听回环地址的随机端口，拒绝非回环 Host（rmcp 默认的 DNS 重绑定防护）。
- **令牌按任务、按轮签发**：准入时生成随机令牌，绑定 `(ticket, job, turn, agent)`；轮次结束即吊销。令牌只出现在子进程环境或该轮命令行配置中，不写入事件表。
- 所有消息复用现有信箱（`mailbox` + `events`），因此重启续跑、去重、暂停门、界面显示都沿用已有机制。引擎每 80 ms 轮询准入，总线写入的消息无需额外唤醒。

## 工具

| 工具 | 输入 | 行为 |
| --- | --- | --- |
| `roster` | 无 | 列出本工单的任务：编号、上级、执行端、角色、状态、最近一条阶段汇报，并标出调用者自己 |
| `send` | `to`（任务编号，或 `lead` / `parent`），`body` | 以调用者身份写入目标任务的信箱。目标正在运行时，支持轮中插入的执行端会立即收到；目标在等待时会被唤醒 |
| `advise` | `brief` | 请教工单的顾问：另一个执行端、另一家模型。顾问在工单项目目录以**只读档位**运行一轮，可读文件、搜代码核实简报，返回建议；最长 14 分钟 |
| `ask` | `to`，`question`，可选 `wait_seconds` | 发出问题并在本轮内等待对方 `reply`（默认 45 s，最多 55 s，低于 Codex 与 DeepSeek Harness 默认 60 s 的 MCP 工具超时）。超时返回未回答，之后的回答进入收件箱。对方正在等你（或形成等待环）时拒绝，避免双方互相卡住 |
| `reply` | `question_id`，`body` | 回答发给自己的问题；提问方的 `ask` 立即返回答案，不再在收件箱重复出现 |
| `inbox` | 无 | 返回调用者未读的消息并标记已读 |
| `claim` | `paths` | 声明将要修改的文件或目录（项目相对或绝对路径，目录覆盖其下全部内容）。与其他运行中任务的声明重叠（同一路径或上下级）时不登记该路径，返回持有者并通知对方。Windows 上比较不区分大小写 |
| `release` | 可选 `paths` | 释放指定声明，省略时释放全部；轮次结束时自动释放 |
| `publish` | `id`，`summary` | 发布阶段性成果，等同 `SA_PROGRESS`，界面与其他协作者可立即看到 |
| `tools` | 可选 `server`、`tool` | 列出本工单已授权、当前可用的 MCP 工具（描述只给预览）；指定服务与工具时返回完整描述和参数 schema |
| `call` | `server`，`tool`，`arguments` | 在调用者本轮内同步调用一个已授权工具并返回结果 |

每个工具结果都附带调用者当前的未读数，提示 agent 读取，避免反复空轮询。

### 跨执行端顾问

思路同 Anthropic 的 [advisor tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool)（便宜模型干活、关键处请教强模型），但不限于 Anthropic 模型：任何挂载总线的 agent 都能请教任意执行端上的任意模型。

- **顾问的选择**：工单单独设置 > 全局默认 > 主控当前路由（`native_routes` 中根任务的路由与执行端）。在模型窗口的“顾问”分段设置：默认“跟随主控”，关闭后选择执行端、供应商、模型和思考强度。存储在 `bus_advisor` 表（`global` 或 `ticket:<id>`）。
- **执行方式**：`Engine::consult` 用现有适配器跑一次性轮次，`profile=readonly`、不挂总线、不续接会话。提示词为固定的 `advice::GUIDE` 加调用方简报，固定前缀利于缓存。调用方各自的对话记录平台拿不到，所以要求调用方写自包含简报；顾问能自己读项目文件弥补。
- **记录**：调用方任务名下记 `advice.started`（顾问、简报字节数与摘要）与 `advice.result`（耗时、建议正文或错误）。顾问自身的原生事件不入账本，因此顾问的 Token 用量尚未计入工单用量统计。
- **等待**：一次顾问轮次可能要几分钟。平台为总线设置更长的工具超时：Codex `tool_timeout_sec=900`，DeepSeek Harness `toolCallTimeoutMs=900000`，Claude 在低于该值时设 `MCP_TOOL_TIMEOUT=900000`；引擎对顾问轮次的上限为 14 分钟。agy 的 ACP 通道超时未调整。
- 与 Anthropic advisor 的区别：后者在一次 API 请求内完成、能看到完整对话但不能用工具；这里是独立的只读轮次，只看到简报但能检查项目。

### 工具开放给所有 agent

此前 MCP 工具只进入主控的上下文（`project_extensions.mcp` 仅在根任务填入），子任务无法使用；技能正文已对所有任务下发。总线的 `tools` / `call` 让每个挂载总线的任务都能用工单授权的工具，检查与 `SA_TOOL` 相同：工单授权范围、服务启用且版本未变、工作目录范围、私有连接池；调用记录为调用任务名下的 `mcp.started` / `mcp.result`（`via: bus`）。区别是结果在本轮内直接返回，不写 `SA_TOOL` 回执、不触发额外一轮。总线没有审批通道，因此只在完全访问 / 自动档位允许 `call`；其他档位仍由主控用 `SA_TOOL` 走审批。

### 缓存命中

总线进入各执行端提示前缀的只有固定的 6 个工具定义和一段说明。它们不含令牌、任务号、未读数等每轮变化的数据，测试会逐字节比较不同任务、不同工单拿到的定义。工单的 MCP 工具不逐个注册成执行端工具（那会让工具目录变化直接改变每个 agent 的前缀，也会在目录很大时占满上下文），而是通过固定的 `tools` / `call` 按需查询，与平台“按需读取 MCP 参数”的做法一致。令牌放在 HTTP 请求头里，不进入提示。

声明是协作提示，不是文件锁：平台不会阻止未声明的写入；写入目录的串行与隔离工作区规则保持不变。`roster` 会列出每个任务当前的声明。

## 送达：拉取 + 强制注入

只靠模型自觉调用工具不够可靠，因此分两层：

1. **每轮引导注入**（已实现，完全访问 / 自动档位）：引擎在引导末尾追加固定文本 `guides::BUS`，平台引导中写明总线工具与使用时机——开始前看 `roster`，完成阶段成果就 `publish`，在长操作前后检查 `inbox`，需要别人配合时直接 `send`。
2. **执行端级强制注入**（不依赖模型主动调用）：
   - Codex：已有 `turn/steer`，信箱新消息直接插入当前轮。
   - Claude Code：适配器保持双向消息流的 stdin 打开，**只在工具执行期间**把未读消息写成一条用户消息。实验确认（2.1.323）：工具运行时写入的消息会在工具结果之后、同一轮的下一次模型请求里以“用户在你工作时发来新消息”的形式出现，仍只产生一个结果；不在工具执行期间写入则会另起一轮，因此不这样做。读到最终结果后关闭 stdin。该推送对所有档位生效，主控留言、暂停收尾通知也因此能在轮中送达 Claude。
   - agy、DeepSeek Harness：暂无轮中注入，依靠引导与工具结果中的未读提示。

## 各执行端挂载方式（已验证事实）

隔离验证：已安装 CLI 对回环假模型服务和测试 MCP 服务，不访问真实模型（2026-11-14）。

- **Claude Code 2.1.323**：MCP 工具默认被延迟到工具搜索之后，模型看不到就不会用；服务配置加 `"alwaysLoad": true`、工具加 `_meta` 的 `anthropic/alwaysLoad` 后，总线工具直接出现在模型请求里。令牌用 `headers` 中的 `${SA_BUS_TOKEN}` 从子进程环境展开。`--mcp-config '{"mcpServers":{"sa_bus":{"type":"http","url":…,"headers":{…}}}}'` 可在 `--strict-mcp-config` 下挂载，工具名为 `mcp__sa_bus__<tool>`。`--safe-mode` 会关闭全部 MCP，因此受限档位（当前使用 `--safe-mode`）无法挂载；去掉 `--safe-mode`、保留 `--tools` 限制与 `--setting-sources ""` 时可以挂载。`dontAsk` 权限模式下需要 `--allowedTools mcp__sa_bus__*` 放行。
- **Codex 0.169.0**：`-c mcp_servers.sa_bus.url="…"` 被识别为 `streamable_http`，支持 `bearer_token_env_var` 从子进程环境读取令牌。Codex 把整个服务作为一个 `namespace` 工具（`mcp__sa_bus`，内含 `roster`/`send`/`inbox`/`publish`）发给模型，并用服务的 `instructions` 作为命名空间说明，因此总线引导文字能直接送达模型。
- **agy**：`--settings` 拒绝 `mcpServers`（`unknown setting`），`agy mcp add` 会写入用户的 `~/.gemini/config/mcp_config.json`，都不适合按次挂载。完全访问 / 自动档位、Google 账号来源的轮次改走已有的 `agy acp` 通道，在 `session/new` / `session/load` 的 `mcpServers` 中传入总线（令牌放在 stdio 上的请求里，不上命令行）。API 与 ADC 来源的 ACP 认证尚未接入，这些来源暂不挂载。agy 通过通用的 `call_mcp_tool` 调用 MCP 工具，init 中不逐个列出工具名。
- **DeepSeek Harness 0.2.2-rc.1**：在平台本就生成的 `--patch` 策略文件中追加 `@deepseek-ai/dsh-mcp-client` 条目（`transport: streamable-http`，`Authorization` 头用 `!!js` 从子进程环境读取令牌）。工具名同样是 `mcp__sa_bus__<tool>`，服务说明会进入系统提示。
- **通用 ACP 服务**：协议支持 `mcpServers`，平台的通用 ACP 执行端尚未传入（后续）。

当前只在**完全访问 / 自动**档位挂载总线（Claude、Codex、DeepSeek Harness、Google 账号来源的 agy）。受限档位是否为总线去掉 `--safe-mode` 需要单独评估：`--safe-mode` 还关闭了其他扩展点，总线本身不触碰文件系统，但改动该参数会改变受限档位的安全边界。

## 安全

- 只监听回环地址，校验 Host；每个请求必须带有效令牌，令牌只能访问自己工单内的任务。
- 消息正文长度上限 16 KiB；每工单总线消息预算独立于 `SA_NOTE` 的 128 条预算。
- 只读工单仍可收发消息（消息本身不写文件），但原型阶段只在高权限档位挂载。
- 暂停门：工单请求暂停后，`send` 仍可用于交接，被暂停的任务不会因新消息被准入（沿用现有准入闸门）。

## 风险与局限

- MCP 工具需要 agent 主动调用；正在执行长命令的 agent 要等命令结束才能处理消息，只有轮中注入能缩短这段时间。
- 各模型是否恰当使用协作工具差异很大，引导需按执行端调整，并用实测确认。
- 频繁读取收件箱消耗 Token；默认依靠结果中的未读提示而不是轮询。
- 总线消息写入工单库，长期运行会增加事件量；需要观察增长并考虑归档策略。

## 已完成的验证（2026-11-14）

- `sa-bus` 单元测试 5 项：消息带发送者身份写入信箱、未读计数、跨工单 / 发给自己 / 过期轮次被拒、发布进展与去重、经 rmcp 客户端的真实 HTTP 往返（吊销后拒绝）。
- `SA_LOCAL_GATEWAY_CLI=1 … test -p sa-adapters --test bus_cli`：已安装的 Claude Code 与 Codex 经 Super Agent 适配器启动，回环假模型要求调用一次总线 `send`；两者都把消息写进了上级任务的信箱，发送者分别为 `claude (#2)` 与 `codex (#2)`。不访问真实模型。
- 桌面程序在临时数据目录启动总线并正常运行。

- **真实模型实测（计费，2026-11-14）**：`SA_LIVE_BUS="大核桃 · claude,deepseek-v4.1-flash" … test -p sa-adapters --test bus_cli live_two -- --nocapture`。两个 Claude Code 任务并发运行：持有者循环检查收件箱，工作者通过总线索要暗号。时间线（以第一条总线事件为 0）：
  - +0.0 s 工作者 `send` 请求给 #3；
  - +5.5 s 持有者 `inbox` 读到；
  - +9.6 s 持有者 `send` 回复暗号；
  - +16.2 s 工作者 `inbox` 读到；
  - +19.3 s 工作者 `publish` 结果。
  两轮分别在 40 s、41 s 内结束，工作者最终回复正确暗号。一次完整问答约 16 s，全部发生在两个任务各自的轮中，没有经过轮末回报或主控转派。延迟主要由轮询间隔（提示词中的 `sleep 5`）和单步模型响应时间决定。
- **跨执行端实测（计费，2026-11-14）**：通用配对测试 `live_pair_exchanges_a_code_word`，用 `SA_LIVE_BUS_KEEPER` / `SA_LIVE_BUS_WORKER` 指定执行端、来源和模型（格式见测试源码），宿主与引擎一样把未读消息交给执行端做轮中推送。
  - Claude 持有者 ↔ Codex 工作者（都用大核桃 `deepseek-v4.1-flash`，Codex 经自定义供应商直连大核桃的 Responses 接口）：+0.0 s Codex `send`，+8.1 s Claude 读到，+13.8 s Claude 回复，+14.2 s Codex 收到（距回复 0.4 s，与 `turn/steer` 轮中推送一致；当时 `inbox` 与推送的已读记录尚未区分，现在分别记为 `via: bus` / `via: native`），+52.0 s Codex 发布结果。两轮分别用时 59 s、146 s，工作者回复正确暗号。
  - Claude 持有者 ↔ DeepSeek Harness 工作者：DeepSeek Harness 经 `dsh-mcp-client` 连上总线并发出请求（发送者 `deepseek (#2)`），但从启动到第一次 `send` 用了约 11 分钟，持有者已按提示结束轮询，测试被手动终止。挂载已验证，协作往返未验证。
  - agy：当天 agy 无法完成请求，未实测；ACP 挂载只有离线编译与代码路径，没有真实验证。
- **第 2 阶段（2026-11-14）**：
  - Claude 轮中推送：`SA_LOCAL_GATEWAY_CLI=1 … --test bus_cli claude_receives_a_note_while_its_tool_runs`，真实 Claude CLI 执行 4 秒命令期间写入的留言出现在同一轮工具结果之后的请求里，只有一个结果，留言标记已读。
  - `ask` / `reply`、`claim` / `release`：`sa-bus` 单元测试覆盖问答、反向等待拒绝、超时后迟到回答进入收件箱、路径重叠拒绝与通知、轮次结束释放。
  - 引擎测试 `tests/bus.rs`：完全访问轮次拿到令牌和引导，只读轮次没有引导，每个令牌都被吊销。
  - 计费实测 `live_pair_asks_and_replies`（两个 Claude 任务，`dahetao · claude-desktop` 的 `qwen3.8-27b`，测试用干净的 `CLAUDE_CONFIG_DIR`，因为加载全局 MCP / 插件后提示超出该模型上下文）：+0.0 s 工作者 `ask`，+0.1 s 持有者读到，+11.4 s `reply`，+11.7 s 工作者拿到答案，+14.9 s 发布；两轮 27 s / 42 s。这一轮答案同时经 `ask` 返回和 Claude 轮中推送送达（`via: native`），重复了一次；之后改为 `bus-r-*` 回答不参与推送，该修复只有代码与离线测试，没有再次计费验证。随后两次重试中，持有者的首个请求都被上游以“超出上下文”拒绝，工作者按提示超时、催促并向主控报告未收到，属于上游不稳定。
- **跨执行端顾问（计费，2026-11-15）**：`live_cross_harness_advise`。工作者为 Claude Code（`大核桃 · claude` 的 `deepseek-v4.1-flash`），被要求不能自己读文件；顾问为 Codex（大核桃 Responses 接口直连的 `deepseek-v4.1-flash`）。工作者调用 `advise` 后，顾问只读读取项目中的 `RELEASE_NOTE.txt`，19.5 s 返回“发布码 ORCHID-7731，已读取文件核实”；工作者 27.5 s 回答正确。同一测试先用 `qwen3.8-27b` 作工作者时，上游以“超出上下文”拒绝首个请求。
- 同一天先用 `gemini-4-hydrogen` 实测失败：上游返回 500、404、429（额度耗尽）和超时，两个任务都停在第一次模型请求，与总线无关。

未验证：agy 与 DeepSeek Harness 的完整协作往返；由主控派工产生的真实工单中，模型是否会在没有明确指令时主动使用总线；更长任务中的消息量与 Token 成本。

## 验证计划

1. 离线：总线工具单元测试（令牌校验、跨工单拒绝、消息落库、未读计数、吊销）。
2. 真实 CLI、回环假模型：确认 Claude 与 Codex 在适配器启动参数下实际连上总线并列出工具。
3. 计费实测（需用户同意）：两个真实执行端在各自轮中对话，测量“发送→对方处理”的延迟，与现有轮末回报对比。
