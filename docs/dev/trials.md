# 固定任务集与受控试验

本文对应[长期设计](../architecture-evolution.md) §13 和 P5。目标是在相同任务、相同验收标准下比较执行策略，给出六项目标各自的结果和未达项。实现见 [trials.rs](../../crates/sa-engine/src/trials.rs)，命令行见 [trial.rs](../../crates/sa-adapters/examples/trial.rs)。

## 任务集 core-v1

[eval/suites/core-v1/suite.json](../../eval/suites/core-v1/suite.json) 是带版本的任务集（`superagent-core` 1.1.0；1.1.0 把 recovery-restart 的重启从第 45 秒提前到第 20 秒，确保重启落在主控执行中）。六个任务对应 §13.2 的六类风险：

| 任务 | 类别 | 主要风险 |
| --- | --- | --- |
| small-bugfix | small_change | 过度拆分、验证比实现贵 |
| cross-module | cross_module | 数据层与报告接口不一致、遗漏要求 |
| parallel-modules | parallel | 两个独立模块再集成：冲突、等待、重复构建 |
| requirement-change | requirement_change | 执行中修订需求，旧证据不能满足新合同 |
| recovery-restart | long_recovery | 运行中重启调度器，不重复副作用、不丢状态 |
| usage-doc | docs_delivery | 文档必须可用：示例命令的实际输出要一致 |

每个任务目录包含三部分：

- `project/`：初始项目，包括操作员可见的需求检查文件。
- `acceptance/`：隐藏的独立验收，运行时不会复制进项目。
- `solution/`：参考解，只用于校验任务集本身。

检查命令都是 `python -B …`。需求检查与项目检查合计不超过 4 条，且都由试验执行器作为操作员采用的标准写入。

## 先校验任务集（不调用模型）

```powershell
scripts\cargo.cmd run -p sa-adapters --example trial -- --suite eval/suites/core-v1/suite.json --validate --output new-validation.json
```

每个任务必须同时满足两点：

- 初始项目至少有一项隐藏验收失败，任务才有区分度。
- 参考解通过全部隐藏验收、需求检查和脚本中的修订检查。

2026-11-18 校验结果：六个任务全部有效。

## 运行试验（计费）

```powershell
scripts\cargo.cmd run -p sa-adapters --example trial -- --suite eval/suites/core-v1/suite.json --run --variants eval/suites/core-v1/variants.example.json --repeat 3 --out <新目录> --billable
```

不加 `--billable` 会拒绝运行。`--agents claude` 只加载列出的执行端，其余执行端对主控和派工都不可用，用于把整次试验限定在指定模型上（配合变体里的 `lead` 和 `selections` 固定路由）。执行端集合写在输出目录的 `harness.json` 中，同一目录续跑时必须一致。每次运行使用新的临时项目和工单库，不读写真实工单库。过程如下：

1. 复制项目并建立初始提交，配置项目检查。
2. 写入需求，采用行为检查。按任务设置开启交付门、隔离写入或合入队列。
3. 启动后按脚本执行操作员事件：
   - `follow`：追加要求。
   - `revise`：修订需求并采用新检查。
   - `restart`：关闭并重开调度器。
4. 运行中，执行器代替操作员处理待办：问题按默认答案或第一个选项回答，审批全部允许，合入候选全部确认。重启打断的任务会进入中断状态，平台不会自行重跑；执行器像操作员点“重试”一样重试它，每个任务最多 3 次。这些都计为人工交互。
5. 根任务进入终态、没有排队或运行中的任务、脚本执行完毕后，结束本次运行；超过时间上限会停止工单。
6. 在项目副本上叠加隐藏验收并运行。

每次运行的记录（`runs/<任务>-<变体>-<轮次>.json`）包含：

- 任务集身份（id、版本、文件摘要）和变体设置。
- 根任务终态、是否超时、端到端耗时。
- 隐藏验收逐项结果。
- 交付门状态。
- 用量账目摘要（总量、覆盖、分组、缺口数）。
- 人工交互：问题、审批、合入确认、中断重试、脚本追加、脚本修订、重启。汇总里的人工交互数只计问题、审批、合入确认和中断重试。
- 返工：检查退回、合入退回、平台重试。

判定规则：

- `success`：隐藏验收全部通过，并且开启交付门时其状态为 ready。
- `unknown`：超时或检查无法执行，不计为成功。
- 其余为 `failure`。

## 变体与对照

变体文件是数组，每项只写要改变的变量：

- `lead`：主控执行端。
- `selections`：路由。
- `efficiency`：效率设置，例如 `max_collaborators`、`incremental_context`、`quota_guard`。
- `isolated_writers`：是否隔离写入。
- `policy`：执行策略模式（`record` 或 `apply`），用于 §8.3 的匹配对照：同一任务在只记录与按规则调整两种模式下比较。

执行顺序是每轮依次运行所有任务和所有变体，让时间漂移对各变体的影响相同。已存在的记录会跳过，中断后可以续跑。

汇总：

```powershell
scripts\cargo.cmd run -p sa-adapters --example trial -- --suite eval/suites/core-v1/suite.json --aggregate <目录> --output new-report.json
```

报告按任务和变体分组，给出：

- 运行数、成功、失败、未知和成功率。未知计入分母。
- 耗时 P50 / P90。
- 已报告用量总和，包括失败的运行。
- 单位成功支出：只有用量覆盖完整且成功数大于零时才计算，否则为 null。
- 人工交互数和返工数。

每个任务的第一个变体作为基线，其余变体给出差值。来自不同版本任务集的记录不能合并。

## 限制

- 隐藏验收只能证明任务集定义的行为，不代表任意项目的质量。
- 每组 3 次只用于探索，不构成统计显著。
- Claude 只统计主循环用量，多数运行的用量覆盖不完整，单位成功支出通常为 null。这时只能比较已报告部分，并注明覆盖不足。
- 执行器使用内置提供方配置，不读取桌面端的自定义供应商。需要指定路由时，写在变体的 `selections` 里。
- 人工体验、界面舒适度和真实通知点击不在本执行器范围内，需要单独人工验收。
