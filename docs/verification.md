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
| `SA_LIVE_COLLAB=<scenario.json>` | `… -p sa-adapters --test collab_live live_collaboration_scenario -- --exact --nocapture` | 在独立项目与新工单库中运行协作验收；支持 Claude / Antigravity，固定指定模型 |
| `SA_LIVE_TURNS=1`（可选 `SA_LIVE_AGENTS=codex,claude`） | `… -p sa-adapters --test turns_live -- --nocapture` | 每个执行端读挑战文件、续接同一原生会话 |
| `SA_LIVE_ENGINE=1` | `… -p sa-adapters --test engine_live -- --nocapture` | DeepSeek 主控向其余三个执行端派工，核对回报与写者文件字节 |
| `SA_LIVE_UI=1` | `… -p sa-desktop live_composer -- --nocapture` | 从窗口输入框发起一轮并核对答案 |
| `SA_LIVE_EXT=1` | `… -p sa-desktop live_mcp -- --nocapture` | 批准后的 MCP 调用、`SA_MEMO` 写入知识库、完成后自动记进展 |
| `SA_LIVE_PROVIDER=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_custom -- --nocapture` | 自定义供应商探测与一轮读文件 |
| `SA_LIVE_ROTATION=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_rotation -- --nocapture` | 首选供应商认证失败后按轮换改用下一条路由 |

`KEY_ENV` 只写环境变量名，不要把密钥写进命令或文件。回执默认保存在当前用户的 `.local/share/superagent/checks`，只存哈希。完整事件可能含提示、模型回复、项目路径、会话标识和工具结果，检查时宜使用合成数据。

协作验收可先用 `python scripts/prepare-collab-acceptance.py deps <不存在的运行目录> --agent antigravity --provider google --model <实时目录中的精确型号>` 准备项目，再将环境变量指向生成的 `scenario.json`。`deps` 测实现、独立审查和后续需求；`checks` 测正常完成检查；`bounce` 通过分阶段交付定向触发检查退回；`ask` 测澄清问题。驱动拒绝复用已有工单库，要求根任务及全部协作者为 `done`、所有追加指令已送达，并核验 `required_events` 所列事件真实发生；功能仍须结合事件与产物核验。未配置的意外提问会停止测试，不会自动编造操作者答案。2026-11-17 的样本与限制见[真实验收](dev/collaboration-acceptance.md)。

用 `python scripts/collab-acceptance-summary.py <运行目录> --json` 只读提取证据。平台轮次、原生结果数和会话数分别记录；AGY 用量按会话累计结果去重，不把各轮累计值再次相加。缺失值为未知，模型自述与日志中的 `test result: ok` 不等于功能验收通过。运行日志与旧账本没有可核对关联时，不合并解释。完成后清除 `SA_LIVE_COLLAB`，避免全量测试再次发起调用。

问题通知可以独立运行 `scripts\cargo.cmd test -p sa-desktop windows_question_toast_reaches_notification_history -- --ignored --nocapture`。此检查从临时问题表经过轮询器发送真实通知，核对 Windows 通知历史中的标题、工单号及私密正文未外泄，并清理专属测试身份；不证明屏幕横幅、应用后台门控或点击导航。

## 权限

复验权限时测量文件系统的实际变化，不要只看模型自述。一次 Windows 实机验证观察到：DeepSeek 在批准受限写入轮次后只能写项目内目标；Claude 的受限工具清单未开放 Write；Antigravity 的受限写入尝试也未产生目标文件；在完整权限下三个执行器都曾写入项目外专门准备的兄弟目录。完整权限与自动模式只适合在隔离账户或测试机上验证。

## 2026-11-17 后续开发实测

实现范围与使用方式见[候选队列及推荐检查](dev/integration-queue.md)。本轮没有向远端推送，也没有合并当前 Super Agent 开发分支。

| 层次 | 已执行结果 | 本机证据 |
| --- | --- | --- |
| 自动检查 | `build --workspace`、`test --workspace`、`clippy --workspace --all-targets -- -D warnings` 均退出 0；测试汇总 654 通过、0 失败、7 忽略 | `tmpstate/followup/build-final3.log`、`test-final3.log`、`clippy-final3.log` |
| 队列与界面回归 | 八项队列测试通过，包括真实 Git 冲突、两次修复上限、旧令牌失效、多个前置工作区、重启恢复、停止与关闭时后代进程退出、数据库占用恢复；七项工作流界面测试通过 | 上述全量日志、`queue-tests.log`、`enabled-contention.log` |
| 旧库兼容 | SQLite 备份副本读取 7 张工单、306 张任务、445 条原生任务记录；假执行端续跑 3 张已完成工单通过。未调度原库 | `legacy.log`、`legacy-read.log` |
| 真实模型 | agy 1.5.9 的 `google / gemini-4-hydrogen-high` 两个授权场景均通过：原会话续派后依赖审查、隔离候选验证后确认合入 | [真实调用记录](dev/followup-live-acceptance.md) |
| 原生界面 | 在独立合成工单库观察推荐来源、点击采用并核对实际保存；打开候选确认后取消，发布事件为 0。深浅主题渲染可见且无截断 | `ui-receipt.json`、`release-light.png`、`release-dark-small.png` |
| 正式程序 | 更新 `target/release/super-agent.exe`，保留本轮前程序备份并重新渲染启动检查 | `release-final.log`、`release-final.png`、`release-hashes.json`、`super-agent.before-followup.exe` |

命令通过 `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/cargo.ps1` 调用。开发检查使用 `CARGO_TARGET_DIR=target/audit`；发布编译使用默认目录。全量测试没有启用计费开关；部分条件式真实测试会直接跳过，因此 654 通过不能解释为 654 次真实外部执行。旧库最大工单的冷读约 17.21 秒，兼容通过不代表该冷读性能已优化。

首次全量检查暴露了新队列在数据库占用时错误进入调度故障的问题；后续回归又发现快速读取改变原有锁顺序，已恢复调度器先于数据库的顺序，并增加启用队列的占用恢复测试。另修正本地控制测试对合法 `state_available=false` 响应的假设。原始失败日志保留为 `test-final.log`、`test-final2.log`，最后全量检查通过。

真实模型队列样本首次检查即通过，失败退回与取消分支由离线回归验证；确认合入由临时项目验收驱动调用 API，原生窗口另验证了确认展示和取消。没有用户人工验收，也没有据此宣称任意项目的质量、模型服从率或节费比例。系统通知的屏幕横幅与点击导航仍沿用此前未验收边界。

复用流程更新于 `C:/Users/hjy03/.codex/skills/local-superagent-control-room/references/integration-queue.md`，记录版本、验证令牌、进程取消、调度锁顺序及异步界面保存的检查方法；完成结构检查，并用于本轮数据库占用回归及真实队列样本。

## 工作目录自动 Git（2026-11-17）

新工单与旧工单实际执行前的 Git 准备、初始提交及权限边界见[工作目录 Git 记录](dev/project-git.md)。专项与工作区检查已执行；全量批次唯一界面夹具失败修复后，整个桌面组复验通过。记录明确区分全量命令退出结果与后续专项复验，没有把一次失败的全量命令写成直接通过。本轮使用合成旧工单恢复，未调度真实旧项目，也未追加计费模型调用。

## 架构演进第一轮开发包（2026-11-17）

落实长期设计 §14.1 的四项增量，操作及限制见[需求证据、交付快照与顾问用量](dev/delivery-evidence.md)。这不是 P0–P5 全部阶段完成，也不是六项长期产品目标已达成。源码基于 `0cc3b8b8d25ac9ed06bafa61a69e0458ebc13bed` 加既有未提交改动；本轮 24 个实现文件的冻结摘要保存在 `tmpstate/evolution-p0/source-manifest.json`，不能只用 HEAD 复现。

| 层次 | 已执行结果 | 本机证据（`tmpstate/evolution-p0/` 下） |
| --- | --- | --- |
| 构建与静态检查 | 工作区构建退出 0；冻结源码后的 `clippy --workspace --all-targets -- -D warnings` 退出 0 | `build-final.log`、`clippy-final.log` |
| 全量自动检查 | `test --workspace --no-fail-fast -- --test-threads=2` 退出 0，702 通过、0 失败、7 忽略 | `workspace-tests-frozen.log` |
| 需求与对象恢复 | 最终专项 16 项通过，覆盖修订、摘要标准、路径边界、损坏清单、忙对象、隔离备份和重新捕获；全量再次通过 | `delivery-final-tests.log`、`delivery-repair-observation.md` |
| 桌面交互 | 真实 App 的两项新增回归通过：记录、捕获、修订变过期；只读观察不提供修改控件且不产生事件 | `desktop-delivery2.log`、全量日志 |
| 历史评测 | 修改原项目文件后，同一断言在当前工作区失败，在历史快照通过；无当前文件回退 | `current-report.json`、`historical-report.json`、`evaluate-history.log` |
| 真实旧库只读 | SQLite 备份副本读取 7 张工单、306 张任务、445 条原生任务记录通过；未启动原库调度 | `legacy-read.log` |
| 真实 Fable 审查 | Claude CLI 2.1.325，两次授权只读审查；第一轮原生结果 `modelUsage` 为 `claude-fable-5-2`，第二轮经实际 `Engine::consult` 返回建议并验证独立用量、总量不重复 | `fable-review.json`、`advisor-live2.log`、`review-ledger/advice.json`、`review-ledger/usage-receipt.json` |
| 本地发布与窗口 | 发布编译退出 0，桌面快捷方式仍指向已更新的 `target/release/super-agent.exe`；合成只读账本原生窗口已渲染并查看，过期/未知状态可读且无截断 | `release.log`、`delivery-release.png`、`super-agent.before.exe`、`verification-receipt.json` |

发布程序 SHA-256 为 `32294C07DA909AB319FB5BA8DC38E20C9ECA94462D8EF83DBCB21372F3359F99`；替换前没有运行中的 Super Agent，旧程序备份摘要为 `9CA515E2E46286CC4D79EE305A4C70E216805DF66B9BD68D06D2971A89FE2924`。发布前后重新核对 24 个实现文件摘要一致。原生截图证明该发布程序能启动并显示合成证据，不是用户人工验收；图形进程退出码未采集，未将其记作退出 0。没有提交、推送或合并本轮工作区。

命令均经 `scripts/cargo.ps1` 调用。未配置真实调用开关时，条件式测试直接返回；702 通过不能解释为 702 次真实模型调用。第二轮顾问调用方为合成任务，只有顾问执行真实模型。用量为已报告范围内的 221,280 Token，仍为 partial；回执的 `reported_model` 是选择路由标签，不是独立的服务端身份认证。目录和第一轮原生结果支持 Fable 路由，但不据此断言供应商内部实现或实际扣费金额。

Fable 审查后修复了需求提交后再读失败导致的误报、损坏最新清单阻断编辑、受控坏对象无法恢复的问题。两次 Fable 输入早于最终修复，不能称为最终源码的再次模型验收；最终修复经独立静态复核及冻结源码全量检查。建议计量失败保留建议并标记 partial，最终建议回执无法落盘仍报错，避免伪装可靠交付。

首次全量运行有五个完成检查超时；同组以两个测试线程复验 13 项通过，最终全量沿用该并发度通过。中间运行另暴露 Windows 对象替换时目标句柄未释放导致 OS error 5，真实文件测试推翻了仅凭共享标志的静态判断；关闭守护句柄后再原子替换，保留损坏对象备份，最终专项及全量均通过。最初桌面测试还发现输入框无障碍角色被覆盖，改用标签关联后通过。原始失败日志保留，未将失败批次重记为通过。

旧库最大工单冷读约 21.91 秒，本轮未解决该性能问题。文件证据只是预设摘要与快照字节的对应，不等于功能、合入、发布或用户接受；纯文字需求仍为 unknown，新增摘要不拦截既有任务完成。未做用户人工验收、任意项目质量保证或节费对照；自动需求提取、行为验证器、强制验收门和自适应调度留待后续阶段。

本次复杂流程补充到 `C:/Users/hjy03/.codex/skills/local-superagent-control-room/references/rust-followups.md`，记录顾问计量、历史文件来源及 Windows 对象修复的边界；技能结构检查通过，并用本轮真实顾问回执与历史/当前报告复用核对。

## 可选快照交付门（2026-11-17）

本轮完成 P1A 的显式文件快照交付核验增量，见[操作与边界](dev/delivery-gate.md)。门默认关闭，由操作员启用；需求可带理由改为必需、可选或撤销。核验回执绑定合同、策略、快照和原始清单摘要，缺证会记录部分符合或受阻。job 状态、持续任务调度、模型重试、合入与人工接受保持独立。没有实现基于任务报告的自动交付门或通用行为验收。

| 层次 | 已执行结果 | 本机证据（`tmpstate/evolution-p1/` 下） |
| --- | --- | --- |
| 最终构建与静态检查 | `build --workspace` 和 `clippy --workspace --all-targets -- -D warnings` 均退出 0 | `build-final.log`、`clippy.log` |
| 最终全量回归 | 修复后的冻结代码运行 `test --workspace --no-fail-fast -- --test-threads=2` 退出 0：724 通过、0 失败、7 忽略 | `tests-final.log`、`tests-final-exit.txt` |
| 核心定向回归 | Store 5、原交付 13、新门 14、精确提交窗口单测 1 均通过；最后一项包含合同、策略 ABA、新快照和清单摘要四个确定性分支 | `core-final.log` |
| 桌面回归 | 5 项真实 App 与 2 项受控界面状态测试通过，覆盖启用、必需项未知阻塞、可选/撤销、重新捕获、两种状态并存、只读和迟到回执；编辑器固定打开时合同版本，刷新后旧草稿不能覆盖新修改 | `ui-tests.log` |
| 实际交付 API | 合成工单从 partial 经操作员范围调整和新捕获到 ready；修改项目文件后当前证据 stale、冻结快照仍 ready；关闭并只读重开保持相同结果 | `demo-final.log`、`native-fixture-final/receipt.json` |
| 原生桌面 | 已查看只读窗口和可操作窗口截图，控件及状态可读；可操作窗口只运行合成已完成工单，任务仍 done，未产生模型执行 | `gate-light.png`、`gate-editor.png`、`render-exit.txt`、`editor-render-exit.txt` |
| 独立模型审查 | Claude CLI 通过精确 `claude-opus-5-5` 路由完成一次只读审查，原生结果 success、is_error=false、15 turns，modelUsage 同型号 | `opus-review.json`、`opus-review-receipt.json`、`source-manifest.json` |
| 本地发布 | `build --release -p sa-desktop` 退出 0；桌面快捷方式仍指向新版程序；发布版以合成账本只读启动，深色 1400×1000 窗口已查看，退出 0 | `release.log`、`shortcut.json`、`gate-release.png`、`release-render-exit.txt` |

最终发布程序 SHA-256 为 `94ce7d5e4411dcc11c48e8ed3fd88ccf04484b0b6d9929040ce8c1870224f900`。更新前没有运行中的 Super Agent，旧版保存为 `super-agent.before.exe`，摘要为 `32294c07da909ab319fb5ba8dc38e20c9eca94462d8ef83dbcb21372f3359f99`。总回执在 `verification-receipt.json`，核对各命令退出码、最终源码摘要、桌面快捷方式、合成账本事件与发布摘要；当前源码与最终冻结清单一致。

本轮首次 Fable 请求收到网关 503“没有可用账号”，没有返回审查意见。用户说明体验结束后，改用目录中存在的 Opus 5.5。没有把 Fable 失败或仅查询目录记为模型审查成功，没有改写全局模型设置。Opus 审查是代码审查，不是模型执行真实项目的功能验收，也不据其用量估计实际账单。

全量检查没有设置 `SA_LIVE_*`，条件式真实调用测试会跳过其外部调用；724 项通过不等于 724 次模型或服务端执行。自动检查使用 `CARGO_TARGET_DIR=target/audit`，发布程序使用默认 `target/release`。

独立审查发现并修复了刷新导致旧编辑草稿借用新合同版本提交的问题；Opus 又发现类型损坏的历史核验会阻断编辑恢复，以及预读清单摘要与实际读取清单缺少直接绑定的问题。修复后增加 BLOB、非 UTF-8、错误数值/布尔类型、无效策略版本及原始旧清单摘要回归，并经另一次独立代码复核。`source-manifest.json` 对应 Opus 实际审查输入，`final-source-manifest.json` 对应修复后的冻结代码；不将二者冒称同一份审查输入。

新增损坏夹具首次向无效外键写入时，被引擎正常的外键约束拒绝。随后改在新的合成账本上用独立连接模拟外部损坏，生产约束未关闭。失败完整流在重跑前未保留，工具回执中的失败片段恢复到 `core-before.log` 并明确标注范围；最终定向日志完整保留。首次全量冻结版本 720 通过、0 失败、7 忽略，记录在 `tests.log`，此版本早于 Opus 修复。

策略版本号本身损坏时，不推测原高水位、不自动启用：需求编辑仍可用，门保持关闭，策略需维护。仅有其他策略字段损坏且版本可信时可追加新设置恢复。本轮未修改真实用户工单库，未提交或推送开发分支，未完成人工体验验收，也没有质量、成本或自主完成率的对照结论。未新增或修改技能。

## P1A 行为检查与操作员接受（2026-11-18）

在原工作树上接续完成需求、命令、验证器文件、输入清单、行为尝试和交付回执的绑定。协作页可运行、取消、重跑和核验检查，单独查看关联任务的合入轮次。操作员可明确接受一份核验回执对应的快照或部分交付。[操作与边界](dev/delivery-behavior.md)

| 验证 | 最终结果 |
| --- | --- |
| 构建、静态检查、全量测试 | 经 Windows Cargo 包装器执行工作区构建、全目标严格 Clippy 和全量测试，均退出 0。756 项通过、0 失败、7 项忽略，测试并发为 2。比较源码清单中的 193 个 Rust 源文件及 crate 清单，运行前后摘要一致。 |
| 真实命令与竞态 | Python 断言让漏乘数量的实现退出 1，快照受阻。修正后新快照的真实进程退出 0，重新核验后可交付。另使用真实 Git 快进合并、Windows 文件句柄、取消及提交窗口来验证输入换版、验证器换弱和迟到结果不会继承通过。 |
| 人工接受操作 | 合成操作员接受精确回执，重开后仍可追溯。同快照再核验、更换行为尝试、对象损坏、部分范围变化均有回归。接受部分仍显示未满足项，重复确认仅产生一次记录。 |
| 桌面与本地发布 | 在合成工单上检查可操作和只读原生窗口。已完成、冻结检查通过、当前输入过期、暂无合入及已接受对应快照可同时辨认。发布 SHA-256 为 `64ecca9125004d4ed0f502f3dc5c1c44e81b146af1704879f786e096f6a541ad`，桌面快捷方式指向这份程序，原发布程序已保留。 |
| 历史兼容 | 用 SQLite backup 复制上一轮合成账本，在新桌面打开后原有 partial/ready 回执及未满足项保持一致，新增行为尝试和接受记录均为零。另对既有真实账本的只读副本运行 real_db，7 张工单全部读取通过。 |
| 顾问 | 配对目录确认后，使用 `claude-opus-5-5` 完成一次设计审查及一次冻结核心代码复核。两次原生结果均成功且报告同型号。第二次覆盖 6 个文件，早于最终 Git 边界、预检和人工接受增量。后续由本机回归与原生运行验证。 |

本机回执位于 `tmpstate/delivery-behavior-20261118/verification.json`，对应日志为 `build-verified.log`、`clippy-verified.log` 和 `test-verified.log`。真实示例在 `verified-fixture/receipt.json`，发布窗口在 `release-writable.png` 和 `release-observer.png`。发布前备份为同目录 `super-agent.before.exe`。真实模型端到端工单开关未开启，运行中的模拟接受不代表用户已验收本次产品。长期架构其余阶段及质量、用量对照仍待验证。

## 完成时自动核对交付证据（2026-11-18）

写入型任务的完成检查通过后，平台会在任务工作目录的快照上自动运行操作员已采用的需求标准，并把结论交给主控。交付门默认只记录。门开启时，必需项失败会复用两次退回上限，退回同一任务修复。[操作与边界](dev/delivery-behavior.md#完成时自动核对)

| 验证 | 结果 |
| --- | --- |
| 构建与静态检查 | 经 Windows Cargo 包装器（`CARGO_TARGET_DIR=target/audit`）执行工作区构建、全目标严格 Clippy，均退出 0。最后一处修改（保持手动捕获事件格式不变）之后重跑了 Clippy 和 `sa-engine` 全部测试：371 项通过、0 失败、4 项忽略。 |
| 全量测试 | `test --workspace --no-fail-fast -- --test-threads=2` 退出 0：763 项通过、0 失败、7 项忽略。新增 7 项：`sa-engine --test delivery_auto` 的 4 个集成场景，以及 `delivery::auto` 的 3 个模块测试。另扩充了一个桌面标签测试。 |
| 集成场景 | 使用脚本化执行端、真实 Git 仓库和真实检查进程。覆盖：记录模式下捕获任务目录并通过，主控回报中带 `delivery`；交付门开启时，依赖被忽略文件的通过在快照上失败、退回同一任务、修复后核验为 ready；隔离 worktree 在合入前捕获，当前视图显示尚未合入；没有需求的工单保持原检查回执。 |
| 模块测试 | 输入不变时沿用快照，不重跑已有回执的检查；项目超过 128 个文件时退回到「上次快照 + 改动」；门关闭不退回，门开启时退回内容带需求标题和退出码。 |

日志位于 `tmpstate/delivery-auto-20261118/`：`build.log`、`clippy.log`、`clippy-final.log`、`test.log`、`test-engine-final.log`。未执行的部分：没有用真实模型跑端到端工单，没有重新编译或替换 `target/release` 发布程序，也没有人工体验验收。真实模型是否会利用回报中的 `delivery` 字段，仍待实际运行确认。
