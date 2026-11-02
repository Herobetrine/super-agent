# Super Agent

在一个 Windows 窗口里指挥 Codex、Claude Code、DeepSeek Harness 和 Antigravity。向主控提出目标，它可以自己回答，也可以分派任务、接收其他 agent 的完整回报，再继续工作。你可以在对话中补充要求、给具体任务留言或停止执行。

例如，选中项目后输入「修复导入器无法处理中文文件名的问题。先写出复现步骤，修改后运行相关测试，再请另一个 agent 审查」。打开 **协作** 可看到实际参与的 agent、当前分工和回报。

## 安装与启动

需要 Windows、Python 3.10 或更高版本，以及至少一种已安装并完成认证的 agent CLI。桌面程序使用 PyQt6，调度和消息存储在本机运行；选定的推理供应商会收到执行任务所需的上下文。

在 PowerShell 中从源码启动工作台。

```powershell
git clone https://github.com/Herobetrine/super-agent.git
cd super-agent
python -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
.\.venv\Scripts\python.exe desktop.pyw
```

要创建桌面快捷方式，在项目目录激活同一个虚拟环境后运行安装脚本。脚本会重新读取快捷方式，核对解释器和程序入口。

```powershell
.\.venv\Scripts\Activate.ps1
.\Install-Desktop.ps1
```

| Agent | 自动执行入口 | 配置和安装 |
| --- | --- | --- |
| Codex | 本机 Codex CLI 的 app-server | [适配说明](docs/codex.md) · [官方安装](https://developers.openai.com/codex/cli/) |
| Claude Code | 本机 CLI 的双向 JSON 消息流 | [供应商与认证](docs/claude-antigravity.md) · [官方安装](https://code.claude.com/docs/en/setup) |
| DeepSeek Harness | Node.js 及平台专用的上游 npm 运行时 | [版本、安装命令和配置](docs/deepseek.md) |
| Antigravity | 本机已认证的 `agy` CLI | [供应商与认证](docs/claude-antigravity.md) · [官方安装](https://antigravity.google/docs/cli/install) |

在 **模型与执行端** 中为每个 agent 选择供应商与模型。候选项来自本机配置或对应 CLI 的目录。可以跟随当前默认，也可以把当前组合固定在一张工单中。选中的供应商失效时，任务显示错误。界面只启用已打通自动投递、执行和回传的执行端。

## 开始工作

1. 点击 **新对话**，选择项目。没有项目时，平台会创建独立临时工作目录。
2. **在消息输入框正下方找到「权限」下拉框**，它位于项目和主控按钮上方。发送第一条消息前选择权限档位。点 **主控** 可更换主理 agent 或调整协作者选型授权。只安装了一种 CLI 时，请选择已准备好的那一种。
3. 输入目标并点击 **发送**，或按 `Ctrl+Enter`。同一对话里的后续消息会交给主控。
4. 展开 **协作** 查看各项任务，选中执行者可以定向留言。右上角更多菜单可以查看原生事件、导出完整记录或停止。
5. 检查项目中的实际文件和测试结果。若进程异常退出，重新打开后，可核对已经发生的文件操作再重试中断任务。

### 会话权限与自动目标

![权限下拉框位于消息输入框下方](docs/images/composer-permissions.png)

新对话默认只读。需要你决定的动作出现在窗口顶部的 **待审批** 中，可检查工单、执行者和请求内容后批准或拒绝。

新对话默认允许主控为每个子任务选模型、分配职能并收紧权限。主控会从目标 agent 当前获准的供应商与执行端中读取可选模型，按任务从中选择。要坚持使用原先配置的模型，请在发送首条消息前点 **主控**，关闭 **允许主控为协作者选择模型和子任务权限**。

展开 **协作** 并选中任务，可以核对本次使用的模型和权限。主控不能替工单切换供应商，也不能给子任务授予超出工单档位的权限。子任务继续派工时仍继承自己的权限上限。已开始的工单锁定原授权，需要不同的工单级权限时请新建对话。

| 模式 | 行为 |
| --- | --- |
| 只读 | 用于问答、审查和旧工单复验。受限执行器禁用写入。 |
| `ask` | 需要批准的执行请求进入待处理状态。Codex 可以发出原生审批，DeepSeek 的写入轮次需要先获准。 |
| `partial` | 允许受支持执行器在项目工作区内写入。Codex 需要提升权限时仍可请求审批。Claude Code 和 Antigravity 在此档维持只读。 |
| `full` | 执行器以当前系统用户的广泛权限无人值守运行，可访问项目外文件并执行命令。 |
| `auto` | 具有 `full` 的高权限，主控还可跨轮持续推进当前目标。 |

**完全访问和自动模式均为高风险授权。** Agent 可以用当前系统用户的权限读取凭据、运行命令、改变项目外的文件，或把数据发送给模型和网络服务。在本次会话明确确认后授权才生效；新对话恢复为只读。

### 持续自动运行

新建的 `auto` 工单完成一个阶段后，会总结经验并搜寻下一步。主控可根据已验证的项目状态提出具体目标、证据和行动，继续下一轮。遇到阶段性阻塞时也会考虑替代路径。主控通过 `SA_LEARN` 保存经验；通过 `SA_EXHAUSTED` 提交结论、已核查事项和替代方案，才可按持续运行协议结束工单。旧版自动工单保留原有的阶段结束规则和 64 轮限制。

窗口顶部的**自动运行**栏显示执行阶段或“正在搜寻下一步”、当前目标、最近经验以及退避等待。你可以追加指令调整工作方向。要终止后续调度，点击 **停止自动运行**，也可以给主控单独发送“停止”。已停止的持续工单在刷新或重开后保持停止状态；输入新的工作指令，点击 **恢复并发送** 并确认，才会继续。

持续工单的消息区最多保留 100 张近期卡片。重新打开很长的工单时，从最近 2,000 条原始事件中提取可见活动；协作面板展示主控、活动任务和最近 40 项任务。较早记录仍在本地账本中，可用预览提示或右上角菜单导出完整记录。

**持续费用。** 搜寻新目标、核查路径、自动重试以及重启后的恢复检查，都可能产生新的模型请求。已完成阶段和暂时没有新成果时也可能继续计费。关闭窗口会中断本机执行，重开后未停止的自动工单可能继续；需要结束运行时，请先停止工单。已产生的费用和文件变更保留。

**免责声明。** 授权前的确认框会说明数据丢失、信息外传和模型费用风险。软件按 GPLv3 以原样提供，在适用法律允许的范围内不提供担保。系统账户、供应商和外部工具的权限仍然生效。

## 消息完整性与本地数据

- SQLite 保存派工、回报、定向信件、任务状态及可导出的原生事件。完整终稿按收到的字符串写入，包括换行。界面可折叠控制消息并缩短过长的预览。
- 一个 `SA_SEND` 可附带最多 16 个项目相对路径，文件合计不超过 64 MiB。平台按 SHA-256 保存交付时的字节快照，后续项目修改不影响该份快照。
- 轮内接收取决于执行器。Codex 的活动轮次可使用原生 steering，其余信件按对应执行器的回执或后续轮次投递。模型的上下文窗口和上游事件可见性仍由相应服务决定。
- 默认工单库位于 `%USERPROFILE%\.local\share\superagent\control-room`，可用 `SUPERAGENT_DATA` 指定独立目录。关闭窗口后可备份整个目录，包括数据库和附件对象。会话、文件内容、工具输出和端点配置可能是私有数据；复现实验时可以只分享经过检查的最小样例。

## 测试与设计来源

本地回归覆盖调度、恢复、权限、消息、模型配置和 Qt 交互。真实 CLI 核验会产生模型请求，按需使用独立测试项目。[验证方法](docs/verification.md)

```powershell
.\.venv\Scripts\python.exe -X utf8 -m unittest discover -s tests -v
```

本地统筹、独立 worker 和持久化消息参考了 DeepSeek 的 Super-agent 原型。权限设计也参考了 OpenHands 的确认策略及 OpenCode 的请求、允许、拒绝规则。[DeepSeek 原型](https://github.com/deepseek-ai/dsh-superagent-prototype/tree/6337fc823ce20251c8de8085948165c81aaa701e) · [OpenHands 确认策略](https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-sdk/openhands/sdk/security/confirmation_policy.py) · [OpenCode 权限规则](https://opencode.ai/docs/permissions/)

## 许可证

本项目按 **GPL-3.0-only** 发布。Python 依赖和单独安装的商业或开源 CLI 适用各自条款。[许可证全文](LICENSE) · [依赖与第三方说明](docs/licenses.md)
