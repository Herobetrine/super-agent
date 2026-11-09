# DeepSeek Harness

## 安装

适配器支持 `@deepseek-ai/dsh@0.2.1-rc.5` 和 `0.2.2-rc.1`，在用户目录中独立运行。新版要求 Node.js 24.9 或以上。旧包与旧会话保留；不要在旧目录覆盖安装。先确认 `node --version` 与 `npm --version`，再在 PowerShell 安装新版：

```powershell
$runtime = Join-Path $env:USERPROFILE '.local\share\superagent\dsh-runtime-0.2.2-rc.1'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
npm install --prefix $runtime --save-exact --ignore-scripts --no-audit --no-fund '@deepseek-ai/dsh@0.2.2-rc.1'
node --expose-internals (Join-Path $runtime 'node_modules\@deepseek-ai\dsh\lib\bin.js') --version
```

保留安装目录中的 `package-lock.json`，可在同目录使用 `npm ci --prefix $runtime --ignore-scripts` 重建。用 `$env:SUPERAGENT_DSH_RUNTIME = $runtime` 在独立进程中选择待验版本；不会修改其他已运行工作台的环境。平台核对包版本和 Node 版本后才启动。默认保留旧 `dsh-runtime`；新版目录完成联测并具有 `verified-runtime.json` 验证回执后，新会话才优先使用新版。已有会话通过供应商与原生会话身份的哈希记录恢复到原运行时；没有记录的历史会话继续旧版。显式环境变量优先。

新版使用 `dsh-state/0.2.2-rc.1/<权限>` 独立状态目录，旧版保留 `dsh-state/<权限>`，不迁移或覆盖旧会话。新版仅在进程成功退出、终态 `final.sessionId` 匹配本次会话且 `final.reason.kind=completed` 时确认成功；旧版使用对应版本的事件协议。[官方 rc.1 变更](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.2.2-rc.1) · [官方 headless 接口](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.2-rc.1/packages/bundle/headless/README.md)

2026-11-09 已在 Windows / Node 24.16.0 实测新版只读文件挑战、同会话恢复、终稿逐字节匹配，以及工作区内写入成功、实际项目外写入尝试被拒绝、原文件保持不变、用量上报和写入会话恢复。旧版 `0.2.1-rc.5` 同日重新通过只读与恢复回归。真实执行器测试命令如下；测试使用临时项目，不打开工作台工单库，但会产生模型费用。测试保存独立回执，环境开关缺省关闭不算真实通过。

```powershell
$env:SUPERAGENT_DSH_RUNTIME = $runtime
$env:SA_LIVE_TURNS = '1'
$env:SA_LIVE_AGENTS = 'deepseek'
scripts\cargo.cmd test -p sa-adapters --test turns_live -- --nocapture
$env:SA_LIVE_DSH_BOUNDARIES = '1'
scripts\cargo.cmd test -p sa-adapters --test deepseek_upgrade_live -- --nocapture
```

`verified-runtime.json` 的 `version` 必须为 `0.2.2-rc.1`，并同时记录 `readonly_resume=true` 和 `workspace_boundary_resume=true`；只有实际通过上述检查后才应写入该验证记录，保留相应回执路径和摘要。仅安装成功或 `--version` 返回正确不能启用该标记。

## 配置与工作方式

先在原生 Harness 或支持的供应商配置中完成认证。在 **模型与执行端 → DeepSeek Harness** 中可以核对本机发现的供应商、模型、协议和可用状态。官方 API 的凭据使用原生环境变量或工具自身的凭据来源。外部网关的端点、模型与认证必须匹配同一条供应商记录。

平台读取原生配置、兼容的 CC Switch 配置及供应商在线模型目录。官方目录再经已安装运行时的 `parseModelListing` / `resolveListedCatalog` 检查版本与协议兼容性。思考强度使用运行时声明的级别及 wire 映射，显式选择非 off 时相应启用 thinking。显式选定的模型在调用时复查，上游模型名与协议错误将作为任务失败返回。运行时 profile 和原生会话状态位于当前 Windows 用户目录。模型请求通常携带任务上下文，端点可自行决定保留策略。

## 权限档位

`ask` 下先核对并批准当前写入轮次，批准后仍限定在工作区；`partial` 直接采用工作区写入权限。`full` 和 `auto` 使用当前用户的完整访问能力并关闭原生重复审批。权限应与任务实际需要的文件和命令相匹配。

## 设计出处

工作流中的主控派工、独立会话、消息持久化和恢复参考了 DeepSeek Super-agent 研究原型。研究原型包含面向 GitHub 的调度与运行环境，本项目提供交互式 Windows 本地工作台。[固定版本设计与执行流程](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/6337fc823ce20251c8de8085948165c81aaa701e/docs/003-execution.md) · [架构说明](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/6337fc823ce20251c8de8085948165c81aaa701e/docs/007-architecture.md)
