# ACP v1 执行端

工作台通过官方 Rust SDK `agent-client-protocol = 5.0.0` 连接本机 stdio ACP 服务。原有四种原生执行端保持独立；ACP 显示为第五种执行端，每个配置的服务显示为一个供应商。

在 `%USERPROFILE%\.local\share\superagent\acp.json` 写入配置，或用 `SUPERAGENT_ACP_CONFIG` 指定其他文件。保存后刷新模型目录；配置在每次执行前重读，无需重新编译。程序必须已安装并已完成自身认证。命令直接启动，不经过 shell；Windows `.cmd` 包装器应改成实际 `node.exe` 与脚本入口。

```json
{
  "servers": [
    {
      "id": "my-agent",
      "label": "我的 ACP 服务",
      "command": "C:\\Program Files\\nodejs\\node.exe",
      "args": ["C:\\tools\\my-agent\\cli.js", "--acp"],
      "env": ["MY_AGENT_API_KEY"],
      "models": ["default"]
    }
  ]
}
```

上例路径和参数需按实际 agent 的官方安装说明填写。`env` 只填写本机已存在的变量名称，不写密钥值。默认只传必要的系统路径、用户目录和临时目录变量；额外变量须显式列出。工作目录始终来自工单项目，不接受服务配置覆盖。配置错误、重复 ID、找不到程序会使该服务不可执行。

若服务在 `session/new` 或 `session/load` 返回 `configOptions` 中声明模型选项，可设置 `"model_config_id":"model"` 和真实 `models` 值。执行前逐项检查该选项及目标值，调用 `session/set_config_option`；服务未声明目标值时拒绝。未配置此字段时只允许 `default`，即 agent 自身已配置的默认模型；不会把任意模型名假装发送成功。

## 权限

ACP 协议本身不提供文件系统隔离。默认服务仅接受 `full` / `auto` 工单，并按已授权范围选择 `allow_once`，不会选择持久授权 `allow_always`。需要仅一次拒绝时选 `reject_once`；缺少所需选项则取消。

只有操作人已实际核查该服务的沙箱后，才应配置 `trusted_sandbox: true` 与 `permission_modes`，例如 `{"readonly":"read-only","ask":"workspace-write","partial":"workspace-write"}`。这些值必须与运行时返回的 `availableModes` 精确匹配；设置 mode 失败即停止。该声明表示对特定服务的信任，不是 ACP 通用能力证明。`readonly` 始终拒绝额外工具授权；`ask` / `partial` 中的权限请求交由工作台审批。关闭、取消或无匹配会话的审批均拒绝。

客户端不声明文件读写或终端代理能力，不向任意服务提供宿主工具。无沙箱声明时，选择受限工单会在启动前失败，不会静默提升权限。

## 协议与恢复

连接顺序为 `initialize(v1)` → `session/new` 或 `session/load` → 可选 mode/model → `session/prompt`。只有服务声明 `loadSession` 才能续接，失败不会偷偷新建会话。恢复回放不计入本轮终稿。`session/update` 的正文与工具生命周期写入现有事件通道；只有本轮 `end_turn` 视为成功，其他 stop reason 明确失败。图片输入暂未开放；不把图片转换成文本后声称已经支持视觉。

取消发送原生 `session/cancel`，最多等待短暂终态，然后通过现有进程封装结束整棵进程树。程序以隐藏窗口启动，stderr 不进入账本；RPC 错误仅保留错误码，不公开远端错误中的潜在凭据。ACP 不支持工作台的轮内信箱 steering；追问由下一轮投递。

## 验证

```powershell
scripts\cargo.cmd test -p sa-adapters --test acp -- --nocapture
```

测试使用真实 Node stdio fixture，无模型请求；覆盖 initialize/new/load、历史回放隔离、精确文本、模型配置、工具事件、受限拒绝、主机审批、无恢复能力、错误会话、未完成终态和原生取消。它不代表任意第三方 agent 的认证、沙箱或模型效果已经通过。新增服务必须在临时项目分别验证读写边界、恢复和取消。

来源：[官方 SDK](https://github.com/agentclientprotocol/rust-sdk)、[ACP 协议](https://agentclientprotocol.com/protocol/overview)。SDK 采用 Apache-2.0；通过现有 `Proc` 包装 `ByteStreams` 保留 Windows 隐藏窗口与整树回收，而不引入第二套进程管理。
