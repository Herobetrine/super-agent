# 验证方法

离线测试通过只证明协议与状态契约；本机 CLI 能否登录、实际推理和遵守权限需要分别做真实验证。以下命令在仓库根目录的 PowerShell 中运行；`scripts\cargo.cmd` 会准备 Windows GNU 工具链（见 [Rust 重构路线](rust-port.md#构建)）。

## 自动化回归

```powershell
scripts\cargo.cmd build --workspace           # 同时生成 MCP 测试服务 sa-ext-fixture-mcp
scripts\cargo.cmd test --workspace
scripts\cargo.cmd clippy --workspace --all-targets
```

- `sa-engine`：工单与任务状态机、`SA_*` 控制行、信箱、审批、持续运行、断线重试与熔断、供应商轮换、工单库独占、项目知识库（假执行端）。
- `sa-adapters`：四个执行端的目录发现、事件流、权限映射、取消后不残留子进程、stderr 不外泄、自定义供应商与 HTTPS 探测（本机夹具）。
- `sa-ext`：技能扫描、MCP stdio 与回环 HTTP、授权与撤销、幂等回执、取消后服务进程退出。
- `sa-desktop`：用 egui_kittest 通过无障碍树驱动真实窗口——输入、发送、审批、计划、模型选择、供应商与轮换、MCP 与技能、知识库；另测被占用的工单库只能只读观察。

## 界面截图

截图模式渲染后保存 PNG 并退出，默认不调度：

```powershell
target\debug\super-agent.exe --demo --screenshot demo.png --size 1440x900
target\debug\super-agent.exe --data <临时目录> --schedule --dialog models --screenshot models.png
target\debug\super-agent.exe --observe --ticket 5 --inspector knowledge --screenshot ticket.png
```

## 旧工单库续跑

不要对正在使用的工单库直接测试。先用 SQLite 备份接口复制一份（例如 `python -c "import sqlite3;s=sqlite3.connect('file:<原库>?mode=ro',uri=True);d=sqlite3.connect('<副本>/hub.sqlite3');s.backup(d)"`），再运行：

```powershell
$env:SA_LEGACY_COPY='<副本目录>'; scripts\cargo.cmd test -p sa-desktop legacy -- --nocapture
```

它在副本上启动调度器，检查恢复后没有遗留运行中任务，并给已完成的非持续工单追加指令续跑（假执行端，不调用模型）。

## 真实调用（计费）

下列开关会实际请求模型，只在临时项目中运行；用完清除环境变量。

| 开关 | 命令 | 检查 |
| --- | --- | --- |
| `SA_LIVE_CATALOG=1` | `scripts\cargo.cmd test -p sa-adapters --test catalog_live` | 只读探测四个执行端的供应商与模型目录 |
| `SA_LIVE_TURNS=1`（可选 `SA_LIVE_AGENTS=codex,claude`） | `… -p sa-adapters --test turns_live -- --nocapture` | 每个执行端读挑战文件、续接同一原生会话 |
| `SA_LIVE_ENGINE=1` | `… -p sa-adapters --test engine_live -- --nocapture` | DeepSeek 主控向其余三个执行端派工，核对回报与写者文件字节 |
| `SA_LIVE_UI=1` | `… -p sa-desktop live_composer -- --nocapture` | 从窗口输入框发起一轮并核对答案 |
| `SA_LIVE_EXT=1` | `… -p sa-desktop live_mcp -- --nocapture` | 批准后的 MCP 调用、`SA_MEMO` 写入知识库、完成后自动记进展 |
| `SA_LIVE_PROVIDER=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_custom -- --nocapture` | 自定义供应商探测与一轮读文件 |
| `SA_LIVE_ROTATION=agent,base_url,KEY_ENV,model` | `… -p sa-desktop live_rotation -- --nocapture` | 首选供应商认证失败后按轮换改用下一条路由 |

`KEY_ENV` 只写环境变量名，不要把密钥写进命令或文件。回执默认保存在当前用户的 `.local/share/superagent/checks`，只存哈希。完整事件可能含提示、模型回复、项目路径、会话标识和工具结果，检查时宜使用合成数据。

## 权限

复验权限时测量文件系统的实际变化，不要只看模型自述。一次 Windows 实机验证观察到：DeepSeek 在批准受限写入轮次后只能写项目内目标；Claude 的受限工具清单未开放 Write；Antigravity 的受限写入尝试也未产生目标文件；在完整权限下三个执行器都曾写入项目外专门准备的兄弟目录。完整权限与自动模式只适合在隔离账户或测试机上验证。
