# 主控 Token 与缓存优化

## 问题与目标

2026-11-10 的本机只读抽样显示，历史 Claude 主控 106 次平台 JSON 输入共约 29.54 MB（UTF-8），最大单次约 445 KB。最近 30 次主控输入的扩展目录与技能正文约 9.36 MB，是最大的重复字段。这是平台注入字节量，不是供应商 Token 数。

最近 20,000 条 Claude 主控原生事件中，36 个具有完整终态 usage 的轮次合计缓存读取 18,273,329、缓存写入 13,218,949、普通输入 354 Token，读取比例约 58.0%。这是终态样本，排除内部子任务，可能漏掉失败或无终态请求，不代表完整账单。缓存写入不能计作命中。

目标优先级是减少重复输入、减少无效模型轮次、保留正确恢复能力，再提高实际缓存命中。99% 不能作为所有任务的保证：若缓存读取为 C、新增与重写输入为 U，需要 C ≥ 99U 才能达到 99%。冷启动、压缩、切换模型、缓存驱逐均影响该比例；人为膨胀 C 或付费保温会增加消耗。

## 默认行为

- 主控等待员工时结束本轮，由调度器等待终态或实质信件。没有新增模型心跳；协议明确禁止仅为缓存保温而 sleep、轮询或互发确认。
- 已确认成功、原生 session 与执行路由一致的 `full` / `auto` 任务，续轮只传变化的上下文。不变的工具目录、技能正文与历史不再每轮追加。
- 新报告和变更报告保留全文。数组明确标记追加或按身份更新，删除字段明确标记。权限、身份及小型任务目标仍随轮提供。
- 完整上下文保存在 `context.snapshot` 事件和不可变 `full_context` 文件；模型上下文已压缩时可按需读取。受限权限仍完整投递，避免让恢复依赖项目外不可读文件。
- 成功终稿与 `context.delivered` 回执在同一事务提交。失败请求不推进确认基线；新 session、路由不符、损坏快照或不完整压缩交付均保守完整重放。原始全量报告继续用于完成判断。
- 已识别的 Claude / Codex 原生压缩信号使下轮完整重放，即使 session 不变。其他执行端未知的压缩事件格式仍依赖 `full_context` 恢复入口，不能据此承诺所有模型都自动识别了上下文丢失。
- `running` 事件中的 `full_prompt_bytes`、`sent_prompt_bytes` 和 `saved_bytes` 衡量平台输入减少量；这些字节指标不替代原生 usage 或供应商账单。

已有会话升级后第一次输入仍完整交付，建立新的成功回执后才使用增量。

## 桌面控件

点击侧栏底部 **Token 与缓存**（左侧齿轮也可打开）：

- **增量上下文**：默认开启，关闭后后续轮次完整投递；受限权限仍保持原有完整投递方式。
- **精简协作提示**：默认开启，要求简短交接、只传相关证据，将长日志留在文件中；关闭后按任务需要决定说明详细程度。
- **Claude 缓存时长**：选择 **沿用执行端 / 5 分钟 / 1 小时**，同时作为主会话与内部子请求的默认时长。已有 CLI 显式时长、强制或禁用缓存设置仍优先。

点击 **保存** 后作用于本工作台所有工单的后续轮次，当前执行不被打断，重启后保留。**取消** 丢弃尚未保存的修改；**恢复默认** 只重置面板草稿，仍需保存。保存失败保留草稿以便重试。只读观察窗口显示已保存值但不能修改；演示模式可试选而不写入真实设置。

这些控件不提供未经执行端支持的缓存率或 Token 硬上限，也不改变模型、供应商或权限选择。

## Claude 长等待缓存

Claude Code 2.1.242+ 官方提供主会话与内部子请求的独立 TTL 设置。本机检查版本为 2.1.317。桌面时长选择仅修改对应子进程的缓存偏好，不修改用户凭据、端点或全局 Claude 配置。也保留 `SUPERAGENT_CLAUDE_CACHE_TTL=1h|5m` 启动参数；面板选择“沿用执行端”时保留该环境变量，选择具体时长时以面板值作为平台偏好。已有 CLI 缓存开关及 TTL 设置仍优先。

在已确认网关支持 1 小时 TTL、会透传 `anthropic-beta` 时，可这样启动一次：

```powershell
$env:SUPERAGENT_CLAUDE_CACHE_TTL='1h'
Start-Process '.\target\release\super-agent.exe'
Remove-Item Env:SUPERAGENT_CLAUDE_CACHE_TTL
```

应先正常退出旧工作台再启动，避免另开只读观察窗口。1 小时缓存写入成本高于 5 分钟，是否节省取决于重用间隔与供应商计费。设置到达子进程不等于服务器实际命中。本次没有调用 Claude（包括 fable）测量命中率，也没有自动改变现有账户的 TTL 策略。

设置读取覆盖本地用户、所选来源、项目及单个 Windows `managed-settings.json` 中的缓存字段。企业 Registry、drop-ins、远端与 `policyHelper` 策略未完整复刻；这些环境应优先直接由 CLI 管理 TTL。Haiku 主会话 TTL 覆盖要求 2.1.283+，平台通用版本门槛仍为 2.1.242，不能仅凭通过该门槛推断每种模型都支持覆盖。

Codex app-server 当前没有核验到公开可用的缓存 TTL 配置，不把 Responses API 的参数直接注入 Codex。DeepSeek 缓存由服务端尽力保留，没有采用未经支持的 TTL 参数。

## 验证与复测

```powershell
scripts\cargo.cmd test -p sa-engine --test context_delivery --test idle_budget
scripts\cargo.cmd test -p sa-adapters cache_policy
scripts\cargo.cmd build --workspace
scripts\cargo.cmd test --workspace
scripts\cargo.cmd clippy --workspace --all-targets -- -D warnings
python scripts/audit-context-cost.py "$env:USERPROFILE\.local\share\superagent\control-room\hub.sqlite3"
```

审计脚本以 SQLite `mode=ro` 打开，输出字段大小和用量汇总，不输出提示正文、密钥或请求模型。离线验证使用临时数据库和假执行端；默认不启用任何计费测试开关。实际长期缓存率、网关 TTL 执行和任务质量仍需在正常工作负载中观察。不要把合成字节降幅称作实际账单降幅。

真实 Engine 配合固定大技能目录的离线回放中，重启后续轮输入从 488,568 降到 2,073 字节，减少约 99.58%；这是可重复的合成场景字节指标。另一个等待回归模拟 256 次调度检查，主控维持 1 次调用，员工完成后才增至 2 次。失败重试、并发报告、授权撤回、session/路由变化、损坏快照、同 session 压缩和历史分支均有离线回归。

最终验证（2026-11-10）：workspace build/test、Clippy `-D warnings`、release build 均退出 0。测试器报告 363 passed / 0 failed / 3 ignored，其中环境门控的提前返回不算真实模型验证。增量专项 8 项、等待专项 1 项、TTL 专项 8 项通过。debug/release 演示窗口均正常启动、保存截图并退出，截图已查看；未调度真实工单。桌面快捷方式仍指向重建后的 `target/release/super-agent.exe`。本次没有模型推理调用，线上费用效果和用户验收尚未完成。

本机完整回执保存在用户数据目录的 `checks/token-efficiency-20261110/`，同目录保留测试日志与截图；这些私有运行材料不提交仓库。

桌面控件接续验证（2026-11-10）：4 项真实 App 点击测试覆盖取消、保存、重启回读、恢复默认、失败重试、只读窗口与演示预览；Host 验证数据库保存失败不发布运行时设置。全量测试报告 375 passed / 0 failed / 3 ignored，环境门控提前返回仍不算真实模型调用；workspace build、Clippy 与 release build 通过。新版 release 已在独立空库中打开设置面板并截图检查，空库始终 0 个任务。桌面快捷方式指向已更新的 `target/release/super-agent.exe`，没有改变真实账户的设置。控件验证回执位于用户数据目录的 `checks/token-settings-ui-20261110/`，不提交私有日志。

## 设计来源

- [Claude Code prompt caching](https://code.claude.com/docs/en/prompt-caching)：TTL、覆盖优先级、1h 成本和网关要求。
- [Anthropic 上下文工程](https://www.anthropic.com/research/effective-context-engineering-for-ai-agents)：按需检索、压缩、结构化笔记和子任务隔离。
- [LangGraph 持久化中断](https://docs.langchain.com/oss/python/langgraph/interrupts)与[子图状态](https://docs.langchain.com/oss/python/langgraph/use-subgraphs)：运行时等待、恢复与隔离子任务上下文。
- [OpenAI 缓存](https://developers.openai.com/api/docs/guides/prompt-caching)与[Codex 配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)：区分 API 能力和本地执行器可配置项。
- [DeepSeek context caching](https://api-docs.deepseek.com/guides/kv_cache/)：前缀复用、命中计数和尽力保留。

采用现有 SQLite 事件、原生会话恢复和 SHA-256 对象存储，没有新增 agent 框架依赖或复制上游实现。更换 CLI、网关或恢复机制后，应重新验证增量语义与缓存策略。
