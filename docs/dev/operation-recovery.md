# 平台操作回执与重启对账

平台自己发起的 Git 写入、合入发布和检查进程，在产生副作用之前先写入一条操作回执（`operations` 表），结束后在原有结果事务中写入已知结论。程序在中途退出时，下次启动先按回执核对实际状态，再恢复队列和任务；核对只读取仓库和账本，不重做副作用。没有收到回执不能证明操作没有发生，核对不出结果的操作保留为“结果未知”，交给操作者人工核对。

实现在 `crates/sa-engine/src/operations.rs`。回执是增量表，旧账本打开时自动创建，旧程序不读取它。

## 状态

| 状态 | 含义 |
| --- | --- |
| `started` | 意图已保存，副作用即将或已经开始。`planned` 保留在表约束中，当前不单独使用。 |
| `confirmed` | 副作用已按预期发生，或检查进程已运行到确定结局（退出码、超时后确认终止）。 |
| `failed_before_effect` | 核实副作用没有发生，例如程序未能启动、仓库仍停在操作前提交。 |
| `unknown_effect` | 无法证明是否发生。不阻止停止工单、不让已停止的工单恢复、不触发模型执行。 |
| `cancel_requested` | 已请求取消平台拥有的进程树，尚未确认终止。请求取消本身不等于没有副作用。 |
| `cancelled` | 进程树已确认终止。 |
| `resolved` | 操作者核对了 `unknown_effect` 并留下说明，原结论保存在 `detail.previous`。 |

每次状态变化写一条 `operation.<状态>` 事件，载荷为 `{operation, kind, state, detail}`。新工单的项目 Git 初始化发生在工单创建之前，这两条回执先不带工单，创建工单的事务再把它们归属到主任务，不补写事件；回执行本身即为记录。重启对账写入的结论在 `detail.reconciled` 标为 `true`。

意图只保存对账所需的路径、分支和提交号，以及命令参数的摘要。不保存提示词、命令参数原文、MCP 参数或凭据。

操作入口：`Engine::operations(ticket)` 列出工单的回执；`Engine::resolve_operation(id, note)` 仅接受 `unknown_effect`，说明为 1–2000 字。

## 覆盖的操作与对账规则

启动时 `Engine::start` 在恢复合入队列、中断运行中任务和调度之前调用 `operations::reconcile`。仍为 `started`/`cancel_requested` 的回执按下表分类；本进程正在执行的操作不参与。Git 检查在数据库事务之外运行，结论与相关行的更新在同一事务中写入。

| 种类 | 副作用 | 已发生 | 未发生 | 其他情况 |
| --- | --- | --- | --- | --- |
| `project.git_init` | 工作目录 `git init` | 目录可作为工作树读取 → `confirmed` | 目录中没有 `.git` → `failed_before_effect` | 有元数据但不可读 → `unknown_effect` |
| `project.initial_commit` | 初始提交 | `HEAD^{commit}` 存在 → `confirmed` | 仓库仍无提交 → `failed_before_effect` | 仓库不可读 → `unknown_effect` |
| `workspace.worktree_add` | `git worktree add -b` | 目录存在且根目录、分支、公共仓库与记录一致 → `confirmed`；下次准备复用该目录 | 目录和分支都不存在 → `failed_before_effect` | 分支已建而目录缺失、目录与记录不符 → `unknown_effect` |
| `git.dependency_merge` | 把前置分支合并进依赖写任务的工作区 | HEAD 包含前置提交 → `confirmed` | HEAD 仍是合并前提交且干净 → `failed_before_effect` | 存在 `MERGE_HEAD` 或 HEAD 意外 → `unknown_effect`，保留现场，不自动中止合并 |
| `git.checkpoint` | 隔离工作区检查点提交 | HEAD 的父提交是记录的合并前提交且工作区干净 → `confirmed` | HEAD 未变 → `failed_before_effect` | 其他提交 → `unknown_effect` |
| `integration.candidate` | 队列候选准备：检查点、`--detach` 候选工作区、合并、检查。不修改原项目分支 | 正常结束记 `confirmed`，`detail.state` 为候选结论 | 原项目 HEAD 仍是基线 → `failed_before_effect`，`detail.candidates` 列出遗留候选目录 | 原项目在准备期间改变 → `unknown_effect` |
| `integration.publish` | 确认后 `merge --ff-only` 到原项目 | 原项目 HEAD 等于候选提交 → `confirmed`，并按正常确认写入 merged、重新排队同源候选、记录带 `reconciled` 的 `integration.confirmed` | HEAD 仍为基线且干净 → `failed_before_effect`；验证仍有效时队列回到待确认，否则标为过期 | 分支切换或 HEAD 意外 → `unknown_effect`，队列标为 `blocked` |
| `workspace.integrate` | 旧版操作者合入的快进 | 同上，工作区行写为 merged | 工作区行写为 failed，需重新验证 | 工作区行写为 failed，需人工核对 |
| `process.validate` | 旧版操作者验证命令 | 正常结束 → `confirmed`（含退出码、是否超时） | 程序无法启动 → `failed_before_effect` | 重启时一律 `unknown_effect`，工作区行写为 failed |
| `delivery.behavior_check` | 交付行为检查进程 | 有结果行 → `confirmed` | 启动前失败 → `failed_before_effect` | 尝试没有结果 → `unknown_effect`；该尝试保持未通过并继续遮盖更早的通过，不补写结果、不重跑 |
| `mcp.tool_call` | `SA_TOOL` 调用 | — | — | 重启时仍为 `pending` 的 `studio_tool_receipts` 改为 `uncertain`，记录 `mcp.result` 事件，新建一条 `unknown_effect` 回执；不重发调用 |

发布不再在打开的数据库事务内运行 Git：确认前先在事务中把队列标为 `publishing`，`publish` 在快进前写入回执，快进后单独写入结果。结果事务或快进命令本身报错时，平台立即按上表检查原项目；已经快进就记为合入，不再合并第二次。调度租约在整个过程中阻止相关工作区被准入。

没有回执的旧版 `validating`/`integrating` 工作区行（升级前遗留，或在写回执之前退出）启动时改为 failed 并要求重新验证。`integrating` 行的提交已包含在原项目中时，另建一条 `workspace.integrate` 的 `unknown_effect`，提示缺少合入回执。

## 取消

旧版验证命令和交付行为检查的进程树登记在本进程中。`Engine::cancel` 在停止工单的同一事务中把这些操作标为 `cancel_requested`，提交后发出取消信号。进程树经 Job Object 确认终止后记为 `cancelled`；无法确认终止时记为 `unknown_effect`。行为检查也会因关闭程序或调用方取消而结束，此时先补记 `cancel_requested` 再记 `cancelled`。旧版验证属于操作者维护任务，关闭程序时照原设计等待其完成，只有停止工单会取消它。停止工单从不查询回执，未知结果不会阻塞停止，也不会使已停止工单在重启后恢复。

## 不在覆盖范围内

- 执行端 CLI 内部的工具调用、文件写入和网络请求。平台只看到执行端报告的事件，不能为它们建立精确回执；这些副作用仍按原有“中断后需核对实际状态”的说明处理。
- 任务完成检查命令（`check.started`/`check.result`）和合入队列检查命令的内部运行。候选准备只有一条整体回执，检查命令本身的外部副作用不记录。
- 协作总线的 MCP 直连调用（`call_direct`）没有回执；只有经 `SA_TOOL` 的调用有。
- 远端服务、Git 远端和任何外部 API。平台不推送、不调用远端，也不能声明 exactly-once。
- 对账不清理遗留的候选工作区，不中止未完成的合并，不重置分支；这些只列在 `detail` 中，交给操作者。
- 非 Windows 平台上进程组可能被子进程脱离，取消确认弱于 Windows Job Object。

## 故障注入

测试通过 `EngineOptions { failpoints: Some(Failpoints::new()), .. }` 打开注入，生产默认 `None`，每处检查只是一次判断。名称为 `<kind>.<阶段>`：

| 阶段 | 位置 |
| --- | --- |
| `begin` | 写入回执之前；`Fault::Error` 模拟回执写入失败，此时不执行副作用 |
| `after_begin` | 回执已提交、副作用之前 |
| `after_effect` | 副作用已返回、结果之前 |
| `receipt` | 成功结果事务内（发布、依赖准备、旧版验证/合入、行为检查） |

`Fault::Error` 返回普通错误，走原有错误路径；`Fault::Crash` 返回 `SimulatedCrash`，并让该次操作之后不再写账本，模拟进程退出。除下列例外，每个覆盖的种类都有这四个点：`integration.candidate` 没有 `receipt` 点（其结果事务失败会令调度器停止，属于原有行为）；`mcp.tool_call` 只在启动对账中生成，没有注入点。测试也直接构造崩溃现场：手工执行 Git 效果后插入 `started` 回执，再关闭并重开引擎。

## 验证

`crates/sa-engine/tests/operations.rs` 使用真实临时 Git 仓库，覆盖：发布快进后退出（重启确认合入、reflog 只增加一次、再次确认被拒绝）；保存意图后退出（`failed_before_effect`，回到待确认后可再次确认）；回执写入前失败和结果回执失败（不盲目合并、不重复合并）；创建工作区后退出（确认并复用目录）；工作区、检查点、项目初始化回执按仓库状态分类；`MERGE_HEAD` 现场为 `unknown_effect`、不被中止，停止工单照常生效且重启后保持停止、没有模型调用，人工核对后为 `resolved`；运行中的旧版验证被停止工单取消，事件依次为 started、cancel_requested、cancelled；旧版工作区行和挂起的 MCP 回执在重启时对账，再次启动不重复。

`delivery::behavior::tests` 覆盖行为检查超时（`confirmed`，状态 invalid）、取消（`cancelled`），以及检查结束后、写结果前退出：该尝试没有结果、遮盖先前的通过、重启后为 `unknown_effect`，不补写结果。

```powershell
$env:CARGO_TARGET_DIR = "$PWD\target\audit"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/cargo.ps1 test -p sa-engine --test operations -- --test-threads=2
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/cargo.ps1 test -p sa-engine --lib -- operations behavior
```

真实模型调用、断电和非 Windows 平台不在这些测试的证明范围内，也没有进行用户人工验收。
