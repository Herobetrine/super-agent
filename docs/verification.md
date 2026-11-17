# 验证方法

## 协作闭环（2026-11-17）

实现、规则与边界见 [协作闭环](dev/collaboration-loop.md)。

- `build --workspace`、`test --workspace`（630 通过、0 失败、6 忽略）、`clippy --workspace --all-targets -- -D warnings` 通过。此前用量调度器一轮为 562 通过。
- 新增离线测试（假执行端经真实调度器，不调用模型）：
  - `sa-engine/tests/collab_loop.rs` 23 项：结论行的状态效果、`resolved`、无效行回执与终稿最后一条生效；提问后停驻、多轮调度不再准入、回答送达且只能回答一次、重复与替换、追加指令撤回、有未解决子任务时仍停驻、自动模式主控提问不推进阶段；续派沿用平台记录的原生会话标识并拒绝不合法目标；依赖派工的等待、前置失败后跳过、改派链跟随、不占槽位；以上状态在关闭并重开引擎后保持。
  - `sa-engine/tests/completion_checks.rs` 13 项，使用真实临时 Git 仓库和真实子进程：通过、退回原会话后通过、三次失败记为部分完成、有未了结退回时文件未变也重跑、沿用上次通过结果、只读任务与非完成声明不触发、停止工单时终止检查进程树、配置校验。
  - `sa-engine/tests/stall_handling.rs` 4 项：重复的下一步触发平台提问后不再调用执行端，回答“继续”恢复，回答“停止”取消工单，重启后保持。
  - `sa-engine` 单元测试：工作树指纹覆盖未跟踪文件内容和二进制改动。
  - `sa-store`：新快照字段、`attention`、缺少新表的旧工单库、排队计数排除等待前置的任务。
  - `sa-desktop`（egui_kittest 经无障碍树驱动）：点选项和输入回答会提交回答动作，观察与演示模式没有控件；侧栏“待你处理”只在有内容时出现且不重复列出；协作页的等待前置、结论和平台检查文案；完成检查编辑器的增删保存与 4 条上限；问题通知的启动基线与新问题。
- 本机已安装的 Claude Code 2.1.325 与 Codex 0.169.0 对回环假模型：`bus_cli` 7 项与 `gateway_compat` 1 项通过，说明加长后的引导文字没有破坏请求前缀检查和总线工具调用。其中 3 个 `live_*` 因环境开关未设置而提前返回，不算真实模型验证。
- 真实工单库经只读 SQLite backup 复制后，`SA_LEGACY_COPY=… test -p sa-desktop legacy` 通过；副本原本没有本轮新增的表。没有在真实库上启动测试调度器。
- 界面截图已查看：`--demo --inspector collab` 亮色与暗色 1280×860、880×600，以及 1440×900 对话视图。可见“待你处理”分组、问题卡片、结论与平台检查行、“等待 #103 完成”。演示模式只读，卡片上的选项按钮和输入框、完成检查编辑器的展开状态只由界面测试覆盖，没有截图。

未验证：真实模型是否会按引导写出 `SA_REPORT`、`SA_ASK`、`continue` 和 `after`；真实执行端下续派对原生会话的实际复用；主控轮次、用量和产出质量的实际变化；Windows 系统通知“需要你的回答”的实际弹出；用户验收。

专项复验：

```powershell
scripts\cargo.cmd test -p sa-engine --test collab_loop --test completion_checks --test stall_handling
scripts\cargo.cmd test -p sa-store --test collaboration --test completion_checks --test queued_projection
scripts\cargo.cmd test -p sa-desktop ui_tests::
scripts\cargo.cmd test -p sa-desktop workflow_tests
target\debug\super-agent.exe --demo --inspector collab --screenshot collab.png --size 1280x860
```

## 用量调度器与读路径（2026-11-17）

实现与边界见 [用量调度器](dev/usage-governor.md)。同一批改动还包含界面读路径：读线程按账本变更计数刷新（40 ms 检查一次，不再固定每秒轮询）、Markdown 解析结果跨刷新复用，以及流式遥测批次以 `synchronous=NORMAL` 提交（见 [引擎规格 §1.1](dev/engine-spec.md)）。

- `build --workspace`、`test --workspace`（562 通过、0 失败、6 忽略）、`clippy --workspace --all-targets -- -D warnings` 通过。其中 3 个 `live_*` 总线测试因环境开关未设置而提前返回，不算真实模型验证。
- 新增离线测试：`sa-engine/tests/usage_governor.rs` 16 项（槽位上限与多工单公平、额度等待后沿用同一原生会话续跑、超过 6 小时放弃、12 次等待上限、主控保留只拦同供应商协作者、远期周窗口不拦截、等待只通知上级一次、`usage_budget` 取整）；`sa-store` 中窗口解析、过期和快照相关的 13 项；Claude 与 Codex 适配器的额度帧处理；桌面设置面板、页脚、事件文案。短窗口续跑测试在测试库里提前了 `ready_after`，没有真实等待 60 秒的重置余量。
- 本机已安装的 Claude Code 2.1.325 与 Codex 0.169.0 对回环假模型：`SA_LOCAL_GATEWAY_CLI=1 … --test bus_cli` 中 4 项回环测试与 `--test gateway_compat` 通过，说明真实 Codex app-server 在 `turn/start` 之前接受新增的 `account/rateLimits/read` 请求，Claude 的请求前缀检查不受新引导文字影响。不调用真实模型。
- 用本机 `codex app-server` 直接请求一次 `account/rateLimits/read`（不调用模型），返回 `primary.usedPercent`、`windowDurationMins`、`resetsAt`，解析测试使用了该实际形状。
- 真实工单库经只读 SQLite backup 复制后，`SA_LEGACY_COPY=… test -p sa-desktop legacy` 通过（副本原本没有 `quota_windows` 表）。没有在真实库上启动测试调度器。
- 界面：`--dialog efficiency` 与 `--demo --inspector collab` 截图已查看（亮色、暗色，1280×860 与 880×600）。演示数据里的两个额度窗口是示例值。

未验证：真实订阅额度耗尽后的等待与自动续跑；Claude `rate_limit_event` 帧的真实样本（本机 Claude 走本地网关，账本最近 40 万条事件里没有这类帧，字段名取自已安装 CLI 的类型定义）；引导文字对主控实际派工数量、质量和用量的影响；用户验收。

专项复验：

```powershell
scripts\cargo.cmd test -p sa-engine --test usage_governor
scripts\cargo.cmd test -p sa-store quota
scripts\cargo.cmd test -p sa-desktop efficiency_tests
$env:SA_LOCAL_GATEWAY_CLI='1'
scripts\cargo.cmd test -p sa-adapters --test bus_cli -- --nocapture
Remove-Item Env:SA_LOCAL_GATEWAY_CLI
```

## 社区能力集成验收（2026-11-09）

本轮实现入口和边界见[社区机制吸纳](dev/community-integration.md)。`build --workspace`、`test --workspace`、`clippy --workspace --all-targets -- -D warnings` 已通过；环境门控提前返回的真实调用不计为实测。新增针对实际 App 的分支确认、取消、选中新工单、迟到回执和重复提交回归通过。离线场景覆盖真实临时 Git、Windows 子进程树、SQLite 重启和 MCP/ACP stdio 进程，不以模型自述代替断言。

单独执行的真实验证：DSH `0.2.2-rc.1` 读文件、项目内写入、实际项目外尝试被拒、usage、终稿字节和同会话恢复通过；旧 `0.2.1-rc.5` 读文件与恢复回归通过。SCM 通过既有 `gh` 登录只读获取本仓库 PR #1，并在临时工单中将观察送达假执行端；未修改远端。真实旧库由只读 SQLite backup 复制后，读取 5 工单/66 任务/251 原生任务，以及 3 个旧工单假执行端续跑通过；没有在真实库上启动测试调度器。

本地证据目录：`%USERPROFILE%\.local\share\superagent\checks\community-integration-20261109\`，含全量日志、桌面截图和最终回执；DSH 边界证据 `checks/dsh-upgrade-CSAj7c/receipt.json`；旧库证据 `checks/legacy-audit-imrflt9y/compatibility-receipt.json`。截图已检查，人工验收、任意第三方 ACP 沙箱、非 Windows 平台，以及真实模型质量提升均不在此次通过声明中。

专项复验：

```powershell
scripts\cargo.cmd test -p sa-engine --test workspaces --test workspaces_edges --test checkpoints --test scm --test scm_delivery --test stuck
scripts\cargo.cmd test -p sa-desktop workflow_tests
scripts\cargo.cmd test -p sa-desktop ui_tests::
scripts\cargo.cmd test -p sa-desktop host::tests
scripts\cargo.cmd test -p sa-desktop review::
scripts\cargo.cmd test -p sa-store --test evaluation
scripts\cargo.cmd test -p sa-adapters --test acp
# 另行联网，只读取该 PR，不调用模型；使用可访问的实际 PR URL
$env:SA_LIVE_SCM_URL='https://github.com/owner/repo/pull/123'
scripts\cargo.cmd test -p sa-engine --test scm_live -- --ignored
Remove-Item Env:SA_LIVE_SCM_URL
```

离线测试通过只证明协议与状态契约；本机 CLI 能否登录、实际推理和遵守权限需要分别做真实验证。以下命令在仓库根目录的 PowerShell 中运行；`scripts\cargo.cmd` 会准备 Windows GNU 工具链（见 [Rust 重构路线](rust-port.md#构建)）。

## 自动化回归

```powershell
scripts\cargo.cmd build --workspace           # 同时生成 MCP 测试服务 sa-ext-fixture-mcp
scripts\cargo.cmd test --workspace
scripts\cargo.cmd clippy --workspace --all-targets
```

- `sa-engine`：工单与任务状态机、`SA_*` 控制行、信箱、审批、持续运行、断线重试与熔断、供应商轮换、工单库独占、项目知识库（假执行端）。
- `sa-adapters`：四个执行端的目录发现、事件流、权限映射、取消后不残留子进程、stderr 不外泄、自定义供应商与 HTTPS 探测（本机夹具）。
- `sa-ext`：技能扫描、MCP stdio 与回环 HTTP、授权与撤销、幂等回执、取消后服务进程退出。
- `sa-desktop`：用 egui_kittest 通过无障碍树驱动真实窗口——输入、发送、审批、计划、模型选择、供应商与轮换、MCP 与技能、知识库；另测被占用的工单库只能只读观察。
- `sa-store --test usage` 覆盖各执行端的累计去重、续接、失败、缺失字段、限批扫描及跨分页汇总；速率按可配对执行的输出和耗时加权，缓存命中率检查未知字段、零分母和缓存桶语义。
- `sa-desktop usage::tests` 验证精确用量、Token/s 和缓存比例只在悬停时出现，移开后隐藏、点击不会固定详情，窄布局入口不溢出。真实 App 的追加图片回归同时核对用量入口位于输入框下方。
- 图片测试覆盖快照校验、Codex/Claude 协议块、续接替换、并发追加与改派能力过滤。桌面测试从输入区拖入图片并发起后续消息，验证快照字节和草稿保留。
- `sa-desktop review::tests` 使用临时 Git 仓库验证逐文件预览、中文与重命名、检查点捕获/重开/对比、内容变更使审阅标记失效，以及截断后的审阅限制。
- `sa-engine --test routing` 验证协作者模型/强度授权、主控模型固定、运行中策略变化、重开与提交前撤权；`sa-adapters --test catalog_paging` 验证完整分页、同源认证及不完整目录报错。桌面模型测试覆盖搜索、思考强度、供应商往返和目录请求编号。

Windows 原生通知可单独做实机检查，会以临时应用身份显示合成通知并查询系统通知历史，结束时清理测试登记与快捷方式。

```powershell
scripts\cargo.cmd test -p sa-desktop windows_toast_reaches_notification_history -- --ignored --nocapture
```

通知历史通过证明 Windows 已接受该通知。点击导航和系统免打扰策略需要另外观察，真实模型视觉理解也需要独立的计费图片挑战。

## 界面截图

截图模式渲染后保存 PNG 并退出，默认不调度：

```powershell
target\debug\super-agent.exe --demo --screenshot demo.png --size 1440x900
target\debug\super-agent.exe --data <临时目录> --schedule --dialog models --screenshot models.png
target\debug\super-agent.exe --observe --ticket 5 --inspector knowledge --screenshot ticket.png
```

## 旧工单库续跑

不要对正在使用的工单库直接测试。先用 SQLite 备份接口复制一份（例如 `python -c "import sqlite3;s=sqlite3.connect('file:<原库>?mode=ro',uri=True);d=sqlite3.connect('<副本>/hub.sqlite3');s.backup(d)"`），再运行：

```powershell
$env:SA_LEGACY_COPY='<副本目录>'; scripts\cargo.cmd test -p sa-desktop legacy -- --nocapture
```

它在副本上启动调度器，检查恢复后没有遗留运行中任务，并给已完成的非持续工单追加指令续跑（假执行端，不调用模型）。

## 真实调用（计费）

下列开关会实际请求模型，只在临时项目中运行；用完清除环境变量。

| 开关 | 命令 | 检查 |
| --- | --- | --- |
| `SA_LIVE_CATALOG=1` | `scripts\cargo.cmd test -p sa-adapters --test catalog_live` | 只读探测四个执行端的供应商与模型目录 |
| `SA_LIVE_TURNS=1`（可选 `SA_LIVE_AGENTS=codex,claude`） | `… -p sa-adapters --test turns_live -- --nocapture` | 每个执行端读挑战文件、续接同一原生会话 |
| `SA_LIVE_ENGINE=1` | `… -p sa-adapters --test engine_live -- --nocapture` | DeepSeek 主控向其余三个执行端派工，核对回报与写者文件字节 |
| `SA_LIVE_UI=1` | `… -p sa-desktop live_composer -- --nocapture` | 从窗口输入框发起一轮并核对答案 |
| `SA_LIVE_EXT=1` | `… -p sa-desktop live_mcp -- --nocapture` | 批准后的 MCP 调用、`SA_MEMO` 写入知识库、完成后自动记进展 |
| `SA_LIVE_PROVIDER=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_custom -- --nocapture` | 自定义供应商探测与一轮读文件 |
| `SA_LIVE_ROTATION=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_rotation -- --nocapture` | 首选供应商认证失败后按轮换改用下一条路由 |

`KEY_ENV` 只写环境变量名，不要把密钥写进命令或文件。回执默认保存在当前用户的 `.local/share/superagent/checks`，只存哈希。完整事件可能含提示、模型回复、项目路径、会话标识和工具结果，检查时宜使用合成数据。

## 权限

复验权限时测量文件系统的实际变化，不要只看模型自述。一次 Windows 实机验证观察到：DeepSeek 在批准受限写入轮次后只能写项目内目标；Claude 的受限工具清单未开放 Write；Antigravity 的受限写入尝试也未产生目标文件；在完整权限下三个执行器都曾写入项目外专门准备的兄弟目录。完整权限与自动模式只适合在隔离账户或测试机上验证。
