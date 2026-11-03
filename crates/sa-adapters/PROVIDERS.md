# 自定义供应商与轮换（sa-store / sa-adapters）

运营方需求：“可在界面中添加供应商；提供轮换开关，某个供应商不可用时自动轮转到其他已配置供应商重试。”本文记录数据层与执行端的实现；界面与基于数据库的 `RotationSource` 由桌面端负责。

## 存储（`sa_store::providers`）

两张附加表，`CREATE TABLE IF NOT EXISTS`，旧 Node 引擎打开同一工单库时忽略它们：

- `studio_providers(id, agent, name, base_url, key_env, models JSON, enabled, position)`
- `studio_provider_rotation(singleton=1, enabled, routes JSON)`，`routes` 为 `{agent: [{provider, model, mode}]}`

接口：`ensure_schema`、`list`、`save`（空 id 时生成 12 位十六进制 id，否则插入或更新）、`remove`（同时从轮换顺序删除 `sa-<id>`）、`rotation`、`save_rotation`、`validate`、`validate_base_url`、`validate_rotation`。

**不保存密钥**：`key_env` 只存环境变量名。校验规则：

- `agent` ∈ `claude | codex | deepseek`；轮换顺序还允许 `antigravity`（可引用原生供应商）。
- `base_url`：HTTPS，或仅本机（`127.0.0.1`、`localhost`、`[::1]`）可用 HTTP；不得带用户名/密码、查询参数或片段；≤2048 字节；保存时去掉末尾 `/`。
- `key_env`：`^[A-Za-z_][A-Za-z0-9_]{0,127}$`；远程端点必填，本机端点可空；拒绝 `PATH`、`USERPROFILE` 等明显不是密钥的变量。
- 名称 ≤64 字符、无控制字符；模型 ID 与 Codex 规则相近（另允许 `@`、`[1m]` 之类后缀），去重，≤256 个；供应商 ≤64 个；每个执行端轮换路由 ≤32 条、`mode` 只能是 `cli`。

## 执行端接入（`sa_adapters::custom`）

- `Registry = Arc<RwLock<Vec<CustomProvider>>>`；`all_with(sys, registry)` 构造四个适配器，`all(sys)` 等于空注册表。每次 `catalog()`/`run()` 读取注册表快照，编辑后无需重建适配器。
- 目录合并：已启用、属于该执行端的条目追加为 `{"id": "sa-<id>", "custom": true, ...}`。缺少密钥变量时 `available:false`、`unavailable_reason:"auth"`，详情只写变量名；没有模型时为 `not_configured`。若原生配置已有同名 ID，跳过自定义条目，原生供应商不会被遮盖。
- 路由：`Route{provider:"sa-<id>", model, mode:"cli"}` 经 `common::select` 校验后执行（单测覆盖往返）。轮换列表直接使用这些 ID；`CustomProvider::route_id()` 给出同一 ID。

| 执行端 | 方式 | 密钥传递 | 官方依据 |
| --- | --- | --- | --- |
| Claude Code | 移除继承的 `ANTHROPIC_*`、`CLAUDE_CODE_OAUTH*`、`CLAUDE_CODE_USE_*`；设置 `ANTHROPIC_BASE_URL`（去掉末尾 `/v1`）、`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`、`CLAUDE_CODE_SUBAGENT_MODEL` 均为所选模型，以及 `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`；仍用 `--model` | 子进程环境（`Authorization: Bearer`） | [env-vars](https://code.claude.com/docs/en/env-vars)、[settings-reference#env](https://code.claude.com/docs/en/settings-reference) |
| Codex | `-c model_provider="sa-<id>"` 与 `-c model_providers.sa-<id>.{name,base_url,wire_api="responses",env_key}`；有 Codex profile 时同样写入 thread `config` | Codex 从继承环境读取 `env_key` 指向的变量；argv 只有变量名 | [config-reference](https://developers.openai.com/codex/config-reference)（现跳转 learn.chatgpt.com） |
| DeepSeek Harness | 每轮 policy 补丁中的 `llm-pi-ai.providers.sa-<id>`：`api: openai-completions`、`baseURL`、`apiKeyEnv: SUPERAGENT_DSH_CALL_KEY`、所选模型 `{id,name}` | 子进程环境 `SUPERAGENT_DSH_CALL_KEY`；补丁文件不含密钥 | 已安装 `@deepseek-ai/dsh-llm-pi-ai`（0.2.1-rc.5）的 `supportedProtocols()` 与 profile schema，见 `docs/deepseek.md` |
| Antigravity | 不支持（`agy` 走 Google 账号/Gemini API，未提供自定义端点） | — | — |

核对过的要点（2026-11-03）：

- Claude Code 设置文件中的 `env` 会覆盖从 shell 继承的同名变量；`CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` 使 Claude Code 忽略设置文件里的端点、认证和供应商选择变量，因此自定义端点与密钥不会被 `~/.claude/settings.json`（例如 CC Switch 写入的配置）或项目设置改写。`--model` 优先于 `ANTHROPIC_MODEL`。
- Codex `wire_api` 只接受 `responses`；`openai`、`ollama`、`lmstudio` 是保留 ID，`sa-` 前缀不会冲突；项目级 `.codex/config.toml` 不能设置 `model_providers`。
- DSH `llm-pi-ai` 支持 `openai-completions`、`openai-responses`、`anthropic-messages`；模型条目只有 `id` 必填。自定义供应商固定使用 `openai-completions`。

本机无密钥端点：Claude 与 DeepSeek 发送固定占位凭据 `superagent-local`，避免执行端回落到自身登录；Codex 不设置 `env_key`，不发送认证头。

## 获取模型与 HTTPS 目录

- `probe_models(&provider, &sys, &cancel)`：Claude 请求 `<根>/v1/models?limit=1000`（`x-api-key`、`Authorization: Bearer`、`anthropic-version: 2023-06-01`），Codex/DeepSeek 请求 `<base_url>/models`（Bearer）；解析 `data[].id` 或 Codex 目录 `models[].slug`；10 秒、4 MiB；错误信息不含密钥或响应正文。
- `common::get_json` 改用 [`ureq`](https://crates.io/crates/ureq) 3.5（rustls + ring，`platform-verifier` 使用系统证书库），支持远程 HTTPS，补上 Codex `model_catalog_url` 的远程目录。保留 JS 版的安全约束：不读环境代理、不跟随重定向（3xx 视为失败）、请求头不得含换行、默认 3.5 秒 / 1 MiB。选择 ureq 的理由：同步小客户端，无需引入 hyper/reqwest 全套；rustls 避免 Linux 上依赖 OpenSSL；ring 可用现有 llvm-mingw clang 编译（aws-lc-rs 需要 CMake/NASM）。
- Claude 原生（CC Switch）记录仍只对本机端点探测模型，远程来源列出已保存模型，与 JS 版一致。

## catalogRevision

`catalog_revision(&sys, agent)` 移植 `metadata.cjs` 与各适配器的 `watchPaths()`：对相关文件的路径、大小、修改时间、创建时间（Unix 为 ctime 与 inode）做 SHA-256，不读文件内容。`catalog_revision_with(&sys, agent, &registry)` 还纳入该执行端已启用的自定义供应商及其密钥变量是否存在（不含值）。未知执行端返回空串。与 Node 版数值不同（稳定 Rust 读不到 Windows 文件索引），只用于同一进程内比较。

## 限制与未验证

- 没有做任何真实模型调用：三个执行端对自定义端点的实际联通（Claude 的占位凭据、Codex `-c` 覆盖下的 `thread/start` 回显、DSH 对 `sa-` 供应商 ID 的接受）均未经真实运行验证；只有参数/环境构造与本机 HTTP 探测有离线测试。
- 密钥取自 `Sys` 快照：桌面启动后新设置的环境变量需重建 `Sys`（或重启）才会生效。
- 探测与目录请求不走代理；需要代理才能访问的远程端点会探测失败，而执行端子进程本身仍按各自规则使用代理。
- 自定义供应商不支持自定义请求头、查询参数或其他协议选择（Codex 固定 Responses，DeepSeek 固定 Chat Completions，Claude 固定 Anthropic Messages）。
- 执行端自身配置无法读取时（例如 Codex `config.toml` 无效），`catalog()` 整体报错，自定义条目也不会列出，与原生行为一致。

## 验证

```powershell
node scripts/cargo.cjs test -p sa-store -p sa-adapters
node scripts/cargo.cjs clippy -p sa-store -p sa-adapters --all-targets
```

覆盖：存储增删改、轮换校验与删除时清理、附加表可在 `open_journal` 的库上重复创建；目录合并（可用/缺密钥/无模型/ID 冲突）与 `select` 往返；Claude 环境、Codex `-c` 参数（逐项按 TOML 解析）、DeepSeek policy 与密钥只进环境；进程内 `TcpListener` 上的 OpenAI/Anthropic/无密钥探测、401 不泄露密钥、不跟随 302、1 MiB 上限、取消；`catalog_revision` 随文件和注册表变化。
