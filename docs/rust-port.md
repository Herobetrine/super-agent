# Rust 重构路线

目标：用 Rust 取代 Electron 桌面端、Node.js 调度器和遗留 Python/Qt 实现，并提供纯 Rust（egui）的桌面界面。迁移期间现有 Electron 版本继续可用；同一个工单库 `hub.sqlite3` 任何时刻只能有一个调度器持有者。

## 工作区

| 路径 | 作用 | 取代 |
| --- | --- | --- |
| `crates/sa-store` | 兼容现有表结构的 SQLite 访问、只读快照、进展投影 | `runtime/engine-schema.cjs`、`studio.cjs` 读路径、`progress.cjs`、`native-activity.cjs`、`harness-activity.cjs` |
| `crates/sa-desktop` | egui 桌面工作台（可执行文件 `super-agent`） | `desktop/`（Electron 主进程与 HTML 界面） |
| `crates/sa-engine` | 工单、任务状态机、控制指令、持续运行、断线重试与熔断 | `engine*.cjs`（`broker.cjs` 待移植） |
| `crates/sa-adapters` | Codex app-server、Claude Code stream-json、DeepSeek Harness、Antigravity CLI | `runtime/adapters/*` |
| 计划：`crates/sa-ext` | MCP（官方 Rust SDK `rmcp`）与技能目录 | `extensions.cjs`、`skills.cjs` |
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

尚未完成：真实执行端适配器（第 3 阶段）；`desktop.lock` 独占锁与 broker；`SA_CANCEL` 与只读子任务自动改派（spec §9.2 F 可选项）；桌面端尚未接入引擎写路径；未对真实工单库副本做联测。

## 执行端适配器现状（第 3 阶段）

`sa-adapters` 移植了 `runtime/adapters/*`：共用进程管道（无控制台窗口、取消时结束整棵进程树、stderr 只留内存尾部用于分类且不入日志）、严格 UTF-8 NDJSON、CC Switch 只读读取、权限档到沙箱的映射，以及四个执行端的目录发现与单轮执行。失败一律返回分类后的 `AdapterError`，会话 ID 一出现就上报，供引擎续接。`sa_adapters::all(Sys::from_process())` 给出可传给 `Engine::open` 的四个适配器。

已验证：离线夹具测试（Codex JSON-RPC 审批、过期请求拒绝、信件插入与终稿逐字节一致；UTF-8 分帧；取消后子孙进程不残留；stderr 不外泄）；`SA_LIVE_CATALOG=1` 时对本机四个执行端做只读目录探测，提供方 ID、模型数和默认路由与 JS 版一致。

尚未验证：真实模型轮次（会消耗额度，按 `docs/verification.md` 另行联测）；远程 HTTPS 自定义目录（Rust 版目前只取本机 HTTP 目录，远程来源只列已保存模型）；`catalogRevision` 文件指纹未移植。

## 只读观察窗口

`super-agent` 以 SQLite 只读模式打开工单库，可与正在运行的 Electron 调度器同时使用：

```powershell
node scripts/cargo.cjs build --release -p sa-desktop
target\release\super-agent.exe                    # 默认工单库，或用 --data 指定目录
target\release\super-agent.exe --ticket 5 --inspector
target\release\super-agent.exe --demo --dark      # 合成数据
```

`--screenshot <png> [--size 宽x高]` 渲染后保存截图并退出，用于界面回归。

## 追加需求（运营方，2026-11-03）

- **执行端断线与卡住**：上游服务（如 OpenAI/Codex）中途卡顿或断开时，调度器要及时识别并处理：按阶段的空闲看门狗、瞬时/永久错误分类、带抖动的有限重试并续接原生会话、把失败原因与执行端健康状态告知主控以便改派，界面显示等待上游、疑似卡住、重试中等状态。
- **供应商管理与轮换**：可在界面中添加供应商；提供轮换开关，某个供应商不可用时自动轮转到其他已配置供应商重试。
- **项目共享知识库**：同一项目内的所有 agent 共享记忆、经验和项目进度，并在每轮上下文中可用。

这些需求纳入第 2–4 阶段的设计与验收。
