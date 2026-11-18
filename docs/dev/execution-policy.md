# 规则化执行策略（P2）

对应 `docs/architecture-evolution.md` §8.2 与 §15.2。策略模块 `crates/sa-engine/src/execution_policy.rs`，版本 `rules-1`。它不调用规划模型，只用确定性规则给派工、重试和修复提出建议，并为每次判断留下回执。

## 两种模式

模式按项目保存（`project_policies`，键与 `project_checks` 相同，即规范化的项目目录），没有记录等于 `record`。

| 模式 | 行为 |
| --- | --- |
| `record`（默认） | 只记录决策，调度、重试、修复的结果与没有策略时完全一致 |
| `apply` | 仅以下三种效果会生效：隔离写入者之间加串行依赖、用尽工单自动恢复预算后放弃重试或修复、同类失败连续 3 次后放弃原样重试。 |

`Engine::execution_policy(cwd)`、`Engine::set_execution_policy(cwd, "record"|"apply")`、`Engine::policy_decisions(ticket, limit)`（按新到旧）。切换只影响之后的决策，不删除历史。模式不会放宽任何授权：权限检查、路由和模型选择都不在策略范围内，主控模型保持不变，不会自动换模型。

## 决策回执

每次决策写入 `policy_decisions`（`id, ticket, job, turn, point, policy_version, mode, inputs, alternatives, selected, reason, applied, created`），同时在同一事务里记录同内容的 `policy.decision` 事件（含决策编号）。回执与触发它的状态变更同事务，所以回滚时一起消失。

## 决策点与规则

| 决策点 | 触发位置 | 输入 | 选项（selected） |
| --- | --- | --- | --- |
| `dispatch` | SA_SEND 新建子任务时（重放和续派不记录） | 是否写入、声明文件、角色、`after` 数量、目标执行端、是否指定模型、同一调用者的活动子任务（写入与否、声明文件）、是否隔离写入者 | `single`、`limited_parallel`、`serialize`、`add_review` |
| `retry` | 平台将要自动重试、轮换路由或 `auto.retry` 时 | 工单自动恢复次数、错误类别、本任务同类失败连续次数 | `retry`、`rediagnose`、`stop` |
| `repair` | 完成检查或集成队列将要退回修复时 | 工单自动恢复次数、退回类型（`check`/`integration`） | `repair`、`stop` |

### dispatch 规则

按顺序匹配：

1. 只读任务选 `limited_parallel`。
2. 写入者的声明文件与活动中的同级写入者重叠（同一路径，或一方是另一方的目录前缀），选 `serialize`；Windows 上比较不区分大小写。
3. 未启用隔离写入者，且新写入者与某个活动写入者都没有声明文件，选 `serialize`。调度器本来就按路径串行这类写入者，所以这只是建议，理由里会这样说明。
4. 角色或正文含并发、权限、迁移、安全、恢复（及 concurrency、permission、migration、security、recovery）的写入者选 `add_review`：建议随后用 `after` 派一个不同执行端的只读独立审查。若同时命中 2 或 3，选 `serialize`，理由里保留审查建议，并在输入里标记 `high_risk`。
5. 没有活动同级写入者选 `single`；其余选 `limited_parallel`（声明文件互不重叠；声明不足以证明重叠时在理由里说明）。

`apply` 的唯一生效效果：选了 `serialize`、该工单启用隔离写入者、且存在文件重叠时，给新任务在重叠的活动同级写入者上插入 `job_deps`，让它们先后执行，回执 `applied=1`。只对同一调用者、同一工单、仍处于 queued/running/waiting 的同级任务加依赖，不会指向已结束的任务或跨工单。其余结果只是建议。

注意：依赖的前置任务若以非 done 结束，调度器会按原有规则跳过依赖它的任务。

### retry 规则

- 工单自动恢复次数 ≥ `TICKET_AUTOMATIC_ATTEMPTS`（40）：`stop`。
- 否则本任务（自上次回复、追加、手动重试、放弃以来）同类失败连续 ≥ 3 次：`rediagnose`。连续次数包含当前这次，类别取 `turn.retry_scheduled` 的 `class` 与 `route.rotated` 的 `reason`；类别不同则重新计数，不比较错误码。
- 否则 `retry`。

自动恢复次数 = 该工单中 `turn.retry_scheduled`、`route.rotated`、`auto.retry`、`check.bounced`、`integration.bounced` 事件的数量。平台重试会同时写 `turn.retry_scheduled` 和配对的 `auto.retry`，所以一次重试计两次，实际可用恢复约 20 次，这是按日志事件计数的结果。

`apply` 下 `stop` 以「工单自动重试与修复已达上限（N 次），请人工处理」放弃任务，`rediagnose` 以「同类失败连续 3 次，平台不再原样重试；请重新诊断后再派发」放弃任务。放弃走原有 `give_up` 路径：任务变为 failed（有子任务时先 draining），子任务的终态报告沿用现有渠道回到上级，不新增通知。已被停止或取消的任务不会进入这条路径（先检查任务仍在运行），策略不会让任何东西复活。`record` 下照常重试，决策 `applied=0`。持续项目原有的每小时重试上限仍然生效，策略只会额外提前放弃，不会放宽。

挂在 `fail()` 里 `auto.retry` 分支（非适配器错误）的判断不含错误类别，只受工单预算约束。

### repair 规则

工单自动恢复次数 ≥ 40：`stop`，否则 `repair`。`apply` 下 `stop` 时不退回：完成检查直接走现有的「次数已满」分支，任务以部分完成结束（持续主控沿用原有平台问题）；集成队列不再派修复，直接标为 blocked 并通知上级。`record` 下照常退回。每次将要退回时记录一条 `repair` 决策。

## 主控反馈 `policy_feedback`

根任务的 packet 增加 `policy_feedback`：

```
{mode, policy_version, automatic_attempts:{used,budget},
 recent:[最近 ≤5 条非默认决策 {job, point, selected, reason(≤160 字), applied}],
 routes:[最近 50 个同项目已结束协作者任务按 (agent, model) 的 done/partial/failed/check_bounced]}
```

默认决策（`single`、`limited_parallel`、`retry`、`repair`）不进入 `recent`。无内容可说（`record` 模式、无自动恢复、无非默认决策、无协作者历史）时整个键不出现。总大小限制 4 KiB，超出时先裁掉 `routes`，再裁掉 `recent`。键存在时才在主控的 guide 末尾追加一句 `guides::POLICY`；固定指南的摘要不受影响。子任务不会收到该键。

## 启用步骤与限制

1. 先保持 `record`，对照 `policy_decisions` 与实际结果，确认规则符合预期。
2. 对要试用的项目调用 `set_execution_policy(cwd, "apply")`；随时可改回 `record`。
3. 默认值未改变：存量工单与未授权项目行为不变。

限制：

- 规则按关键词判断高风险，会有误报和漏报，所以只给建议。
- 规则只看已声明的文件，看不到未声明的写入冲突。
- 预算用日志事件计数，不是 Token 或费用；未知用量不会被当成零。
- `rediagnose` 的 3 次比平台对瞬时错误的 5 次上限更严；只在 `apply` 下生效。
- 没有桌面界面入口，模式只能通过 Engine API 设置。
- 尚未做匹配对照（§8.3 第二阶段）；本阶段只提供记录与有限自动效果。
