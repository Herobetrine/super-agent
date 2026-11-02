# 依赖与第三方说明

Super Agent 源码和仓库自有资源采用 `GPL-3.0-only`。[许可证全文](../LICENSE) · [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html)

## Python 运行依赖

| 组件 | 在项目中的用途 | 发行者的许可说明 |
| --- | --- | --- |
| Python | 标准库、子进程和 SQLite 接口 | [Python 许可证](https://docs.python.org/3/license.html) |
| PyQt6 | 桌面窗口、测试和原生事件循环 | [Riverbank · GPL v3 或商业许可](https://en.riverbankcomputing.com/software/pyqt) |
| Qt 6 | PyQt6 wheel 使用的 Qt 运行库；模块可以采用不同许可 | [Qt Licensing](https://doc.qt.io/qt-6/licensing.html) |
| PyQt6-sip | PyQt6 使用的绑定运行库 | [PyPI 项目和许可元数据](https://pypi.org/project/PyQt6-sip/) |
| PyYAML | 解析 Harness 的 YAML 供应商记录 | [MIT](https://github.com/yaml/pyyaml/blob/main/LICENSE) |
| Tomli | Python 3.10 的 TOML 解析；3.11 及以上使用标准库 `tomllib` | [MIT](https://github.com/hukkin/tomli/blob/master/LICENSE) |

这些依赖由 `pip` 安装。打包分发 Qt、Python wheel 或其他运行库时，请保留相应包随附的许可证和通知。[PyQt 许可说明](https://en.riverbankcomputing.com/software/pyqt) · [Qt 第三方组件](https://doc.qt.io/qt-6/licenses-used-in-qt.html)

## 外部执行器

本项目通过各工具的公开命令行入口通信。各执行器、模型服务、账户、订阅和 API 配额由使用者单独安装或取得。当前仓库不包含厂商二进制程序、模型权重、SDK 源码或账户凭据。

- [OpenAI Codex](https://github.com/openai/codex)
- [Claude Code](https://code.claude.com/docs/en/setup)
- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
- [Google Antigravity](https://antigravity.google/download)
- [CC Switch](https://github.com/farion1231/cc-switch) 是可选的本机配置来源。

DeepSeek Super-agent 原型、OpenHands 和 OpenCode 作为架构资料列在[项目说明](../README.md)中。
