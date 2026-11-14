# MCP 工单私有连接复用

2026-11-09 实现。范围是 `ProjectExtensions::invoke_call`，目录探测仍是一次性连接。

## 协议与边界

使用现有 `rmcp 3.8` 的 initialize/capabilities 生命周期。同一工单可以继续使用其已初始化的连接；不同工单不共享 MCP 会话，不宣称实现 OpenCode 的跨项目共享握手。需要 roots、sampling 或 elicitation 的服务不会因连接复用获得额外客户端能力。参考 [MCP 生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)、[传输和会话语义](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) 及 [OpenCode 连接池](https://github.com/anomalyco/opencode/blob/12a661ea40cd5863b271a89bb06b8a3ddb7ab5a0/packages/core/src/mcp/pool.ts)。

连接键包含工单、服务 ID、授权版本，以及启动配置、规范化工作目录、安全继承环境和实际 Bearer 凭据的 SHA-256。凭据只读取一次，同一快照用于选键和建立连接；键不写入日志，明文凭据不入库。stdio 使用本轮 `InvokeArgs.cwd`，空目录保留可执行文件所在目录的兼容行为。

每个键串行租用；等待租约可取消且有超时。成功返回的工具结果（包括 `isError`）允许复用。传输错误、超时、取消、future 被丢弃都会淘汰连接。已确认关闭的缓存连接在发起调用前重建；工具调用一旦发出失败，绝不自动重放。初始化失败允许下一条新调用重连。

最多缓存 64 个会话，满额时新键使用一次性私有连接，不淘汰别人的活动调用。每 60 秒检查空闲，空闲至少 60 秒且未被租用时关闭（最迟约 120 秒；正在执行的工具不受空闲回收影响）。改变配置、禁用或移除服务同步取消所有对应池槽；活动调用保留 `uncertain` 回执。服务关闭/最后一个 owner 丢弃时全部取消。实际进程清理由 rmcp 和 process-wrap 完成。

`pending → done/error/uncertain` 和 `(job, logical turn, id)` 幂等契约保持不变。每次调用仍重新验证工单绑定、审批、活动轮次和服务版本；复用连接不复用审批。远程工具已经产生的效果不能因关闭连接撤销，`uncertain` 必须核查外部状态。HTTP 服务可以在服务端共享全局状态，客户端的工单隔离不能替代服务器的租户隔离。

## 验证

`scripts\cargo.cmd test -p sa-ext` 启动真实独立 Rust stdio/回环 HTTP 服务（不调用模型）：验证初始化计数、同工单 PID 复用、不同工单/工作目录隔离、配置撤权、活动调用取消/清理、失败初始化恢复、传输断开不重放及新调用重连。原有审批、回执幂等、环境隔离、HTTP Bearer、完整引擎工具轮次测试继续执行。

未认证任意第三方远程服务的跨工单隔离，也没有实现跨项目共享握手、OAuth 或服务端幂等。rmcp/MCP 版本、权限契约、工作目录或运行时关闭语义改变后重新运行以上测试。
