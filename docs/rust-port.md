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
| `native/` | 现有 Node 辅助进程，切换完成前保留 | 最终并入上述 crate |

## 阶段

1. **数据层与只读观察界面**：`sa-store` 读路径与投影移植并配套测试；`super-agent` 以只读方式打开工单库，可与正在运行的 Electron 调度器并存。
2. **调度引擎**：工单与任务状态机、`SA_*` 控制协议、信箱、审批、持续运行阶段；以假执行端移植 `tests-node/engine*.test.cjs`。
3. **执行端适配器**：四个 CLI 的进程管理、事件流、权限映射、模型目录；先离线夹具测试，再按 `docs/verification.md` 做真实联测。
4. **交互功能**：新建对话、追加指令、留言、停止/重试、审批、模型与执行端、MCP 与技能、附件快照、Git 变更、导出。
5. **切换**：桌面快捷方式指向 Rust 可执行文件；验证同库独占、旧工单续跑后，移除 Electron/Node/Python 实现并更新 README。

每个阶段完成时都要有可复现的验证记录；未经真实运行验证的部分不视为完成。

## 构建

Windows GNU 工具链需要一个真正的 C 编译器来编译内置 SQLite。将 [llvm-mingw](https://github.com/mstorsjo/llvm-mingw/releases) 的 `msvcrt-x86_64` 包解压到 `%LOCALAPPDATA%\SuperAgent\toolchain\`（或设置 `SUPERAGENT_LLVM_MINGW`），然后通过包装脚本调用 cargo：

```powershell
node scripts/cargo.cjs test --workspace
node scripts/cargo.cjs run -p sa-desktop --release -- --demo
```

包装脚本借用 llvm-mingw 的 `clang`、`llvm-ar`、`dlltool` 和 `ld.lld`，保留 Rust 自带的 CRT 与运行库（`-C linker-flavor=ld.lld -C link-self-contained=yes`）。不要改回 Rust 自带的 GNU `ld`：它会错误链接 llvm `dlltool` 生成的 raw-dylib 导入库，程序在进入 `main` 前崩溃。MSVC 工具链无需该步骤。

## 调度引擎现状（第 2 阶段）

`sa-engine::Engine` 已移植 `engine.cjs`、`engine-dispatch.cjs` 与 `engine-store.cjs` 的写路径：准入循环与写者独占、信封、流式与终稿 `SA_*` 指令、信箱、审批、取消/追加/重试、持续运行状态机、超大输入压缩、附件快照。`tests/engine.rs`、`tests/autonomy.rs` 以假执行端移植了 `docs/dev/engine-spec.md` §8 的 24 个用例（`node scripts/cargo.cjs test -p sa-engine`）。

失败处理经 `TurnRunner`：默认 `ResilientRunner` 对所有任务做分类重试（带抖动、续接原生会话）、按路由熔断（`turn.route_wait`、`turn.deferred`、`harness.health`）、可选供应商轮换（`route.rotated`），子任务首次失败会给上级发 `platform` 留言，放弃时记录 `turn.gave_up` 并在上级 `reports[].failure` 中标明；`PassThroughRunner` 保留 JS 的自动工单正则重试。见 `tests/engine_resilience.rs`。

工单库独占：`sa_engine::owner::claim` 对 `desktop.lock` 加排他文件锁（与 Electron broker 同一个锁文件），并把进程放入 Windows 作业对象（`KILL_ON_JOB_CLOSE`），窗口退出时执行端子进程随之结束。

尚未实现：`SA_CANCEL` 与只读子任务自动改派（spec §9.2 F 可选项）；运行中“等待上游/工具执行中”这类按内存活动推算的实时徽标（目前以日志事件显示卡顿与重试）。

## 执行端适配器现状（第 3 阶段）

`sa-adapters` 移植了 `runtime/adapters/*`：共用进程管道（无控制台窗口、取消时结束整棵进程树、stderr 只留内存尾部用于分类且不入日志）、严格 UTF-8 NDJSON、CC Switch 只读读取、权限档到沙箱的映射，以及四个执行端的目录发现与单轮执行。失败一律返回分类后的 `AdapterError`，会话 ID 一出现就上报，供引擎续接。`sa_adapters::all(Sys::from_process())` 给出可传给 `Engine::open` 的四个适配器。

已验证：离线夹具测试（Codex JSON-RPC 审批、过期请求拒绝、信件插入与终稿逐字节一致；UTF-8 分帧；取消后子孙进程不残留；stderr 不外泄）；`SA_LIVE_CATALOG=1` 时对本机四个执行端做只读目录探测，提供方 ID、模型数和默认路由与 JS 版一致。

真实联测（2026-11-03，计费）：
- `SA_LIVE_TURNS=1 node scripts/cargo.cjs test -p sa-adapters --test turns_live -- --nocapture`：四个执行端各做一次只读文件挑战，再续接同一原生会话读回改写后的文件。四个都通过：答案正确、与原生终稿逐字节一致、会话 ID 在轮次结束前上报、第二轮复用同一会话。`SA_LIVE_AGENTS=codex,claude` 可只测部分执行端。
- `SA_LIVE_ENGINE=1 node scripts/cargo.cjs test -p sa-adapters --test engine_live -- --nocapture`：临时工单库里由 DeepSeek 主控向 Codex（写者）、Claude、Antigravity 派工。三个子任务的原生终稿、入库结果和主控收到的回报一致，Codex 写出的 handoff.txt 字节正确，197 条事件哈希全部吻合，用时约 3.5 分钟。
- 回执在 `~/.local/share/superagent/checks/rust-*`，只存哈希。
- 联测中 Codex 曾返回 `serverOverloaded`（所选模型容量不足），原先归为未分类；现归为 `overloaded`，可按退避重试。
- 本机 Claude 默认来源不可用时，测试改选第一个可用来源；正式界面仍需要用户明确选择。

尚未验证：`catalogRevision` 的 Rust 指纹只比较文件元数据，与 Node 版数值不同，只能在同一进程内比较。远程 HTTPS 目录已改用 `ureq`（rustls）获取。

## 桌面工作台

`super-agent` 启动时尝试取得工单库的 `desktop.lock`：取得则在后台线程运行 Rust 调度器（`host.rs`，单线程顺序执行界面动作）；锁已被占用（Electron 版或另一个窗口在运行）时自动降为只读观察，顶栏显示原因。

```powershell
node scripts/cargo.cjs build --release -p sa-desktop
target\release\super-agent.exe                    # 默认工单库，或用 --data 指定目录
target\release\super-agent.exe --observe          # 强制只读，不取锁
target\release\super-agent.exe --ticket 5 --inspector knowledge
target\release\super-agent.exe --demo --dark      # 合成数据，不调度
powershell -File Install-Desktop.ps1              # 桌面快捷方式指向 Rust 版（-Legacy 改回 Electron）
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

自定义供应商只保存密钥所在环境变量的名字；各执行端如何接入见 `crates/sa-adapters/PROVIDERS.md`。轮换开启后，引擎的 `ResilientRunner` 在认证、额度、限流、过载或供应商不可用时按运营方顺序改用下一条路由（`route.rotated`），换路由后不续接原会话。

项目知识库：表 `project_knowledge` 按工单的规范化 `cwd` 区分项目；执行者在终稿中用 `SA_MEMO {"kind":"memory|lesson|progress","text":"…"}` 写入，主控完成工单时自动记一条进展，每轮信封带 `project_knowledge`（最新条目，约 6000 字符以内）。细则见 `docs/dev/engine-spec.md` §4.4。

## 验证记录（2026-11-05）

自动测试：`node scripts/cargo.cjs test --workspace` 全部通过，`clippy --workspace --all-targets` 无警告。桌面端 egui_kittest 界面测试通过无障碍树驱动真实 `App`：输入并回车创建工单、批准/拒绝、计划、追加；模型对话框保存默认路由与工单路由（`routes.changed`）；知识库添加与归档；供应商新增、轮换保存、删除后轮换同步；MCP 服务添加、真实探测 fixture 服务、技能与服务授权保存；被占用的工单库只能只读观察。

旧工单续跑：`SA_LEGACY_COPY=<副本目录> node scripts/cargo.cjs test -p sa-desktop legacy -- --nocapture` 在 Node 版写出的工单库副本（5 张工单）上启动 Rust 调度器，恢复后无遗留运行中任务，3 张已完成的旧工单追加指令后续跑完成，只读快照全部可读。（用假执行端，未发起模型调用。）

真实联测（计费，均在临时工单库与临时项目中）：
- `SA_LIVE_UI=1`：从窗口输入框发起 DeepSeek 只读轮次，答案与挑战文件一致。
- `SA_LIVE_EXT=1`：DeepSeek 主控经运营方批准调用授权的 MCP 工具，回执 `done`，终稿给出 MCP 返回的口令，并用 `SA_MEMO` 写入知识库；主控完成后自动记进展。此次运行中 DeepSeek Harness 前三次进程异常退出，引擎按分类重试、熔断、等待上游后半开恢复，第四轮完成——断线处理在真实环境下走通。
- `SA_LIVE_PROVIDER=deepseek,https://api.siliconflow.cn/v1,SILICONFLOW_API_KEY,deepseek-ai/DeepSeek-V3`：新增自定义供应商，HTTPS 探测到 99 个模型，路由绑定 `sa-<id>`，DeepSeek Harness 经该端点读出挑战文件。
- `SA_LIVE_ROTATION=…`（同上参数）：首选供应商使用无效密钥，返回认证失败后按轮换改用第二个供应商（`route.rotated`，原因 `auth`），轮次完成。

## 切换状态

已完成：Rust 窗口可完整替代 Electron 工作台的日常操作；`Install-Desktop.ps1` 默认把快捷方式指向 `target\release\super-agent.exe`；同库独占与旧工单续跑已验证。

待运营方决定：删除 Electron（`desktop/`、`runtime/`、`package.json`）、Node 辅助进程（`native/`）与 Python/Qt 旧实现（根目录 `*.py`、`tests/`）。删除前请先用 Rust 版实际工作一段时间；`native/` 下另有未提交的改动，作者未知，一直保持原样。

## 追加需求（运营方，2026-11-03）

- **执行端断线与卡住**：上游服务（如 OpenAI/Codex）中途卡顿或断开时，调度器要及时识别并处理：按阶段的空闲看门狗、瞬时/永久错误分类、带抖动的有限重试并续接原生会话、把失败原因与执行端健康状态告知主控以便改派，界面显示等待上游、疑似卡住、重试中等状态。——已实现并在真实联测中触发。
- **供应商管理与轮换**：可在界面中添加供应商；提供轮换开关，某个供应商不可用时自动轮转到其他已配置供应商重试。——已实现并真实联测。
- **项目共享知识库**：同一项目内的所有 agent 共享记忆、经验和项目进度，并在每轮上下文中可用。——已实现并真实联测。
