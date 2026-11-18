# 用量账目与现状报告

本文对应[长期设计](../architecture-evolution.md) §7 和 P0A。账目只读日志，不改 schema，不调用模型，也不使用价格表。实现见 [ledger.rs](../../crates/sa-store/src/ledger.rs)，用量归一化沿用 [usage.rs](../../crates/sa-store/src/usage.rs)。

## 账目内容

每一次可能消耗模型用量的执行只列一次，并标明三项：

| 字段 | 取值 | 判定依据 |
| --- | --- | --- |
| `role` | `lead`、`collaborator`、`advisor` | 主任务（无父任务）、协作者、顾问调用（`advice:<seq>`） |
| `kind` | `initial`、`continuation`、`retry`、`repair`、`consult`、`unallocated` | 第 1 轮为 initial；上一轮有 `turn.retry_scheduled`、`route.rotated`、`turn.route_wait` 或 `auto.retry` 为 retry；上一轮有 `check.bounced` 或 `integration.bounced` 为 repair；其他后续轮为 continuation |
| `outcome` | `replied`、`retried`、`failed`、`abandoned`、`interrupted`、`running`、`unknown` | 本轮的 `reply`、重试、`failed` 或 `turn.gave_up`、`cancelled`、`interrupted` 事件 |

每条执行带自身用量。数值来自与工单总量相同的归一化，所以账目不会统计总量没有统计的样本：

- Codex 按每轮增量记账。
- Claude 取每轮的 result；没有 result 时取主循环消息。
- DeepSeek 按步骤记到首次出现的轮次。
- Antigravity 的会话累计值无法拆到单轮，单列为 `unallocated`。

失败、取消和中断的执行仍然保留已报告的用量。分组 `buckets` 按 role × kind × outcome 求和；某个字段只有在组内所有已报告执行都提供时才算已知。

## 覆盖与缺口

覆盖率分为两项，不能混为一个比例：

- 多少次执行报告了用量（`attempts` / `reported_attempts`）。
- 已报告的执行中，各字段已知的次数。

无法核算的部分以原因代码列出，不显示为零：

| 原因代码 | 含义 |
| --- | --- |
| `unreported_attempt` | 支持计量的执行端在该轮没有报告用量 |
| `no_usage_protocol` | 执行端不提供可识别的用量（如 ACP） |
| `native_subagents_excluded` | Claude 只统计主循环，原生子代理用量未单独报告 |
| `partial_samples` | 样本缺失、累计值回退或重复更正，已报告部分不含它们 |
| `cumulative_unallocated` | 会话累计用量无法分到具体轮次 |
| `shared_native_session` | 多个任务报告了同一个原生会话，累计值无法在任务间分摊；工单总量标为 partial |
| `advisor_unreported`、`advisor_partial` | 顾问调用没有用量或用量不完整 |
| `advisor_records_incomplete` | 顾问调用记录缺失或无法对应到调用者 |
| `no_usage_events` | 任务在日志中没有任何用量相关事件 |

金额 `cost` 始终为 null，因为还没有带版本、日期、币种和计费项的价格表。账目规则版本为 `rules_version`=1，用量归一化规则为 `usage_rules_version`=2（含顾问调用）。

## 使用

请对工单库副本运行（用 SQLite backup 复制）：

```powershell
scripts\cargo.cmd run -p sa-store --example ledger -- --data <副本目录> --output status.json
scripts\cargo.cmd run -p sa-store --example ledger -- --data <副本目录> --ticket 7 --output ticket-7.json
```

不带 `--ticket` 时，输出全部工单的汇总：

- 每张工单的总量、覆盖和缺口计数。
- 跨工单的分组。

带 `--ticket` 时，另外输出该工单逐次执行的明细。报告只含计数、执行端名称和原因代码，不含目标、提示词、回复或原生会话标识。输出文件必须尚不存在。

## 现状（2026-11-18，真实工单库只读副本）

| 项目 | 结果 |
| --- | --- |
| 工单数 | 7 |
| 执行次数 | 759 次 |
| 报告了用量的执行 | 556 次 |
| 其中 total 已知 | 549 次 |
| 缺口：`unreported_attempt` | 203 |
| 缺口：`partial_samples` | 68 |
| 缺口：`native_subagents_excluded` | 40 |
| 缺口：`cumulative_unallocated` | 16 |

顾问调用 6 次，均早于顾问计量，没有用量。主任务的平台重试中有 112 次以 `retried` 结束，只有 4 次报告了用量，多为额度或路由等待期间的空转。完整数据在 `tmpstate/evolution-full/status-report.json`（本机，不入库）。

这份现状只说明已有日志能解释哪些支出、哪里有缺口，不构成质量或节费结论。

## 限制

- 只覆盖日志中可观察的执行。执行端内部未暴露的调用、平台检查进程和本地构建不计入。
- 重试、修复的分类取决于上一轮的事件。日志缺事件的旧工单可能被归为 continuation。
- 不同执行端、模型的 Token 不能直接相加为费用。总量只在覆盖口径一致时比较。
