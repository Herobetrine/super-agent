# 用量账目与现状报告

本文对应[长期设计](../architecture-evolution.md) §7 和 P0A。账目只读日志，不改 schema，也不调用模型。实现见 [ledger.rs](../../crates/sa-store/src/ledger.rs)，用量归一化沿用 [usage.rs](../../crates/sa-store/src/usage.rs)，价格表与折算见 [prices.rs](../../crates/sa-store/src/prices.rs)。

## 账目内容

每一次可能消耗模型用量的执行只列一次，并标明三项：

| 字段 | 取值 | 判定依据 |
| --- | --- | --- |
| `role` | `lead`、`collaborator`、`advisor` | 主任务（无父任务）、协作者、顾问调用（`advice:<seq>`） |
| `execution` | 0、1、2… | 同一逻辑轮次的第几次实际执行。操作员重试中断或失败的任务时沿用轮次号，但会重新接单（`turn.admitted`），每次接单都是一次新的执行 |
| `kind` | `initial`、`continuation`、`retry`、`repair`、`consult`、`unallocated` | `execution` 大于 0 为 retry；否则第 1 轮为 initial；上一轮有 `turn.retry_scheduled`、`route.rotated`、`turn.route_wait` 或 `auto.retry` 为 retry；上一轮有 `check.bounced` 或 `integration.bounced` 为 repair；其他后续轮为 continuation |
| `outcome` | `replied`、`retried`、`failed`、`abandoned`、`interrupted`、`running`、`unknown` | 本次执行的 `reply`、重试、`failed` 或 `turn.gave_up`、`cancelled`、`interrupted` 事件 |

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
| `native_subagents_excluded` | 日志显示该 Claude 任务（或顾问调用）运行了原生子代理，而计数只含主循环。证据是带非空 `parent_tool_use_id` 的原生帧（超过读取上限的帧也在 SQLite 中检查），或 `Task`/`Agent` 工具调用。没有这类证据时不报这个缺口 |
| `partial_samples` | 样本缺失、累计值回退或重复更正，已报告部分不含它们 |
| `cumulative_unallocated` | 会话累计用量无法分到具体轮次 |
| `shared_native_session` | 多个任务报告了同一个原生会话，累计值无法在任务间分摊；工单总量标为 partial |
| `advisor_unreported`、`advisor_partial` | 顾问调用没有用量或用量不完整 |
| `advisor_records_incomplete` | 顾问调用记录缺失或无法对应到调用者 |
| `no_usage_events` | 任务在日志中没有任何用量相关事件 |

Claude 计数的范围（`scope`）始终是主循环（`main_loop`）。只有日志显示运行过原生子代理时，任务才因此标为 partial，并带 `native_subagents: true`。子代理的 Token 不从 `modelUsage` 推算。

账目规则版本为 `rules_version`=3，用量归一化规则为 `usage_rules_version`=3。账目版本 2 加入顾问调用；版本 3 加入模型归属和按牌价折算的金额，`native_subagents_excluded` 改为只在有证据时报告。用量版本 3 把同一轮次的多次执行分开计量。

此前，被中断的那次执行与随后的重试会合并成一条记录，Claude 的终态用量会覆盖中断执行已经报告的消息用量，导致少计。2026-11-18 的 Opus 5.5 受控试验中，一次中断执行有约 11.7 万输入 Token 被这样漏掉。修正后，未完成的消息只记入输入，输出未知，因此总量显示为未知，不再少计。

## 模型归属

每条执行带 `attribution`（`provider`、`model`、`source`），按以下顺序取证据，取不到就是 `unknown`，不猜：

| `source` | 依据 |
| --- | --- |
| `native_init` | 本次执行 Claude 的 `system`/`init` 帧报告的模型 |
| `native_message` | 本次执行 Claude 主循环消息的 `message.model`（`<synthetic>` 等不算） |
| `route` | 本次执行的 `route.requested` 事件 |
| `route_carried` | 本次执行没有自己的路由事件，沿用同一任务上一次执行的路由 |
| `route_rotated` | 上一次执行之后发生 `route.rotated`，取其切换目标 |
| `advice_route` | 顾问调用在 `advice.started` 记录的路由 |
| `job_route` | 任务当前绑定的路由（`native_routes`）。只在任务从未切换路由时使用 |

`provider` 是路由的上游或网关名称（例如本地网关），不是价格表里的厂商。报告不含原生会话标识和提示词内容。

## 金额（按 API 牌价折算）

`api_cost` 是按各厂商官方 API 牌价折算的**等价金额**，不是实际账单。订阅额度占用另见额度窗口，两者不能互相换算（§7.3）。旧字段 `cost` 保留为 null，供只认字符串的旧读取方解析。

价格表是入库的数据文件 [prices/2026-11-20.json](../../crates/sa-store/prices/2026-11-20.json)，编译时嵌入，版本 `table_version`=`2026-11-20`。每个条目记录厂商、精确型号 ID 和文档列出的别名、币种、`effective_from`、来源 URL、读取日期 `retrieved`、口径 `api_list_price`，以及每百万 Token 的未缓存输入、缓存读取、缓存写入（按有效期分档）和输出价格。推理/思考 Token 属于输出，按输出价计。

折算规则：

- 只按型号 ID 或列出的别名精确匹配，不按前缀、家族或强度后缀推断。路由经过网关时也按厂商牌价折算，匹配到的厂商记在 `price_provider`。
- 未缓存输入 = 输入 − 缓存读取 − 缓存写入（输入含缓存）。输入、缓存读取、缓存写入、输出四项必须都已报告，否则不折算。
- 计算用整数，金额以精确小数字符串保存。
- 只在同一币种内求和，按币种分别列出，不做汇率换算。
- 有任何报告了用量的执行没有折算，或用量本身不完整（账目 `partial`），汇总就标 `partial`：金额只含已折算部分。
- 覆盖率单列：`attempts_with_usage`（报告了用量的执行，含未分配的会话累计）、`attempts_priced`，以及按原因计数的 `unpriced`。

档位与计费项：

- 按请求分档（Claude Haiku 5.5 超过 100,000 Token、OpenAI 输入超过 272K 整次按高价）：只有能证明每个请求都不超过阈值时才按标准价。证据是这次执行的全部输入；Codex 另按每帧推出单个请求的上界（某帧增量等于 `last` 时就是该请求本身，否则增量之和也是每个请求的上界）。证明不了就是 `tier_ambiguous`。
- DeepSeek 分高峰和非高峰。高峰为 UTC 周一至周五 01:00–04:00 和 06:00–10:00；中国法定节假日全天非高峰，但价格表没有节假日表。只有整次执行（接单到结束）完全落在非高峰时才按非高峰价，否则 `tier_ambiguous`。
- 缓存写入：Anthropic 按 `usage.cache_creation` 的 5 分钟和 1 小时分项计价，没有分项且写入大于零时为 `cache_ttl_unknown`。OpenAI 分 30 分钟和 12 小时两档，Codex 的用量帧不分档，所以写入大于零时同样不折算。DeepSeek 只区分命中与未命中，写入缓存的输入按未命中价。Gemini 只有隐式缓存，没有写入计费项。
- 价格表只含标准速度、全球路由、非批量的价格。Claude 用量标明快速模式（`usage.speed` 不是 `standard`）或美国区推理（`inference_geo` 为 `us`）时为 `nonstandard_mode`。
- `effective_from` 是价格表能担保价格有效的最早日期。本版取读取日 2026-11-20，因此更早开始的执行为 `price_not_effective`。要折算更早的执行，需要新增一版带有已核实更早生效日期的价格表。

未折算原因代码：

| 原因代码 | 含义 |
| --- | --- |
| `model_unknown` | 无法确定这次执行的模型 |
| `price_unknown` | 价格表没有该型号的官方牌价，或该型号没有对应的计费项 |
| `price_not_effective` | 执行早于该条目的 `effective_from` |
| `time_unknown` | 执行时间未知，无法判断价格是否适用 |
| `nonstandard_mode` | 快速模式或区域推理加价，价格表不含 |
| `tokens_incomplete` | 某个计费项未报告，或缓存计数大于全部输入 |
| `tier_ambiguous` | 无法判定长上下文档位或高峰档位 |
| `cache_ttl_unknown` | 缓存写入没有按有效期分项 |
| `price_table_invalid` | 嵌入的价格表无法解析（测试防止发生） |

价格表 `2026-11-20` 的来源（均于 2026-11-20 读取）：

| 厂商 | 来源 | 条目 |
| --- | --- | --- |
| Anthropic | <https://platform.claude.com/docs/en/about-claude/pricing>；型号 ID 与日期快照别名见 <https://platform.claude.com/docs/en/about-claude/models/overview> 及各型号页 | `claude-fable-5-2`、`claude-fable-5-1`、`claude-fable-5`、`claude-opus-5-5`、`claude-opus-5`、`claude-opus-4-8`、`claude-opus-4-7`、`claude-opus-4-6`、`claude-opus-4-5`、`claude-sonnet-5-5`、`claude-sonnet-5`、`claude-sonnet-4-6`、`claude-sonnet-4-5`、`claude-haiku-5-5`（100K 分档）、`claude-haiku-4-5`。4.6 及以后的型号另有 Claude Code 的 `[1m]` 别名：这些型号 1M 上下文按标准价 |
| OpenAI | <https://developers.openai.com/api/docs/pricing>；缓存写入分档见 <https://developers.openai.com/api/docs/guides/prompt-caching> | `gpt-6.2-sol`、`gpt-6.1-sol`、`gpt-6.2-astra`（272K 分档） |
| DeepSeek | <https://api-docs.deepseek.com/quick_start/pricing>；命中/未命中见 <https://api-docs.deepseek.com/guides/kv_cache> | `deepseek-flash`（别名 `deepseek-v4-flash`、`deepseek-v4-flash-vision-exp`）、`deepseek-v4-pro`，USD 价，高峰与非高峰 |
| Google | <https://ai.google.dev/gemini-api/docs/pricing>（付费层 Standard） | `gemini-4-hydrogen`、`gemini-4-neon` |

本项目实际用过、但不在表里或折算不了的情况：

- `deepseek-v4.1-flash`（经第三方网关）不是 DeepSeek 文档里的型号名，保持 `price_unknown`。
- Antigravity 的 `gemini-4-hydrogen-high`/`-medium` 不是 Gemini API 的型号 ID，而且执行端不报告输入，所以不折算。
- Codex 有缓存写入的执行为 `cache_ttl_unknown`。Codex 用量帧不含服务档位，因此按标准档折算没有缓存写入的执行；若在 Codex 中开启了快速档，金额会偏低，这一点无法从日志判断。

评测报告的 `cost` 取同一读事务里账目的 `api_cost`。只有全部有用量的执行都按同一币种折算、且用量完整时，才给出 `amount`，原因为 `api_list_price_equivalent`。否则 `amount` 为 null，原因为 `partial_price_coverage`、`mixed_currencies` 或 `no_priced_usage`。`price_table` 记录价格表版本。对比时只有两边币种和价格表版本都相同，才给出 `cost_delta`。

桌面端“用量账目”显示“按 API 牌价折算”的各币种金额、已折算次数和未折算原因。用量悬浮框只在有原生子代理证据时提示子代理未计入。

## 使用

请对工单库副本运行（用 SQLite backup 复制）：

```powershell
scripts\cargo.cmd run -p sa-store --example ledger -- --data <副本目录> --output status.json
scripts\cargo.cmd run -p sa-store --example ledger -- --data <副本目录> --ticket 7 --output ticket-7.json
```

不带 `--ticket` 时，输出全部工单的汇总：

- 每张工单的总量、覆盖和缺口计数。
- 跨工单的分组。

带 `--ticket` 时，另外输出该工单逐次执行的明细，含模型归属和每次执行的折算金额。报告只含计数、金额、执行端与模型名称和原因代码，不含目标、提示词、回复或原生会话标识。输出文件必须尚不存在。

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

这份现状只说明已有日志能解释哪些支出、哪里有缺口，不构成质量或节费结论。它按账目规则 2 生成：当时每个 Claude 任务都记一次 `native_subagents_excluded`，规则 3 只在有子代理证据时记。这些执行都早于价格表 `2026-11-20` 的生效日期，按规则 3 重算也只会得到 `price_not_effective`。

## 限制

- 只覆盖日志中可观察的执行。执行端内部未暴露的调用、平台检查进程和本地构建不计入。
- 重试、修复的分类取决于上一轮的事件。日志缺事件的旧工单可能被归为 continuation。
- 不同执行端、模型的 Token 不能直接相加为费用。总量只在覆盖口径一致时比较。
- 金额是牌价等价，不含协议折扣、区域加价、批量或快速档、工具调用费（如网页搜索按次计费）和订阅额度。
- Claude Code 内部不经过流式帧报告的辅助请求无法观察；没有子代理证据时，主循环之外的这部分仍不计入。
- 价格表要人工维护：厂商调价或新增型号时，新增一个带新日期的数据文件并切换 `PRICE_TABLE_JSON`，旧版本保留以便追溯。
