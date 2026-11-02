# DeepSeek Harness

## 安装

适配器使用上游 npm 包 `@deepseek-ai/dsh@0.2.1-rc.5`，在用户目录中单独运行。Windows 上已用 Node.js 24 验证。先安装 Node.js 并确认 `node --version` 与 `npm --version` 可运行，再在 PowerShell 执行：

```powershell
$runtime = Join-Path $env:USERPROFILE '.local\share\superagent\dsh-runtime'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
npm install --prefix $runtime --save-exact --ignore-scripts --no-audit --no-fund '@deepseek-ai/dsh@0.2.1-rc.5'
node --expose-internals (Join-Path $runtime 'node_modules\@deepseek-ai\dsh\lib\bin.js') --version
```

保留安装目录中的 `package-lock.json`，可在同目录使用 `npm ci --prefix $runtime --ignore-scripts` 重建。平台核对安装包的版本后才启动。升级适配器固定的版本时，应同步复验配置格式、权限策略、输出事件和会话恢复。[上游安装说明](https://github.com/deepseek-ai/deepseek-harness#run-from-npm) · [官方 headless 接口](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/bundle/headless/README.md)

## 配置与工作方式

先在原生 Harness 或支持的供应商配置中完成认证。在 **模型与执行端 → DeepSeek Harness** 中可以核对本机发现的供应商、模型、协议和可用状态。官方 API 的凭据使用原生环境变量或工具自身的凭据来源。外部网关的端点、模型与认证必须匹配同一条供应商记录。

平台读取原生配置、兼容的 CC Switch 配置，以及隔离运行时的模型目录。显式选定的模型在调用时复查；上游的模型名与协议错误将作为任务失败返回。运行时 profile 和原生会话状态位于当前 Windows 用户目录。模型请求通常携带任务上下文，端点可自行决定保留策略。

## 权限档位

`ask` 下先核对并批准当前写入轮次，批准后仍限定在工作区；`partial` 直接采用工作区写入权限。`full` 和 `auto` 使用当前用户的完整访问能力并关闭原生重复审批。权限应与任务实际需要的文件和命令相匹配。

## 设计出处

工作流中的主控派工、独立会话、消息持久化和恢复参考了 DeepSeek Super-agent 研究原型。研究原型包含面向 GitHub 的调度与运行环境，本项目提供交互式 Windows 本地工作台。[固定版本设计与执行流程](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/6337fc823ce20251c8de8085948165c81aaa701e/docs/003-execution.md) · [架构说明](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/6337fc823ce20251c8de8085948165c81aaa701e/docs/007-architecture.md)
