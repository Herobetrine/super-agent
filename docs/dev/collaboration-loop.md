# 协作闭环（2026-11-17）

状态：D1 到 D7 **已实现**，离线测试、界面测试和截图检查通过，记录见 [验证方法](../verification.md)。2026-11-17 补做了 Hydrogen 真实工单验收，已覆盖提问、依赖派工和完成检查退回，并发现续派指令及检查覆盖不足的问题；样本结果、修正与未通过项见[真实验收](collaboration-acceptance.md)。这不代表用量与质量目标已全面达成。

目标来自操作者的要求：协作效率更高、交互更舒适、agent 能力更完整、多数情况下更自主、产出质量更高、用量更低。2026-11-17 已做的[用量调度器](usage-governor.md)只覆盖了“订阅额度窗口”这一小块，本方案处理其余部分。

## 1. 依据

以下资料均直接阅读原文，没有经过其他模型转述。

| 来源 | 采纳的要点 |
| --- | --- |
| DeepSeek 原型 [000 Philosophy](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc5d72c033816c6a8110b37f40cf94fba4/docs/000-philosophy.md)、[001 Domain](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc5d72c033816c6a8110b37f40cf94fba4/docs/001-domain.md)、[003 Execution](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc5d72c033816c6a8110b37f40cf94fba4/docs/003-execution.md)、[004 Prompts](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc5d72c033816c6a8110b37f40cf94fba4/docs/004-prompts.md)、[005 Panel](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/8c32a9dc5d72c033816c6a8110b37f40cf94fba4/docs/005-panel.md)、007 Architecture | 以人的注意力为成本；只在需要人行动或结果可用时才说话；一次只问一个具体问题，且用结构化字段而不是从正文推断；执行者的 Report 有固定状态（进展、等待、就绪、受阻、失败、无需处理）；工单阶段由“在等谁”决定，面板先列等人的；重试在同一个执行会话里继续；概念少、设置少 |
| Anthropic [How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) | 多代理约消耗 15 倍于对话的 Token，编码任务可并行的部分比调研少；派工要写清目标、输出格式、边界；按任务规模决定人数；按最终状态评估；同步等待全部子代理是瓶颈；产物落盘、只回传引用 |
| Anthropic [Effective context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) | 子代理各自消耗大量上下文，只回传 1000 到 2000 Token 的浓缩结论 |
| Cognition [Don't Build Multi-Agents](https://cognition.ai/blog/dont-build-multi-agents) | 上下文连续比并行重要；行动隐含决策，互不知情的并行写者会做出冲突决策；子代理最适合回答界定清楚的问题 |
| Claude Code [Agent teams](https://code.claude.com/docs/en/agent-teams)、[Subagents](https://code.claude.com/docs/en/sub-agents)、[Hooks](https://code.claude.com/docs/en/hooks) | 任务可声明依赖，前置完成后自动解除阻塞；`TaskCompleted` / `TeammateIdle` 钩子用确定性检查把未达标的工作退回原执行者；已结束的子代理可带着完整历史被恢复继续工作；复杂任务先出计划再实施 |
| OpenAI [Responses Multi-agent](https://developers.openai.com/api/docs/guides/responses-multi-agent) 与 Codex [Subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents) | `followup_task` 给已有代理追加任务并恢复其轮次；默认并发 3；短任务、顺序链和写同一资源的工作用单代理 |
| Magentic-One [编排提示词](https://github.com/microsoft/autogen/blob/main/python/packages/autogen-agentchat/src/autogen_agentchat/teams/_group_chat/_magentic_one/_prompts.py) | 每轮判断是否在循环、是否有进展；停滞后更新事实并重新规划 |
| Roo Code [Boomerang Tasks](https://docs.roocode.com/features/boomerang-tasks)、Agent Orchestrator [README](https://github.com/OrchestratorInc/agent-orchestrator) | 子任务只把结论带回上级；CI 失败和评审意见退回拥有该分支的执行者 |

## 2. 现状与缺口（已对照代码）

| # | 现状 | 证据 | 影响的目标 |
| --- | --- | --- | --- |
| G1 | 任务结论没有结构：轮次结束且没有未完成的子任务、信件或追加指令，就记为 `done`。受阻、只完成一部分和真正完成在平台看来相同，主控和界面只能读正文判断 | `engine.rs:2207` | 质量、自主、交互 |
| G2 | 没有“向人提问”的机制。总线的 `ask` 只在 agent 之间使用；主控把问题写在回复末尾，工单却显示“已完成” | `engine.rs:2207`、`sa-bus/src/lib.rs:342` | 交互、自主 |
| G3 | 主控不能让已结束的协作者接着干：`SA_NOTE` 只接受排队、运行、等待中的任务；`retry` 只有操作者能对失败或中断的任务使用。审查发现问题后只能新开任务，新任务没有原写者的上下文 | `db.rs:454`、`engine.rs:886` | 效率、用量、质量 |
| G4 | 派工没有依赖关系，主控要等全部直接子任务结束才被唤醒。“实现后审查”每个环节都要多一轮主控 | `db.rs:510`、`dispatch.rs:205` | 效率、用量 |
| G5 | 平台不运行项目自己的检查。验证命令只在“隔离工作区合入”里由操作者手动触发；自动模式的证据门只检查是否填写了文字 | `workspaces.rs:296`、`autonomy.rs:70` | 质量、自主 |
| G6 | 持续运行的主控重复同一个下一步时，退避到 300 秒后无限重试，每次都是一轮模型调用 | `autonomy.rs:155` | 用量、自主 |
| G7 | 会话列表按置顶和项目分组，不区分“在等我”与“在干活”；系统通知只有审批和工单结束 | `sidebar.rs:213`、`notifications.rs:19` | 交互 |

## 3. 设计

七项改动围绕一条闭环：**派工 → 带检查的完成 → 结构化结论 → 需要时原地返工或向人提问**。前四项是协议，第五、六项是平台行为，第七项是界面。除“项目完成检查”外不新增设置。

### D1 结论行 `SA_REPORT`

任何任务在最终回复里用一行声明结论：

```
SA_REPORT {"status":"done|partial|blocked","summary":"不超过 600 字","verified":["核对了什么、怎么核对的"],"unverified":["没有核对的事项"],"artifacts":["相对路径"],"resolved":[{"job":12,"evidence":"…"}]}
```

- 只取最终回复中的最后一行，写入事件 `report` 和 `jobs.outcome`。
- 只在权威终稿中生效，流式预览里的同名行不算。无效的行留下 `protocol.rejected` 回执，不影响其他控制行。
- `partial` 或 `blocked`：原本会记为 `done` 的任务改记 `partial`。`blocked` 不记为 `failed`，因为 `failed` 表示路由或执行故障，会触发“换执行端重派”的引导。
- 主控上下文里每条 `reports[i]` 增加 `outcome`（含平台检查结果，见 D5）。完整回复仍然送达，不做截断：协作者的最终回复本身就是结论，截断会丢信息。
- `resolved` 与 `SA_FINISH` 中的同名字段含义一致，让非自动模式的主控也能说明“失败的子任务已由我或其他任务补上”，避免工单因一个已处理的失败而显示“部分完成”。
- 不写这一行时行为与今天相同。自动模式的主控继续用 `SA_FINISH` 和 `SA_EXHAUSTED` 推进，`SA_REPORT` 只作记录。

### D2 提问行 `SA_ASK`

```
SA_ASK {"id":"稳定短键","question":"一个具体问题，不超过 500 字","options":["选项一","选项二"],"default":"无人回答时的做法，可省略"}
```

- 主控和协作者都能用，所有权限档位可用。任务结束本轮后停在 `waiting`，且不会被“子任务都已结束”唤醒；只暂停提问的这个任务，其余已派出的工作继续。
- 新表 `questions` 保存问题、选项、状态和回答。每个任务同时只有一个未回答的问题，新问题替换旧问题。
- 回复原本会结束任务（完成或部分完成）时，任务改为停在 `waiting`，直到有人回答；回复时还有未结束的子任务或未读信件，任务照常被唤醒，上下文里的 `open_question` 提醒它问题仍未回答。
- 问题的寿命只有一条规则：被回答，或提问任务的下一次回复没有重复同一个 `SA_ASK`（视为不再需要），或任务结束、被取消。操作者对主控追加指令也会撤回主控的问题。
- 回答（`Engine::answer`）在同一事务里关闭问题并以操作者信件送达，重复回答会被拒绝。
- 自动模式的主控提问时，本轮不做自动推进，回答后在原阶段、原会话继续。
- 引导文字规定：只有属于人的决定才问（范围、不可逆操作、凭据、会改变结果的设计取舍），其余自行决定并在结论里写明假设。计划审批不另设机制：主控先给出计划，再用 `SA_ASK` 请人确认。

### D3 续派：让已结束的协作者在原会话里继续

```
SA_SEND {"id":"稳定短键","continue":42,"body":"后续指令"}
```

- 目标必须是发送者自己的直接子任务，状态为 `done` 或 `partial`，没有被自动改派替代，隔离工作区尚未合入。
- 效果：该任务回到 `queued`，轮次加一，沿用原生会话、模型、权限和写入声明；后续指令作为上级信件送达，计入信件预算，同一 `id` 的重放不重复执行。上级随后照常等待它的新结论。
- `failed` 和 `cancelled` 的子任务不能续派。
- 不能与 `to`、`model`、`write`、`after` 同时使用。

### D4 依赖派工 `after`

```
SA_SEND {"id":"review","to":"claude","after":["impl"],"body":"…"}
```

- `after` 列出本任务此前派出的 `SA_SEND` 的 `id`，或自己直接子任务的任务号。只能引用已存在的任务，因此不会成环。
- 成功的续派 `SA_SEND` 消息 ID 也可作为新派工的前置，解析到续派的原任务。普通留言或缺少成功续派回执的 ID 不可使用；同一逻辑轮次的同一 ID 指向不同任务时必须改用任务编号。
- 依赖任务保持 `queued`，全部前置以 `done` 结束后才进入调度，上下文里带有 `prerequisites`（前置任务的结论和完整回复）。任一前置以其他状态结束时，依赖任务记为 `cancelled`，结果写明原因。
- 主控可以在一轮里发出“实现 → 审查”或“三路调研 → 汇总”，全部结束后只被唤醒一次。等待前置的任务不占协作者槽位，也不占每轮的准入名额。
- 前置任务被自动改派替代时，依赖跟随到替代任务；前置被续派后，尚未开始的依赖任务重新等待。

### D5 项目完成检查

- 操作者在 **项目面板 → 协作** 为项目配置最多 4 条检查命令（参数数组，单条最长 600 秒），存入 `project_checks`。模型不能设置或修改，因此不会在受限权限档位下获得额外执行权。
- 触发时机：声明了写入的协作者，或可写工单的主控，在本轮将以完成结束且没有未完成的子任务、未读信件、追加指令时。自动模式的主控只在 `SA_FINISH` 报告 `done` 时触发。任务自己报告 `partial`、`blocked` 或提问时不运行。
- 检查在该任务的执行目录内顺序运行，遇到第一条失败即停。任务在此期间仍算运行中，继续占用写入目录和协作者槽位。停止工单或退出程序会终止正在运行的检查及其子进程。
- 全部通过：结果记入 `job_checks`，主控在 `reports[].checks` 里看到，界面标注为平台检查。
- 有一条失败：把命令和输出末尾（最后 4000 字节）退回同一任务的原会话，最多退回 2 次；第 3 次仍失败，普通任务记为 `partial`，自动模式的主控停下来等操作者决定继续修复还是停止。
- 为了不在没有改动时白等：Git 项目里，本轮开始和结束时的工作树指纹相同就不运行（有未了结的退回时除外，必须重跑）；指纹和命令都与上次通过时相同则沿用上次结果并标注。指纹包含 HEAD、状态、含二进制的差异以及未跟踪文件的内容（单个超过 16 MiB 的未跟踪文件只计大小和修改时间）。非 Git 项目每次都运行。
- 复用隔离工作区已有的进程运行器（Windows Job Object、超时、取消）。
- 通过只说明这些命令覆盖到的内容。检查由操作者选择，平台不判断它们是否足够。

### D6 停滞处理

持续运行的主控给出某个下一步之后又连续 3 次重复它且没有新结果时，平台不再退避重试，而是生成一个平台提问（复用 D2 的表和界面，`origin` 为 `platform`），选项为继续和停止，也可以直接输入新的方向。操作者回答之前不再发起模型请求；协作者的来信不会唤醒它，操作者的信件和追加指令会。回答“停止”等同于停止工单。

### D7 以注意力为先的界面

- `sa-store` 按 003 Execution 的阶段表为每张工单派生阶段：**等你**（待审批、待回答、失败、受阻或检查未通过）、**进行中**、**已完成**、**已停止**，并给出一句原因。
- 会话侧栏在置顶之上显示“待你处理”分组，只在有内容时出现；正常运行的工单保持现有的安静标记。
- 对话区在输入框上方显示问题卡片：问题、选项按钮和自由回答。任务结束后显示结论卡片：状态、摘要、平台检查结果和“未核对”列表。协作页的任务行显示结论状态与“等待前置”。
- 系统通知增加“需要你回答”；工单结束的通知带上结论状态。

## 4. 不做的事及原因

| 做法 | 不做的原因 |
| --- | --- |
| 把主控限制为不读代码的路由器（原型的主会话） | 原型的路由器服务于从 GitHub 和飞书涌入的多张工单；这里主控是操作者亲自选的，要对结果负责，强制不读代码只会增加轮次 |
| 把协作者回复截断成摘要再交给主控 | 协作者的最终回复本身已是结论；截断会造成转述失真 |
| 平台级的上下文压缩或摘要模型 | 各执行端已有原生压缩；平台再压一次需要额外的模型调用 |
| 强制每次改动都由另一执行端审查 | 审查大约多花 1.3 到 2 倍用量，应由主控按风险决定；D4 让它在需要时只花一行 |
| 第一个子任务结束就唤醒主控 | 会增加主控轮次；协作者需要主控介入时已可通过总线发信唤醒 |
| 拆分主控与协作者的引导、限制委派层数 | 每个协作者任务只省约 1500 Token，却会改变现有能力；收益不抵风险 |
| 原型的 Source、Policy、Mandate、Outbox | 面向团队和外部消息来源的部署概念，不适用于单人桌面工作台 |

## 5. 账本与事件

新增表（只增表，不改已有表的列；只读窗口兼容没有这些表的旧工单库）：`job_outcomes`、`questions`、`job_deps`、`project_checks`、`check_passes`、`job_checks`、`check_bounces`。

新增事件：`report`、`ask`、`answer`、`ask.withdrawn`、`continued`、`skipped`、`check.started`、`check.result`、`check.bounced`、`auto.stalled`。

上下文新增字段：`reports[].outcome`、`reports[].checks`、`prerequisites`、`open_question`、`checks`、`completion_checks`。配置了完成检查的项目多一段固定引导（`CHECKS`）；`BASE` 增加了四段说明，每个任务每轮约多 500 Token 的固定提示。

## 6. 验证边界

已验证的是协议和状态契约：假执行端驱动的调度器测试、真实临时 Git 仓库和真实子进程的完成检查测试、无障碍树驱动的界面测试，以及截图。

以下为最初实现交付时的未验证项；后续实测更新见[真实验收](collaboration-acceptance.md)：

- 真实模型是否会按引导写出 `SA_REPORT`、`SA_ASK`、`continue` 和 `after`，以及写得是否恰当。不写时行为与改动前相同。
- 真实执行端下续派是否如预期复用原生会话（离线测试只断言平台传回了原会话标识）。
- 主控轮次、用量和产出质量的实际变化。新增的固定提示是确定的成本，节省要靠主控真的使用依赖派工和续派才会出现。
- Windows 系统通知“需要你的回答”的实际弹出，以及用户验收。

## 7. 后续开发

自动合入队列、默认完成检查建议和固定引导收紧已进入本轮实现；操作规则和最终验证见 [后续开发记录](integration-queue.md)。队列的自动部分止于候选验证及失败退回，实际更新原项目仍需操作者确认。限制仍包括只适用于 Git、隔离工作区需要各自构建、Git 只能发现文本冲突，以及耦合改动的并行冲突处理成本。
