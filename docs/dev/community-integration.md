# 社区机制吸纳（2026-11-09）

本轮将调研中适配本地 Rust 工作台的机制接入现有调度、账本和权限体系。设计借鉴不等于移植整套上游项目；效果需要以当前实现的测试和运行回执判断。

## 已接入与使用入口

| 能力 | 当前实现和入口 | 边界 |
| --- | --- | --- |
| DeepSeek Harness 更新 | 独立安装 `0.2.2-rc.1`，验证标记通过后新会话默认新版，历史会话绑定原运行时 | 新版需要 Node ≥24.9；保留旧包和旧状态，见 [DSH](../deepseek.md) |
| 隔离写入与本地合入 | **项目面板 → 协作 → 隔离工作区与合入**。开启后，后续写入子任务使用独立 Git worktree；提交并验证，再构造合并候选重新验证，最后 fast-forward 到原分支 | 默认关闭；源目录必须是有提交的干净 Git 根目录；非 Git 项目继续原流程；不自动推送/合并 GitHub PR |
| 按目录协调写入 | 不重叠目录的写任务可并行；相同或嵌套目录仍串行；工作区维护临时阻止重叠任务进入 | 这是调度冲突管理，不是系统沙箱；full/auto 执行器仍有原有系统权限 |
| PR / CI 反馈 | **协作 → PR 与 CI 反馈**。绑定任务与 GitHub PR，点击读取，保存 head、检查结果和最新评审摘要 | 当前按需刷新，仅 GitHub.com，依赖现有 `gh` 登录；不抓 inline threads，不做后台轮询、自动发布或自动恢复停止工单 |
| 语义循环检测 | Codex、Claude、DSH、ACP 的完整工具调用/结果对生成有界指纹，重复模式告警；明确失败的持续循环停止并保留记录 | 成功或未知结果重复不能证明无进展，只告警；等待/轮询清除重复序列；Antigravity 尚无对应原生证据覆盖 |
| MCP 连接复用 | 同一工单、配置、工作目录、凭据身份内复用受控连接；修改/禁用配置撤销旧连接，空闲回收 | 工单间隔离；有不确定副作用的失败不盲目重放；见 [连接池](mcp-pooling.md) |
| ACP 通用执行端 | **模型与执行端**增加 ACP；使用官方 Rust SDK 5.0.0，接 initialize/session/prompt/update/cancel/permission | 需要自行配置服务，限制权限模式要求已验证的执行端沙箱；不代表任意第三方 agent 已实测，见 [ACP](acp.md) |
| 会话检查点分支 | **协作 → 从历史检查点分支**，选历史主控终稿，填写新目标并确认原主控/权限 | 新会话，不重放旧工具、不回滚外部状态、不复制 native session；扩展需重新选；超大上下文保留原文引用 |
| 单文件检查点恢复 | **变更**中预览已保存检查点、确认恢复所选文件，恢复前保留当前前像和回执 | 不 reset 整仓库，不动 index；编辑冲突、链接/重解析点拒绝；见 [恢复说明](checkpoint-restore.md) |
| 任务相关项目知识 | 输入当前目标、派工和最近要求，按路径/符号/词/中文片段检索排序，按预算注入并保留来源 | 本地可解释词法检索，不宣称向量语义检索；创建时间不冒充验证时间 |
| 本地评测 | 版本化数据集、独立产物断言、任务/轮次/恢复/用量摘要及两次报告比较 | 通过 CLI 使用；未知 Token/金额和质量保持未知；不上传外部平台，见 [评测](evaluation.md) |

## 工作区操作约定

验证命令以 JSON 参数数组输入，例如 `["git","diff","--check"]`。实际项目应使用能检查功能的测试命令；该示例只检查 diff 格式，不证明功能正确。平台不用 shell 拼接参数。如需 PowerShell，由操作者明确提供 `powershell.exe` 和独立参数。

验证会在任务分支保存未提交代码并记录精确 HEAD。命令失败、命令改动代码、后续工作区/原分支身份变化，都使合入门槛失效。合并冲突和合并后测试失败保留候选供检查。原目录的新修改、分支切换和会被覆盖的 ignored 文件会阻止合入，不做强制清理。测试生成的 ignored 产物可留在候选内；候选不能安全删除时保留。

长验证在后台维护队列执行，不阻塞其他工单的停止/审批；退出会停止模型调度并等待已启动维护写完回执，期间保持账本独占锁。单个验证命令最长 600 秒，Git 命令最长 60 秒。当前没有单独的“终止本次合入验证”按钮。任务的停止按钮管理模型执行，不撤销已开始的操作员维护。

Windows 验证进程在恢复运行前加入独立 Job Object，父进程正常退出也会清理并确认全部子进程结束，防止后台任务在通过后继续改文件；Git 的自动后台维护关闭。非 Windows 进程组仅尽力回收，本轮未实测。

## 来源与复用方式

- [DeepSeek Super-agent prototype](https://github.com/deepseek-ai/dsh-superagent-prototype)：原本地统筹与消息模型。调研时最新提交 `8c32a9dc` 和原基线 `6337fc82` 的 Git tree 相同，没有需要搬运的代码变化。
- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.2.2-rc.1)：真实升级的是独立执行运行时。
- [Agent Orchestrator](https://github.com/OrchestratorInc/agent-orchestrator)：独立工作区、SCM 反馈的设计参考。
- [OpenHands](https://github.com/All-Hands-AI/OpenHands)：工具结果重复模式的设计参考；本地实现保守地区分失败与成功。
- [OpenCode](https://github.com/anomalyco/opencode)：常驻 MCP 连接与权限生命周期的设计参考。
- [ACP Rust SDK](https://github.com/agentclientprotocol/rust-sdk)：直接使用 Apache-2.0 的官方 SDK。
- [LangGraph persistence](https://docs.langchain.com/oss/python/langgraph/persistence)：检查点与分支的设计参考，本实现保存本地账本证据，未引入图运行时。
- [Langfuse evaluation](https://langfuse.com/docs/evaluation/overview)：数据集、运行报告与评测区分的设计参考，本实现完全本地。

除明确列出的依赖外，以上设计参考未作为新库引入，也未整段搬入上游代码。具体依赖和许可见 [第三方说明](../licenses.md)。

## 验证边界

工作区测试使用真实临时 Git 仓库、冲突及失败命令；桌面测试使用 egui_kittest 驱动控件；MCP 和 ACP 协议测试启动真实进程夹具。DSH 新旧运行时另有真实模型读写、项目外拒绝和会话恢复回执。离线评测使用真实临时文件与合成账本，不证明模型质量已经提升。全量验收及本地证据入口见 [验证记录](../verification.md)。
