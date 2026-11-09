# 检查点单文件恢复

在项目面板“变更”切换“与检查点比较”，选择文件并点击“预览恢复所选文件”。预览方向为当前内容到检查点内容，明确显示覆盖、重新创建或删除新增文件。只有点击“确认恢复此文件”才修改文件；取消和观察窗口均不写项目。一次只处理选中的一个文件，不隐式恢复整仓库。

恢复使用 Windows 独占文件句柄，重新核对预览时的 SHA-256 和长度，并用拒绝共享删除的目录句柄固定路径祖先。已打开的写入者、预览后的编辑、目录链接/重解析点、硬链接、路径逃逸、Windows 设备名称和损坏对象均拒绝。Git index 与 HEAD 不参与修改；其他文件及预览之后的新编辑保留。限制继承检查点的单文件 4 MiB。当前恢复仅支持普通 Windows 文件，不恢复符号链接或 POSIX 权限。

每次恢复前在应用数据 `review/<project hash>/restore-*/` 写入并 flush `before.bin`、`target.bin`、`intent.json`。成功记录 `result.json` 的 `committed`；可捕获的写入故障尝试通过仍独占的句柄恢复原内容，记录 `rolled_back` 或 `recovery_required`。仅移除本次创建且仍为空的目录。应用重启后会显示无终态或需要恢复的操作目录。遇到进程强制终止、磁盘故障或回滚失败，先核对 intent 中 root/path/摘要与当前文件，再从 before.bin 手工恢复；不会在重启时自动覆盖可能已被用户修改的文件。文件恢复不能撤销模型调用、远程 API 或数据库副作用。

实现参考微软的 [CreateFileW 共享模式与重解析点规则](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)、[SetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle) 和 [FILE_DISPOSITION_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_disposition_info)。使用系统 API，不增加第三方依赖。

验证：`scripts\cargo.cmd test -p sa-desktop review:: -- --nocapture`。测试使用临时 Git 仓库覆盖覆盖/新增删除/嵌套目录恢复、预览竞态拒绝、硬链接与 junction 拒绝、独占句柄防止并发写及目录重命名、故障回滚与备份回执、HEAD/index 保留；真实 App 的 egui_kittest 验证预览、取消、确认和观察窗口禁用。真实执行器、进程断电及任意文件系统驱动不在这些测试的证明范围内。

2026-11-09 专项执行：18 项通过、0 失败、0 忽略。包含新增文件故障后的文件/空目录清理，以及重开后识别缺失、损坏、需恢复的回执。本轮没有人工操作输入法、执行模型任务或验证断电恢复。
