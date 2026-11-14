# Codex

安装并登录本机 Codex CLI，先在终端检查 `codex --version`，再打开 **模型与执行端**。Windows 适配器使用原生可执行文件，通过 stdio 与 app-server 通信。[安装 CLI](https://developers.openai.com/codex/cli/) · [app-server 协议](https://developers.openai.com/codex/app-server)

## 供应商与会话

适配器为独立 CLI 的目录探测和执行轮次移除继承的 `CODEX_WINDOWS_REGISTERED_CORE` 环境变量。由 Codex 桌面启动工作台时，该内部变量可能要求独立 CLI 使用桌面注册的启动身份，导致只读命令报 `registered Core launch requires an installed app`。修正仅作用于 Codex 的子进程环境副本，其余环境变量、会话权限与审批策略沿用原配置。2026-11-13 已在本机 Codex CLI 0.166.0 离线复现该差异。

同次验证对 npm 与 WindowsApps resources 中的原生 CLI 分别检查：继承该标志时重现错误，移除后读取成功，覆盖与新建文件均被拒绝，原文件保持不变。测试使用本机 `sandbox --include-managed-config --permission-profile :read-only`，未调用模型。全量 Rust 回归为 409 passed / 0 failed / 3 ignored，workspace build 与 Clippy `-D warnings` 通过；这不代替修复版安装后的实际工单审计结果。

随后在正常工单的安全交接点安装修复版，恢复后重新派发的三个审计任务均保持 `readOnly` / `networkAccess=false`，并实际成功读取本地文件，未再出现上述启动错误。这验证了生产启动路径恢复；各业务审计的完整性和验收仍单独判断。

模型选项按当前来源读取。显式 `base_url` 使用该来源的配对认证在线获取模型并处理分页；配置缺少端点时由 Codex CLI 解析登录来源和原生目录。显式 `model_catalog_json` 继续使用指定文件。目录保留模型声明的 `supported_reasoning_levels` 并传到界面的思考强度选择，执行时通过 `turn/start.effort` 交给 app-server。目录失败时标注当前列表的范围。正在执行的任务沿用启动时的选择。

输入发送到指定的原生 thread 和 turn。恢复会话需收到匹配的线程标识；派工期间可给活动任务投递信件，平台以原生 steering 回执记录轮内接受。连接中断后未确认的消息进入后续轮次。即使已经收到协议回执，模型是否实际采用消息仍应检查其输出。

## 权限档位

`ask` 以只读沙箱启动，必要的执行请求交给用户审批。`partial` 开放工作区写入，超出该范围的请求可通过原生审批流程处理。`full` 和 `auto` 使用 `danger-full-access` 并关闭逐次审批，实际权限可超出当前项目。控制台会检查启动时返回的策略与原生终态。[Codex app-server](https://developers.openai.com/codex/app-server)

## 复验

在临时项目中创建一个只有你知道内容的文件，只在提示里提供文件名并要求读回内容。回复后追加一条不允许再读文件的指令，要求复述上轮内容。分别比较文件、原生终稿和会话 ID。修改本机 provider、CLI 版本或沙箱配置后可重复这一检查。公开的测试入口见[验证方法](verification.md)。
