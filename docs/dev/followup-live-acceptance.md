# 后续改动的真实协作验收

## 范围

2026-11-17 准备两个最小代表场景，验证[固定引导收紧](guide-tuning.md)后的续派消息 ID 依赖，以及新候选验证队列。使用 agy 1.5.9，来源与模型固定为 `google / gemini-4-hydrogen-high`，主控和协作者沿用同一路由。

项目与工单库均为新建临时目录。场景生成器拒绝已有运行目录，验收驱动拒绝已有工单库。`queue` 场景的 `confirm_integration=true` 明确允许测试驱动在该临时项目中扮演操作者确认候选。

## 场景与通过条件

| 场景 | 必须核验的事实 | 当前状态 |
| --- | --- | --- |
| deps-alias | 初次实现和审查完成，原实现者真实续派且原生会话保持，新只读审查任务使用 `after:["resume-impl"]` 并等续做结果，独立检查零和负数边界 | 本次样本通过 |
| queue | 一个真实隔离 writer，队列实际进入 `awaiting_confirmation`，确认前原项目 HEAD 和干净状态保持，驱动使用本次验证的 confirmation_token 确认后队列 merged，原项目产物及独立测试通过 | 本次样本通过 |

准备时两组初始项目均通过既有 2 项 Python 单元测试，Git 干净，工单库尚未创建。重复准备同目录会被拒绝。

驱动新增可选 `isolated_writers`、`integration_queue`、`confirm_integration`。队列场景不能仅靠根任务 done 通过，必须观察到待确认候选并记录确认前的原项目快照，随后检查实际队列状态。`required_job_keys` 与 `required_continuation_keys` 分别要求指定直接子任务完成及唯一真实续派回执，续派目标必须至少运行至第 2 轮。

## 续派消息 ID 样本结果

- 真实调用使用编入本轮固定引导的验收程序。驱动的 3 项无模型回归通过，真实场景进程退出码为 0。
- 根任务 job 1 运行 4 轮，原实现者 job 2 运行 2 轮，初审 job 3 与新复审 job 4 各运行 1 轮，全部 done。两张审查任务均为只读。
- 账本 `continued` 序号 975 的目标为 job 2、消息 ID 为 `resume-impl`。原实现者两轮的原生结果返回同一 conversation ID，未新建实现者原生会话。
- 主控第 3 轮终稿控制原文包含 `review-followup` 的 `after:["resume-impl"]`。真实 `job_deps` 为 4→2。实现者第 2 轮 reply 序号 1221，复审实际 input 序号 1226，证明复审在续做结果后启动。
- 4 张任务的原生路由均为指定的 Google Hydrogen High，`protocol.rejected` 为 0。
- 独立运行最终项目的 11 项单元测试通过。另以独立断言覆盖三类输入在 limit=0 时为空、带自定义 suffix 仍为空、limit=-1 均抛 ValueError，以及正数截断与无需截断两种行为，共 11 个断言通过。

本地运行目录中的 `independent-verification.json` 保存任务、会话、事件序号与路由明细，`summary.json` 为只读账本分析结果。此样本补上此前续派别名解析修复的真实调用验证。

## 候选验证队列样本结果

- 使用已包含确认令牌机制的编译程序独立副本 `tmpstate/followup/live/queue-runner.exe`，副本摘要与构建副本收据一致，运行时未占用 target 中的测试程序。驱动的 3 项无模型回归通过，真实场景进程退出码为 0。之后修复的调度器数据库占用处理由全量回归验证，未追加计费场景。
- 根任务 job 1 运行 2 轮，唯一写入子任务 `queue-impl` 为 job 2，运行 1 轮，两者均 done。写入任务确实使用临时库下 `workspaces/job-2` 的隔离目录。
- `integration.result` 序号 372 将任务置为 `awaiting_confirmation`，隔离目录和合并候选检查均通过。独立读取此时原项目，确认 HEAD 等于验证记录的 baseline、Git 干净且 `textkit/reverse.py` 尚不存在，隔离工作树已有该文件。
- 驱动在根任务汇总结束后再次记录原项目快照，确认与初始快照一致，随后调用 `confirm_integration(job, confirmation_token)`。`integration.confirmed` 序号 693 为成功回执，最终队列与工作区均为 merged，原项目 HEAD 精确等于已验证的 candidate_head，Git 保持干净。
- 独立运行合入原项目的 6 项单元测试通过。另以 6 个断言覆盖空字符串、纯空白、单词、多个单词、混合空白和中文单词逆序，通过。
- 所有原生路由均为指定的 Google Hydrogen High，`protocol.rejected` 为 0。队列修复次数 attempts=0，本真实样本覆盖首次通过路径。队列失败退回、冲突和取消等分支由对应离线回归另行记录。

确认前独立记录为 `independent-before-confirmation.json`，驱动前后观察为 `ledger/acceptance-queue.json`，最终独立核验为 `independent-verification.json`。合入确认由场景驱动调用 API 完成，人工界面确认仍待单独验收。

## 复现

```powershell
python scripts/prepare-collab-acceptance.py deps-alias <new-run-dir> --agent antigravity --model gemini-4-hydrogen-high --provider google --timeout 1200
python scripts/prepare-collab-acceptance.py queue <another-new-run-dir> --agent antigravity --model gemini-4-hydrogen-high --provider google --timeout 1200
scripts\cargo.cmd test -p sa-adapters --test collab_live --no-run
```

仅在运行对应验收进程时设置 `SA_LIVE_COLLAB` 为该场景文件。编译完成后直接调用同一份 `collab_live` 测试程序并传入 `live_collaboration_scenario --exact --nocapture`，保留退出码与日志。运行中保持该程序文件不被重新链接。完成后以 `scripts/collab-acceptance-summary.py` 只读分析账本，并独立检查原生会话、控制回执、依赖顺序与产物。

本轮准备目录位于 `tmpstate/followup/live/deps-alias-20261117-133438/` 与 `tmpstate/followup/live/queue-20261117-133438/`。队列驱动将确认前后观察写入临时库目录的 `acceptance-queue.json`。原始账本和模型文本保留在本机临时证据中。

## 证据边界

这是指定控制形式的定向场景，只覆盖所列真实调用。不会由这两个样本推算模型普遍服从率、Token 或账单变化、节费比例或产出质量提升。人工界面确认和系统通知横幅继续使用各自的验收边界。

本轮恰好运行上述两组计费场景，均通过且没有追加计费复测。构建与工作区全量检查由主代理统筹，此处的真实场景结果与其分别记录。
