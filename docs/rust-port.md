# Rust 重构路线

目标：用 Rust 取代 Electron 桌面端、Node.js 调度器和遗留 Python/Qt 实现，并提供纯 Rust（egui）的桌面界面。迁移期间现有 Electron 版本继续可用；同一个工单库 `hub.sqlite3` 任何时刻只能有一个调度器持有者。

## 工作区

| 路径 | 作用 | 取代 |
| --- | --- | --- |
| `crates/sa-store` | 兼容现有表结构的 SQLite 访问、只读快照、进展投影 | `runtime/engine-schema.cjs`、`studio.cjs` 读路径、`progress.cjs`、`native-activity.cjs`、`harness-activity.cjs` |
| `crates/sa-desktop` | egui 桌面工作台（可执行文件 `super-agent`） | `desktop/`（Electron 主进程与 HTML 界面） |
| `crates/sa-engine` | 工单、任务状态机、控制指令、持续运行、断线重试与熔断、工单库独占、项目知识库 | `engine*.cjs`、`broker.cjs` |
| `crates/sa-adapters` | Codex app-server、Claude Code stream-json、DeepSeek Harness、Antigravity CLI，自定义供应商（见 `crates/sa-adapters/PROVIDERS.md`） | `runtime/adapters/*` |
| `crates/sa-ext` | MCP（官方 Rust SDK `rmcp`）与技能目录 | `extensions.cjs`、`skills.cjs` |

## 阶段

1. **数据层与只读观察界面**：`sa-store` 读路径与投影移植并配套测试；`super-agent` 以只读方式打开工单库，可与正在运行的 Electron 调度器并存。
2. **调度引擎**：工单与任务状态机、`SA_*` 控制协议、信箱、审批、持续运行阶段；以假执行端移植 `tests-node/engine*.test.cjs`。
3. **执行端适配器**：四个 CLI 的进程管理、事件流、权限映射、模型目录；先离线夹具测试，再按 `docs/verification.md` 做真实联测。
4. **交互功能**：新建对话、追加指令、留言、停止/重试、审批、模型与执行端、MCP 与技能、附件快照、Git 变更、导出。
5. **切换**：桌面快捷方式指向 Rust 可执行文件；验证同库独占、旧工单续跑后，移除 Electron/Node/Python 实现并更新 README。

每个阶段完成时都要有可复现的验证记录；未经真实运行验证的部分不视为完成。

## 构建

Windows GNU 工具链需要一个真正的 C 编译器来编译内置 SQLite。将 [llvm-mingw](https://github.com/mstorsjo/llvm-mingw/releases) 的 `msvcrt-x86_64` 包解压到 `%LOCALAPPDATA%\SuperAgent\toolchain\`（或设置 `SUPERAGENT_LLVM_MINGW`），然后通过包装脚本 `scripts\cargo.cmd`（调用 `scripts/cargo.ps1`）运行 cargo：

```powershell
scripts\cargo.cmd test --workspace
scripts\cargo.cmd run -p sa-desktop --release -- --demo
```

包装脚本借用 llvm-mingw 的 `clang`、`llvm-ar`、`dlltool` 和 `ld.lld`，保留 Rust 自带的 CRT 与运行库（`-C linker-flavor=ld.lld -C link-self-contained=yes`）。不要改回 Rust 自带的 GNU `ld`：它会错误链接 llvm `dlltool` 生成的 raw-dylib 导入库，程序在进入 `main` 前崩溃。MSVC 工具链无需该步骤。

## 调度引擎现状（第 2 阶段）

`sa-engine::Engine` 已移植 `engine.cjs`、`engine-dispatch.cjs` 与 `engine-store.cjs` 的写路径：准入循环与写者独占、信封、流式与终稿 `SA_*` 指令、信箱、审批、取消/追加/重试、持续运行状态机、超大输入压缩、附件快照。`tests/engine.rs`、`tests/autonomy.rs` 以假执行端移植了 `docs/dev/engine-spec.md` §8 的 24 个用例（`scripts\cargo.cmd test -p sa-engine`）。

失败处理经 `TurnRunner`：默认 `ResilientRunner` 对所有任务做分类重试（带抖动、续接原生会话）、按路由熔断（`turn.route_wait`、`turn.deferred`、`harness.health`）、可选供应商轮换（`route.rotated`），子任务首次失败会给上级发 `platform` 留言，放弃时记录 `turn.gave_up` 并在上级 `reports[].failure` 中标明；`PassThroughRunner` 保留 JS 的自动工单正则重试。见 `tests/engine_resilience.rs`。

工单库独占：`sa_engine::owner::claim` 对 `desktop.lock` 加排他文件锁（与 Electron broker 同一个锁文件），并把进程放入 Windows 作业对象（`KILL_ON_JOB_CLOSE`），窗口退出时执行端子进程随之结束。

`SA_CANCEL`（上级取消直属子任务）、只读子任务自动改派（每张工单可开关）和按内存活动推算的实时徽标（`Engine::activity`）已实现，见 spec §9.2 F/G。令牌用量通过 `sa-store::usage` 从原生事件增量投影，按执行端分别处理累计值和单次调用值。

自动改派开关在实际改派事务中读取，运行中开启或关闭都会影响该任务下一次失败后的处理；已经完成的改派不撤回。回归测试分别覆盖运行中开启和关闭，不用入场时的旧开关值替代当前设置。

## 执行端适配器现状（第 3 阶段）

`sa-adapters` 移植了 `runtime/adapters/*`：共用进程管道（无控制台窗口、取消时结束整棵进程树、stderr 只留内存尾部用于分类且不入日志）、严格 UTF-8 NDJSON、CC Switch 只读读取、权限档到沙箱的映射，以及四个执行端的目录发现与单轮执行。失败一律返回分类后的 `AdapterError`，会话 ID 一出现就上报，供引擎续接。`sa_adapters::all(Sys::from_process())` 给出可传给 `Engine::open` 的四个适配器。

已验证：离线夹具测试（Codex JSON-RPC 审批、过期请求拒绝、信件插入与终稿逐字节一致；UTF-8 分帧；取消后子孙进程不残留；stderr 不外泄）；`SA_LIVE_CATALOG=1` 时对本机四个执行端做只读目录探测，提供方 ID、模型数和默认路由与 JS 版一致。

真实联测（2026-11-03，计费）：
- `SA_LIVE_TURNS=1 scripts\cargo.cmd test -p sa-adapters --test turns_live -- --nocapture`：四个执行端各做一次只读文件挑战，再续接同一原生会话读回改写后的文件。四个都通过：答案正确、与原生终稿逐字节一致、会话 ID 在轮次结束前上报、第二轮复用同一会话。`SA_LIVE_AGENTS=codex,claude` 可只测部分执行端。
- `SA_LIVE_ENGINE=1 scripts\cargo.cmd test -p sa-adapters --test engine_live -- --nocapture`：临时工单库里由 DeepSeek 主控向 Codex（写者）、Claude、Antigravity 派工。三个子任务的原生终稿、入库结果和主控收到的回报一致，Codex 写出的 handoff.txt 字节正确，197 条事件哈希全部吻合，用时约 3.5 分钟。
- 回执在 `~/.local/share/superagent/checks/rust-*`，只存哈希。
- 联测中 Codex 曾返回 `serverOverloaded`（所选模型容量不足），原先归为未分类；现归为 `overloaded`，可按退避重试。
- 本机 Claude 默认来源不可用时，测试改选第一个可用来源；正式界面仍需要用户明确选择。

尚未验证：`catalogRevision` 的 Rust 指纹只比较文件元数据，与 Node 版数值不同，只能在同一进程内比较。远程 HTTPS 目录已改用 `ureq`（rustls）获取。

## 桌面工作台

`super-agent` 启动时尝试取得工单库的 `desktop.lock`：取得则在后台线程运行 Rust 调度器（`host.rs`，单线程顺序执行界面动作）；锁已被占用（Electron 版或另一个窗口在运行）时自动降为只读观察，顶栏显示原因。

```powershell
scripts\cargo.cmd build --release -p sa-desktop
target\release\super-agent.exe                    # 默认工单库，或用 --data 指定目录
target\release\super-agent.exe --observe          # 强制只读，不取锁
target\release\super-agent.exe --ticket 5 --inspector knowledge
target\release\super-agent.exe --demo --dark      # 合成数据，不调度
powershell -File Install-Desktop.ps1              # 桌面快捷方式指向 Rust 版
```

`--screenshot <png> [--size 宽x高]` 渲染后保存截图并退出，用于界面回归；截图模式默认不调度，加 `--schedule` 才会取锁；`--dialog models|providers|rotation|extensions` 启动时打开对应对话框。

可调度窗口的功能（括号内为对应的 Electron 接口）：

| 功能 | 位置 |
| --- | --- |
| 新建对话、追加指令、定向留言、停止、重试、审批、计划、附件 | 输入区、项目面板（`create`、`follow`、`note`、`cancel`、`retry`、`decide`、`planAdd/planToggle`） |
| 模型与执行端：新会话默认、本次新会话、已停止工单的路由 | 左侧栏滑块图标、输入区（`defaults/saveDefaults/saveRoutes/catalog`） |
| MCP 与技能：默认授权、本次新会话授权、工单已绑定授权（只读）、添加/编辑/探测/移除服务、导入/预览/取消登记技能 | 左侧栏拼图图标、输入区（`extensions*`、`mcp*`、`*Skill`） |
| 自定义供应商与轮换开关 | 左侧栏插头图标（运营方新增需求） |
| 项目知识库：查看、添加、归档 | 项目面板“知识”（运营方新增需求，观察窗口只读可见） |
| Git 变更 | 项目面板“变更”（`diff`，观察窗口可用） |
| 导出完整记录（JSON Lines，逐条校验摘要，只写新文件） | 顶栏导出图标（`exportTo`，观察窗口可用） |
| 卡顿、重试 n/max、供应商轮换、熔断、放弃 | 对话流状态行（`turn.stalled` 等事件） |
| 实时徽标：运行中、等待上游、工具执行中、疑似卡住、等待审批、重试中 n/max、熔断 | 实时状态条、协作卡片 |
| 只读子任务自动改派开关 | 项目面板“协作” |
| 欢迎页（起步提示、最近项目）、命令面板 Ctrl+K、快捷键 Ctrl+N/B/J、Ctrl+Shift+F/L/M | 全局 |
| `@` 提及项目文件、粘贴图片（存入 `.superagent/pasted/`）、拖入文件或项目文件夹 | 输入区 |
| 会话重命名、置顶、归档 | 侧栏右键菜单 |
| 复制回复、导出对话为 Markdown | 回复标题栏、顶栏导出菜单 |
| 工单完成或新审批时任务栏闪烁提醒 | 窗口在后台时 |
| 执行端已报告令牌用量，按任务展开 | 顶栏“用量” |
| Windows 原生通知与点击返回工单 | 后台结束/审批提醒，侧栏铃铛开关 |
| 逐文件差异、按内容版本审阅、最近检查点对比 | 项目面板“变更” |
| Codex/Claude 原生图片输入及后续消息补图 | 输入区附件、拖放、粘贴 |

自定义供应商只保存密钥所在环境变量的名字；各执行端如何接入见 `crates/sa-adapters/PROVIDERS.md`。轮换开启后，引擎的 `ResilientRunner` 在认证、额度、限流、过载或供应商不可用时按运营方顺序改用下一条路由（`route.rotated`），换路由后不续接原会话。

项目知识库：表 `project_knowledge` 按工单的规范化 `cwd` 区分项目；执行者在终稿中用 `SA_MEMO {"kind":"memory|lesson|progress","text":"…"}` 写入，主控完成工单时自动记一条进展，每轮信封带 `project_knowledge`（最新条目，约 6000 字符以内）。细则见 `docs/dev/engine-spec.md` §4.4。

## 验证记录（2026-11-05）

自动测试：`scripts\cargo.cmd test --workspace` 全部通过，`clippy --workspace --all-targets` 无警告。桌面端 egui_kittest 界面测试通过无障碍树驱动真实 `App`：输入并回车创建工单、批准/拒绝、计划、追加；模型对话框保存默认路由与工单路由（`routes.changed`）；知识库添加与归档；供应商新增、轮换保存、删除后轮换同步；MCP 服务添加、真实探测 fixture 服务、技能与服务授权保存；被占用的工单库只能只读观察。

旧工单续跑：`SA_LEGACY_COPY=<副本目录> scripts\cargo.cmd test -p sa-desktop legacy -- --nocapture` 在 Node 版写出的工单库副本（5 张工单）上启动 Rust 调度器，恢复后无遗留运行中任务，3 张已完成的旧工单追加指令后续跑完成，只读快照全部可读。（用假执行端，未发起模型调用。）

真实联测（计费，均在临时工单库与临时项目中）：
- `SA_LIVE_UI=1`：从窗口输入框发起 DeepSeek 只读轮次，答案与挑战文件一致。
- `SA_LIVE_EXT=1`：DeepSeek 主控经运营方批准调用授权的 MCP 工具，回执 `done`，终稿给出 MCP 返回的口令，并用 `SA_MEMO` 写入知识库；主控完成后自动记进展。此次运行中 DeepSeek Harness 前三次进程异常退出，引擎按分类重试、熔断、等待上游后半开恢复，第四轮完成——断线处理在真实环境下走通。
- `SA_LIVE_PROVIDER=deepseek,https://api.siliconflow.cn/v1,SILICONFLOW_API_KEY,deepseek-ai/DeepSeek-V3`：新增自定义供应商，HTTPS 探测到 99 个模型，路由绑定 `sa-<id>`，DeepSeek Harness 经该端点读出挑战文件。
- `SA_LIVE_ROTATION=…`（同上参数）：首选供应商使用无效密钥，返回认证失败后按轮换改用第二个供应商（`route.rotated`，原因 `auth`），轮次完成。

## 切换状态（2026-11-05 完成）

Rust 窗口已替代 Electron 工作台；`Install-Desktop.ps1` 把桌面快捷方式指向 `target\release\super-agent.exe`。旧版 Electron/Node（`desktop/`、`runtime/`、`native/`、`tests-node/`）与 Python/Qt（根目录 `*.py`、`adapters/`、`tests/`）已从仓库删除，可从提交 `ea08b8a` 取回。代码注释里形如 `(E:252)`、`(ST:239-290)` 的出处指向这些已删除的 JS 源文件；`docs/dev/engine-spec.md` 同样以它们为准。

## 追加需求（运营方，2026-11-03）

- **执行端断线与卡住**：上游服务（如 OpenAI/Codex）中途卡顿或断开时，调度器要及时识别并处理：按阶段的空闲看门狗、瞬时/永久错误分类、带抖动的有限重试并续接原生会话、把失败原因与执行端健康状态告知主控以便改派，界面显示等待上游、疑似卡住、重试中等状态。——已实现并在真实联测中触发。
- **供应商管理与轮换**：可在界面中添加供应商；提供轮换开关，某个供应商不可用时自动轮转到其他已配置供应商重试。——已实现并真实联测。
- **项目共享知识库**：同一项目内的所有 agent 共享记忆、经验和项目进度，并在每轮上下文中可用。——已实现并真实联测。

## 后续功能与验收边界

Claude 于 2026-11-05 交接的四项后续功能已接入，具体范围如下。

- **令牌用量**从已保存的 native 事件只读投影，每次扫描最多 256 行，单条解析上限 256 KiB。Codex 按线程累计增量去重，缺基线与乱序值标记部分统计。Claude 按本轮主循环计数，内部子任务范围单列。DSH 按会话/轮次/步骤去重并归一化缓存桶。Antigravity 用会话终态累计值替代已计步骤。字段缺失保持未知，统计范围覆盖工单全任务，明细随当前分页显示。协议核对使用 Codex 0.166.0、Claude 2.1.317、DSH 0.2.1-rc.5、agy 1.5.0。
- **Windows 通知**使用现有 windows-rs 0.62.2，后台线程持有 WinRT 对象，首次发送登记应用通知身份与专属开始菜单链接。审批轮询覆盖当前库的全部待审批工单，结束提醒沿会话历史检测状态变化。通知只带工单号和状态。关闭通知会取消排队项，退出工作台会收回本进程的通知。点击回执通过通道回到 UI 并选中工单。Windows 显示策略仍生效。
- **逐文件审阅与检查点**使用 Git porcelain v2、NUL 分隔和 literal pathspecs。暂存、工作区、未跟踪及检查点比较分别显示，中文路径可读，二进制和截断有专门提示。审阅标记绑定内容摘要，仅保留在当前窗口。每项目最近检查点保存于应用数据的 `review/`，覆盖 tracked 与未忽略 untracked，包括清洁文件，上限 10000 路径、单文件 4 MiB、总量 64 MiB。两次完整扫描一致后原子发布清单，重开可继续比较。检查点用于捕获与对比，恢复文件功能留待后续实现。
- **原生图片**经附件快照校验 SHA-256、长度、对象目录、PNG/JPEG 真实格式与解码尺寸，再传入 Codex image/data URL 或 Claude base64 image block。每轮最多 4 张、每张 5 MiB、合计 16 MiB、单边 8192 像素、总计 1600 万像素。初始消息、SA_SEND 与主控后续消息可携图，新图替换当前附加图片集，同名文本附件更新。当前原生会话保留此前对话上下文。DeepSeek/Antigravity 的未接入能力会明确反馈，自动改派只选择已支持图片的执行端。

协议来源采用官方文档及已安装版本的 schema/源码。[Codex App Server](https://learn.chatgpt.com/docs/app-server) · [Claude 用量](https://code.claude.com/docs/en/agent-sdk/cost-tracking) · [Claude 流式图片](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode) · [Antigravity headless](https://antigravity.google/docs/cli/headless/) · [DSH](https://github.com/deepseek-ai/deepseek-harness) · [Git status](https://git-scm.com/docs/git-status) · [Windows 通知](https://learn.microsoft.com/en-us/windows/win32/shell/quickstart-sending-desktop-toast)

`SA_CANCEL`、只读自动改派和实时活动徽标目前有假执行端回归，新增加的运行中开关测试也属于离线验证，尚未进行这批功能的真实模型联测。界面 IME 自动测试验证 egui 事件与焦点处理，不能代替在 Windows 输入法中的人工验收。

接续验证（2026-11-05）：`build --workspace`、`test --workspace` 和 `clippy --workspace --all-targets` 全部成功，Clippy 无警告。测试器报告 201 项通过、1 项显式忽略；其中 9 项依赖环境开关的真实目录/模型、旧库或剪贴板检查在未设置开关时提前返回，本次未执行这些检查。新增 5 项回归实际执行通过：运行中开启/关闭改派、文件菜单失焦、正常键盘选择、多帧 IME 确认/取消与发送。重建后的调试程序在演示数据和独立空库上渲染截图并正常退出；已查看两张截图。未替换 release 安装或进行 Windows 输入法人工验收。本机证据保存在用户数据目录 `checks/codex-rust-handoff-20261105/`，不提交私有日志。

后续功能验证（同日）：workspace build/test 和 Clippy 通过，报告 237 passed、2 ignored，Clippy 无警告。9 项环境门禁检查仍提前返回，真实模型调用另行验证。Windows 通知历史测试单独以 `--ignored` 执行通过，临时 AUMID/快捷方式随测试清理。通过真实窗口点击验证用量弹窗、检查点保存、修改后对比及已审阅标记，观察到正确的合成计数与差异。release 重建并以演示模式渲染截图后退出，桌面快捷方式已更新。通知点击导航、真实模型视觉理解及人工输入法验收仍待单独确认。证据位于用户数据目录 `checks/rust-followups-20261105/`。

## 模型目录与协作者选型修正（2026-11-05）

模型目录按来源在线或通过本机 CLI 获取，支持同源分页、搜索和数量显示，超限与回退状态明确反馈。原来 Codex 自定义配置缺少 `base_url` 会退回仅显示配置中的一个型号，Claude 仅探测回环端点，DSH/Gemini API 来源也依赖静态配置。这些路径现已接入目录发现。Codex CLI 路径遵循其原生刷新与缓存策略。自定义来源每次刷新可更新能力，目录请求以编号关联，较晚返回的旧结果无法覆盖新目录。

`Route.reasoning_effort` 可持久化并传到对应执行协议，能力取自目录/CLI/运行时声明。显式空能力保留为空，DSH 默认关闭时可以通过显式强度重新启用 thinking，并保留公开级别到 wire 值的映射。

模型窗口提供 **交给主控选择协作者模型和思考强度**。主控自身保持操作员指定模型，协作者手动配置作为回退。关闭后从新派工开始使用手动配置，已接受任务保留原路由和会话。新会话默认策略存入 `studio_model_policy`，草稿可独立覆盖，既有工单通过 `tickets.allow_lead_routing` 更新；派工预检和提交事务读取最新授权。供应商管理可从侧栏文字按钮或模型页直接进入，返回后保留未保存的模型选择。

离线验证报告 260 passed、2 ignored，包含策略持久化、主控固定、撤权并发、强度传递、完整分页、搜索、供应商跳转与目录乱序回执。环境门禁项目仍以文档开关为准。只读 `SA_LIVE_CATALOG=1` 单独通过，本机当时的默认来源为 Codex 9 个型号、Claude 3 个（含 Sonnet）、DeepSeek 6 个、Antigravity 6 个加原生默认项。型号数量是当时观测值。三个旧 DSH 来源请求失败，界面明确标记已保存目录。此轮以夹具和只读目录验证参数与授权，模型推理效果留给实际任务验证；桌面控制在用户按 Escape 后停止。证据位于用户数据目录 `checks/model-controls-20261105/`。

修复版 release 已重建并通过 `--help` 启动检查，桌面快捷方式指向该可执行文件。
