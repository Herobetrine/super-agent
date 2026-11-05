# Claude Code 与 Antigravity

## 供应商与模型

打开设置时，平台读取当前机器的配置。已选的供应商和模型随工单保存。外部供应商列表来自本地记录，列出选项不会触发向这些端点的模型请求。用户选择后，执行器重新匹配记录，向子进程交付该记录配对的端点、认证方式和模型。改名、轮换密钥后可重新匹配。删除记录、改变来源或移除所选模型时，旧选择会报错。

**Claude Code** 分别读取 CC Switch 的 `claude` 和 `claude-desktop` 记录，识别 Anthropic Messages 协议、配置中的模型及 Desktop 的上游模型映射。每个可用来源以自己的端点和配对凭据读取 `/v1/models`，按 `has_more`、`last_id` 和 `after_id` 处理分页。模型能力来自 API 元数据以及本机 initialize 返回的精确型号能力，思考强度通过 `--effort` 传递。Windows 自定义数据库目录来自 CC Switch 的 `app_paths.json` 中的 `app_config_dir_override`。[模型 API](https://platform.claude.com/docs/en/api/models/list) · [模型与强度](https://code.claude.com/docs/en/model-config) · [CC Switch 配置层级](https://github.com/farion1231/cc-switch/blob/main/docs/user-manual/en/5-faq/5.1-config-files.md)

新工单优先采用 CC Switch 中 Claude Desktop 当前选择的供应商。未配置这项选择时，可直接使用原生 CLI 的 `settings.json`、`CLAUDE_CONFIG_DIR` 或进程环境中的配置。官方账号先通过 `claude auth status` 验证，再从 SDK 的 `initialize` 回执取得可用模型。未登录、配置条目损坏或协议无法适配时，列表说明原因。早期工单继续沿用其保存的路由。配置文件、数据库及其 WAL 均包含在 `watch_paths()` 中。

**Antigravity** 从实际安装的 `agy models` 获取账号可见的型号。原生模式优先采用机器明确指定的型号；没有模型设置时使用“跟随 agy 原生默认模型”，启动命令省略 `--model`。明确选择型号则必须匹配实时目录。启用 ADC 的机器可以选择 Google Cloud 账号；有 Gemini 兼容 API 配置或 CC Switch `agy` 供应商记录的机器可以选择对应的本地模型。受限档使用按供应商隔离的配置和会话目录。[Google 认证方式](https://antigravity.google/docs/install?tab=cli) · [CC Switch 对 agy 的配置投影](https://github.com/farion1231/cc-switch/blob/main/src-tauri/src/agy_config.rs)

原生型号的思考强度由只读 `/effort` 命令查询，运行任务时通过 `--effort` 传入。Gemini API 来源在线读取带分页的 `/v1beta/models`。CLI 输出为空或格式无法识别时，默认占位项标记为回退目录。[Antigravity effort](https://antigravity.google/docs/cli/commands/effort/)

## 执行端与安装

两套 CLI 均使用 UTF-8 NDJSON 双向消息，原生终态用于确认任务完成。恢复会话时分别传入 `session_id` 或 `conversation_id`。[Claude 无头模式](https://code.claude.com/docs/en/headless) · [Antigravity 无头模式](https://antigravity.google/docs/cli/headless)

只读、`ask` 和 `partial` 下，Claude Code 开放 Read、Glob、Grep，Antigravity 使用隔离的文件、命令和 MCP 拒绝规则。历史验证中，Antigravity 的 `accept-edits` 能写到临时工作区以外，适配器因此没有使用它作为项目内写权限。显式选择 `full` 或 `auto` 后，Claude 采用 `bypassPermissions`，Antigravity 采用 `--dangerously-skip-permissions`，两者均可用当前用户权限操作项目外资源。[Claude 工具权限](https://code.claude.com/docs/en/permissions) · [Antigravity 权限](https://antigravity.google/docs/permissions?tab=cli)

桌面端仅在具备经验证的全自动收发能力时才能选择。Claude Desktop 的 Code 页不提供 `--print` 或流式终稿输出。Antigravity App 的 Remote Control 供人在网页上操作。该适配器尚无通过自动回传与权限验证的桌面桥接。这两个桌面选项都标记为不可用。以 Desktop 供应商配置执行 CLI 时，执行端始终标注为 CLI。[Claude Desktop 与 CLI 对照](https://code.claude.com/docs/en/desktop) · [Google Remote Control](https://antigravity.google/docs/remote-control/) · [社区桥适用范围](https://github.com/Kio-pon/antigravity-mcp)

官方安装页面随应用设置提供。[Claude Code 安装](https://code.claude.com/docs/en/setup) · [Claude Desktop 下载](https://claude.com/download) · [Antigravity CLI 安装](https://antigravity.google/docs/cli/install) · [Antigravity 桌面下载](https://antigravity.google/download)

配置或版本变更后，可以通过[验证方法](verification.md)重新检查。
