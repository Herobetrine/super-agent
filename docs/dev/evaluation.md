# 本地评测与脱敏执行摘要

评测只读已有工单库，不启动调度器、不调用模型、不上传 Langfuse 等外部服务。报告含工单/任务/轮次身份、终态、恢复次数、失败数、可配对的执行时间及现有用量投影。Token 缺失或不完整保持未知/partial；没有可信账单价格，金额始终为 `null`，不会用零代替未知费用。

```powershell
scripts\cargo.cmd run -p sa-store --example evaluate -- --data C:\path\to\control-room --ticket 42 --output report-a.json
scripts\cargo.cmd run -p sa-store --example evaluate -- --data C:\path\to\control-room --ticket 43 --dataset dataset.json --case empty-file --output report-b.json
scripts\cargo.cmd run -p sa-store --example evaluate -- --left report-a.json --right report-b.json --output comparison.json
```

输出必须是新文件。比较的两个报告必须同为无数据集，或使用完全相同的数据集标识、版本、内容摘要和 case；上例有评分与无评分的报告不能直接比较，应先以同一数据集分别评测两个工单。差值方向为右侧减左侧；不完整 Token 不生成差值。

一个可直接保存为 `dataset.json` 的任务定义：

```json
{
  "schema_version": 1,
  "id": "local-regression",
  "version": "1",
  "cases": [{
    "id": "empty-file",
    "task": "创建空文件 result.txt 并完成工单。",
    "assertions": [
      {"kind": "root_state", "equals": "done"},
      {"kind": "workspace_file_sha256", "path": "result.txt", "equals": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},
      {"kind": "max_failed_jobs", "maximum": 0}
    ]
  }]
}
```

还支持 `result_sha256`（匹配账本中完整原生终稿字符串的 UTF-8 摘要）和 `workspace_file_absent`（预期文件不存在）。每条断言报告 pass/fail/unknown 和证据类别；所有证据可判定时分数为通过数/总数，出现 unknown 时总分为 null。仅状态 done 不代表实现正确，功能质量应使用独立预期的文件摘要或终稿摘要，不能用 agent 自称“测试通过”代替外部断言。

文件断言检查**评测时当前工作区**，不是历史工单的文件快照。应在每次运行结束且产物仍有效时评测并保存报告，再执行下一次运行；不能在后续修改后声称重建了早先工单的产物质量。默认拒绝链接/重解析点、项目外路径、非普通文件、读取期间改变和超过 64 MiB 的文件；无法核实时记 unknown，不算失败通过或不存在。当前只汇总 journal 已记录的恢复事件，未暴露的执行器内部重试无法推算。

默认导出不包含提示词、终稿、工作区路径、文件名、工具参数、供应商端点、原生会话标识或密钥。自定义 agent 名称归为 other；数据集 id/version/case 是运营方指定的公开标识，仍应避免填写秘密。报告保留数值工单/任务 ID，适合本地对比；不声称这些元数据已达到匿名化标准。

自动测试：`scripts\cargo.cmd test -p sa-store --test evaluation`。覆盖真实文件内容改变导致分数下降、终稿摘要、未知费用/Token、复用 native 去重、逐轮恢复统计、版本/断言变化拒绝比较、目录未知、读库无写入、导出不覆盖及脱敏。此工具评测已有结果；没有执行新任务集，也没有获得真实模型质量提升或人类验收结论。

2026-11-09 专项执行：5 项测试通过，`clippy -p sa-store --all-targets` 无警告。另在临时 SQLite 工单库实际运行 evaluate CLI 两次并生成比较文件：将空产物改为错误内容后评分从 1 降为 2/3；Token 差值为 0，金额差值为 null。此为合成账本与真实临时文件的端到端演练。
