# 用量调度器：协作者槽位与额度窗口

状态：引擎、存储、适配器与桌面界面已实现，离线测试通过；尚未在真实额度耗尽的工单上观察过（见[验证](#验证)）。

此前调度器对同一工单同时运行的协作者数量没有上限。订阅账户的用量窗口（例如 5 小时窗口）耗尽时，上游的拒绝被归为 `Billing`，重试次数为 0，没有可轮换的供应商时任务直接失败，长工单因此中断，而不是等额度重置后继续。

用量调度器做四件事：限制每张工单同时运行的协作者数量；记录执行端报告的额度窗口；窗口耗尽时让任务等待重置而不是失败；窗口接近用尽时为主控留出余量，并把这些情况告诉主控。

## 设计来源

- [DeepSeek Super-agent prototype `docs/003-execution.md`](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc/docs/003-execution.md)（commit `8c32a9dc`，“Concurrency” 节）：仅作设计参考，非移植代码。以配置并发上限约束 worker 槽位，并额外保留一个槽位给人工消息或状态查询，防止新派工阻塞人工继续推进工单；本平台将主控（根任务）作为永不计数的保留槽位。
- [Codex app-server rate limits](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt)：仅作协议设计参考，非移植代码。定义 `account/rateLimits/read` 与 `account/rateLimits/updated` 中的 `usedPercent`、`windowDurationMins`、`resetsAt`。2026-11-17 用本机 `codex app-server` 0.169.0 实际请求过一次（不调用模型）：带或不带 `params` 都返回结果，响应除 `rateLimits` 外还含 `accountId`、`planType` 等账户字段。
- Claude Code stream-json `rate_limit_event` 与 `rate_limit_info`（`status`、`resetsAt`、`rateLimitType`、`utilization`、`unifiedWindows`）：仅作协议设计参考，非移植代码。字段名读取自本机安装的 Claude Code 2.1.325；在 API-key、Bedrock、Vertex 会话以及不透传 `anthropic-ratelimit-unified-*` 响应头的网关中始终缺席。
- [Anthropic《How we built our multi-agent research system》](https://www.anthropic.com/engineering/multi-agent-research-system)：仅作设计参考，非移植代码。参考其按任务复杂度调整子 agent 数量的编排原则。

## 行为

### 协作者槽位

- 每个工单同时处于 `running` 状态的非根协作者（`parent IS NOT NULL`）最多为 `max_collaborators` 个（`0` 表示不限制，默认 `4`）。根任务（`parent IS NULL`）永不计数、永不被该规则拦阻，始终作为主控与操作者的通道。
- 为避免超限工单的排队协作者占满每轮 `READS_PER_SLICE`（8 条）的只读候选切片并饿死其他工单，引擎在查询候选前先统计已满额的工单，在 SQL 中排除其非根任务，并在准入循环内随新准入递增计数、直接跳过后续协作者。
- 该检查位于目录与维护检查之后、`route_blocked` 之前，因此被跳过的任务不申请熔断探测，也不消耗信箱消息；不引入新任务状态，受限任务保持 `queued`，在后续调度周期（默认 80 ms）重新评估。

### 额度窗口的来源与存储

- **Claude**：记录每轮最后收到的 `rate_limit_info`。解析 `unifiedWindows` 各条目及顶层 `rateLimitType`、`utilization`、`resetsAt`；仅当顶层 `status == "rejected"` 且 `isUsingOverage` 不为 true 时，将 `rateLimitType` 对应窗口标为 `exhausted`（`five_hour` 记 300 分钟，`seven_day*` 记 10080 分钟）。
- **Codex**：线程启动或恢复后发送 `account/rateLimits/read`（API-key 供应商报错时静默忽略），并在线程过滤器之前记录无线程的 `account/rateLimits/updated` 通知。读取结果写入账本时只保留 `rateLimits`，不保存账户 ID 与套餐信息。解析 `rateLimits.primary` 与 `secondary`（`used = usedPercent / 100`、`resets_at = resetsAt`、`minutes = windowDurationMins`、`exhausted = usedPercent >= 100`，若 `rateLimitReachedType` 非空且无窗口达 100%，将最高占用窗口标为耗尽）。
- **其他执行端**：原生帧解析返回空，仅在失败归类为 `quota:` 时被动记入窗口。

引擎通过 `sa_store::quota` 将观测写入 `quota_windows` 表；仅当出现实质变化（新增记录、`exhausted` 翻转、`resets_at` 偏差超 60 秒或取整百分比 `used` 变化）时提交严格事务 `tx`（不走 `telemetry_tx`，以确保准入可见）并每批至多记录一条 `quota.window` 事件。任意 `(agent, provider)` 轮次成功时，在回复提交事务中删除该对的 `exhausted` 行。

| `quota_windows` 列 | 类型与含义 |
| --- | --- |
| `agent`, `provider`, `kind` | 文本主键；`kind` 为 `five_hour`、`seven_day`、`primary`、`secondary` 或 `unknown` 等 |
| `used`, `resets_at`, `exhausted`, `minutes` | 已用比例（`1.0` 为满额）、重置 Unix 秒数、是否耗尽拒收（0/1）、窗口分钟数 |
| `observed_at` | 观测 Unix 秒数；`resets_at <= now` 或无 `resets_at` 且早于 6 小时视为过期 |

### 额度耗尽时等待重置

- **错误分类与轮换优先级**：`quota:` 前缀错误保持 `ErrorClass::Billing`（`AdapterError::quota_exhausted()`），使同执行端的多供应商轮换继续最先执行。Codex `usageLimitExceeded` 归为 `quota:codex`；文本匹配 `usage limit reached`、`hit your usage limit`、`hit your limit`、`hit your session limit`、`5-hour limit`、`five-hour limit`、`weekly limit`、`usage_limit_reached` 时归为 `quota:text` 并提取相对时长或 `|<unix_s>` 作为 `retry_after`（上限 8 天）；Claude 终态出现 `api_error == "usage_limit_reached"` 或最后 `rate_limit_info.status == "rejected"`（非 overage）时归为 `quota:<rateLimitType 或 window>`。普通 `usage limit`、`insufficient_quota`、`credit balance`、`payment required` 仍按常规 `Billing` 处理。
- **只读子任务转派**：当 `e.quota_exhausted()` 且恢复决策不是 `Rotate` 时，新增 `"quota_exhausted"` 作为跨执行端 failover 原因；根任务与写任务仍被排除。
- **窗口记录**：`quota:` 失败时先写入耗尽窗口。`resets_at` 依次取 `now + retry_after`、同供应商未过期且 `used >= 0.95` 或已耗尽的最早 `resets_at`（此时耗尽标记写在那个已知窗口上），都没有则取 `now + 30 分钟` 作为探测时间，`kind` 记为 `unknown`。
- **准入拦截**：开启 `quota_guard` 时，准入在 `route_blocked` 之后对已绑定路由检查窗口；未绑定路由在 `execution_route` 解析后检查并返回 `TurnError::Quota`。命中耗尽窗口时先走暂停检查与只读子任务转派，否则推迟到重置后 60 秒（已绑定记 `turn.deferred`，未绑定走 `requeue` 记 `turn.quota_wait`），不计入 `route_failures`，也不触发熔断。只拦截 **6 小时内会重置**的窗口：周额度这类几天后才重置的窗口不在准入阶段扣住任务，任务照常运行，被拒绝后按下一条处理。
- **等待重置**：在 `recover` 的 `Recovery::GiveUp` 分支中，若 `err.quota_exhausted()` 且 `quota_guard` 开启，任务保留原生会话重新排队到重置后 60 秒，记 `turn.quota_wait`。每个等待周期只在第一次等待时给上级发一条平台留言（`class` 为 `quota_window`），避免反复唤醒主控。重置时间距现在超过 6 小时时仍放弃任务，记 `turn.gave_up`（带 `quota_until`），结果里写明 UTC 重置时间；自上次 `reply` / `user.follow` / `retry` 以来已有 12 次 `turn.quota_wait` 时也放弃。关闭 `quota_guard` 时，行为与此前完全一致。
- **顾问**：开启 `quota_guard` 时，若顾问所在的 `(agent, provider)` 有当前耗尽的窗口，`advise` 立即返回错误并给出预计恢复时间，不启动顾问轮次。

### 为主控保留额度

开启 `quota_guard` 且 `(agent, provider)` 的未过期窗口未耗尽、但 `used >= 0.90`、已知 `resets_at` 且在 6 小时内重置时，窗口处于 `Reserve` 状态。该规则**仅**拦阻与本工单根任务已绑定路由具有相同 `(agent, provider)` 的非根协作者（延后至 `until + 60` 秒，并按路由绑定情况记录 `turn.deferred`、`reason: "quota_reserve"` 或未绑定重入队的 `turn.quota_wait`，只读子任务先尝试转派）。根任务自身以及使用其他 `(agent, provider)` 的协作者均不被保留规则限制。

### 告知主控与协作者

引擎在 `collaboration_preferences` 同级按需插入 `usage_budget`（已加入 `context_delivery::CURRENT` 列表，随续轮重复投递）：

```json
{
  "usage_budget": {
    "max_parallel_collaborators": 4,
    "windows": [
      {
        "agent": "claude",
        "provider": "<provider>",
        "window_minutes": 300,
        "used_percent": 85,
        "resets_in_minutes": 95,
        "state": "ok"
      }
    ]
  }
}
```

- `max_parallel_collaborators` 为 `0` 时省略；`windows` 仅列未过期且 `used >= 0.5` 或已耗尽的窗口，`used_percent` 与 `resets_in_minutes` 按 5 取整（未知数字省略）以保持前缀稳定，`state` 为 `ok`、`reserve`（`used >= 0.90` 且 6 小时内重置，即准入确实会扣住协作者时）或 `exhausted`，空数组省略；两项均无或 `quota_guard` 关闭且无并发上限时省略整个 `usage_budget`。
- 引导文案方面：`BASE` 改为声明受 `usage_budget.max_parallel_collaborators` 限制、超出部分排队且不限委托深度；新增固定引导 `ECONOMY` 要求主控按任务复杂度控制团队规模、将繁重机械工作交给低成本协作者、根据 `reserve`/`exhausted` 切换执行端或等待而不在耗尽窗口上死循环重试并在采信汇报前核对文件与检查结果；`BUS` 补充一句在决策难逆转或同一问题失败两次时调用一次 `advise`。

## 设置

两项设置位于桌面端 **Token 与缓存** 面板；引擎在每次准入周期开始时实时读取，保存后无需重启即作用于后续准入：

| 设置键 | 类型与默认值 | 说明 |
| --- | --- | --- |
| `max_collaborators` | `u32`，默认 `4`（`0` = 不限制） | 单工单同时处于 `running` 的非根协作者上限；根任务不计数 |
| `quota_guard` | `bool`，默认开启（`true`） | 开启时在窗口耗尽后等待重置并启用 90% 主控保留；关闭时恢复此前 `Billing` 失败行为 |

## 事件

| 事件 | 关键载荷字段 | 触发时机 |
| --- | --- | --- |
| `quota.window` | `agent`, `provider`, `windows` | 解析原生帧或 `quota:` 失败写入 `quota_windows` 且发生实质变化时（每批至多一条） |
| `turn.quota_wait` | `until`, `window`, `reason`（`quota_window` \| `quota_reserve`）, `route` 等 | `Recovery::GiveUp` 转入等待重置，或未绑定路由经 `execution_route` 命中耗尽/保留规则而重入队 |
| `turn.deferred` | `until`, `reason`（`quota_window` \| `quota_reserve` \| `circuit_open`）, `window`（额度原因时） | 准入阶段因已绑定路由命中窗口耗尽、90% 主控保留或熔断器打开而延后准入 |
| `turn.gave_up` | `class`, `code`, `quota_until` | 开启 `quota_guard` 但重置等待超过 6 小时（写明 `quota_until`）或达到 12 次等待上限而放弃任务 |

## 边界与局限

- 这是调度控制，不是硬性 Token 或费用上限。
- 不上报窗口数据的路由（API key、不透传 `anthropic-ratelimit-unified-*` 头的多数网关、DeepSeek Harness、Antigravity、ACP）只能被动等待：无法事先触发 90% 保留或提前拦截，只能在收到拒收错误后进入重置等待。
- 90% 主控保留仅在协作者与本工单根任务已绑定路由共享同一 `(agent, provider)`、且窗口在 6 小时内重置时生效；使用其他供应商的协作者、根任务尚未绑定路由、或周额度这类长窗口的场景不受此规则拦阻。
- 槽位上限按工单计算，不是全局上限：同时运行多张工单时，协作者总数是各工单之和。写入任务仍先受目录串行规则约束。
- `advise` 顾问咨询（`Engine::consult`）与 Claude 在单轮内自行派生的内部子 agent 不计入 `max_collaborators` 槽位。
- 同一账号下其他外部程序消耗的额度，只有在本工作台收到下一条窗口观测帧或被拒收时才会更新。
- 重置时间距离当前超过 6 小时的限制（典型如周额度）仍会直接使任务失败，并在结果中给出重置时间。
- 受限于标准库缺少本地时区偏移，失败与提示消息中的时间统一以 UTC（`yyyy-MM-dd HH:mm UTC`）显示。

## 验证

2026-11-17 的结果、未验证项和复验命令见 [验证方法](../verification.md#用量调度器与读路径2026-11-17)。概括：全量离线测试、Clippy、真实 Claude / Codex CLI 对回环假模型的联测和旧工单库副本续跑通过；真实额度耗尽后的等待与续跑、Claude `rate_limit_event` 的真实样本、以及引导文字对实际用量的影响都还没有观察过。

下一次真实工单遇到额度窗口时，核对账本里的 `quota.window`、`turn.quota_wait` 事件和任务是否在重置后沿用原会话继续，再把结果补到这里。
