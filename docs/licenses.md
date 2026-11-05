# 依赖与第三方说明

Super Agent 源码和仓库自有资源采用 `GPL-3.0-only`。[许可证全文](../LICENSE) · [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html)

## Rust 依赖

精确版本记录在 `Cargo.lock`。主要组件：

| 组件 | 用途 | 许可 |
| --- | --- | --- |
| egui / eframe、egui_kittest | 桌面界面与界面测试 | MIT 或 Apache-2.0 |
| egui-phosphor（Phosphor Icons） | 界面图标 | MIT |
| rusqlite（内置 SQLite） | 工单库 | MIT；SQLite 属公共领域 |
| tokio、futures | 异步运行时 | MIT |
| rmcp | 官方 MCP Rust SDK | Apache-2.0 |
| reqwest、ureq、rustls | HTTP 与 TLS | MIT 或 Apache-2.0（rustls 另含 ISC） |
| rfd | 系统文件对话框 | MIT |
| windows-rs | Windows 原生通知、COM 与应用登记 | MIT 或 Apache-2.0 |
| image、pulldown-cmark、serde、anyhow 等 | 图标解码、Markdown、序列化、错误处理 | MIT 或 Apache-2.0 |

界面字体（Segoe UI、Microsoft YaHei、Consolas）运行时从 Windows 系统字体目录读取，不随仓库分发。重新分发二进制时应一并保留上游的许可证和第三方通知（可用 `cargo about` 或 `cargo license` 生成清单）。

## 外部执行器

本项目通过各工具的公开命令行入口通信。各执行器、模型服务、账户、订阅和 API 配额由使用者单独安装或取得。当前仓库不包含厂商二进制程序、模型权重、SDK 源码或账户凭据。

- [OpenAI Codex](https://github.com/openai/codex)
- [Claude Code](https://code.claude.com/docs/en/setup)
- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
- [Google Antigravity](https://antigravity.google/download)
- [CC Switch](https://github.com/farion1231/cc-switch) 是可选的本机配置来源。

DeepSeek Super-agent 原型、OpenHands 和 OpenCode 作为架构资料列在[项目说明](../README.md)中。
