# Codex

安装并登录本机 Codex CLI，先在终端检查 `codex --version`，再打开控制台 **连接与诊断**。Windows 适配器使用原生可执行文件，通过 stdio 与 app-server 通信。[安装 CLI](https://developers.openai.com/codex/cli/) · [app-server 协议](https://developers.openai.com/codex/app-server)

## 供应商与会话

模型选项来自本机使用的配置、profiles、模型缓存和可用的原生模型清单。控制台区分跟随原生默认和显式固定组合。正在执行的任务沿用启动时的选择，选择发生变化时需在新轮次重新解析。桌面客户端的交互界面与这里使用的 app-server 执行入口分开展示。

输入发送到指定的原生 thread 和 turn。恢复会话需收到匹配的线程标识；派工期间可给活动任务投递信件，平台以原生 steering 回执记录轮内接受。连接中断后未确认的消息进入后续轮次。即使已经收到协议回执，模型是否实际采用消息仍应检查其输出。

## 权限档位

`ask` 以只读沙箱启动，必要的执行请求交给用户审批。`partial` 开放工作区写入，超出该范围的请求可通过原生审批流程处理。`full` 和 `auto` 使用 `danger-full-access` 并关闭逐次审批，实际权限可超出当前项目。控制台会检查启动时返回的策略与原生终态。[Codex app-server](https://developers.openai.com/codex/app-server)

## 复验

在临时项目中创建一个只有你知道内容的文件，只在提示里提供文件名并要求读回内容。回复后追加一条不允许再读文件的指令，要求复述上轮内容。分别比较文件、原生终稿和会话 ID。修改本机 provider、CLI 版本或沙箱配置后可重复这一检查。公开的测试入口见[验证方法](verification.md)。
