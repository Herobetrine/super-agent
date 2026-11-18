# 上下文选择：需求入包、角色内容包与知识失效

P1B 在不改变增量投递、权限和回执的前提下，决定每个角色每轮收到哪些材料，并记录这次选择。选择在完整信封构建、补充扩展信息之后、生成快照之前运行，所以快照和增量差异都基于“选择后”的包，`context.snapshot` 仍是实际交付内容的无损记录。没有交付合同的旧工单行为不变。

## 各角色收到什么

角色由结构决定：根任务是主控；写入型子任务是实现者；其余子任务是审查者；顾问走单独的咨询路径。

| 内容 | 主控 | 实现者 / 审查者 | 顾问 |
| --- | --- | --- | --- |
| `requirements` | 全部有效（未撤回）项 | 全部必需项，加上关联到自己任务的项，逐项标 `assigned` | 仅必需项，作为提示词中的“当前需求”段，最多 4 KiB |
| `roster` | 全部任务 | 只留父任务、自己、直接子任务和前置任务，被省略的行数写入 `roster_omitted` | 不适用 |
| `user_updates`、`reports`、`prerequisites`、`files`、`notes`、`checks`、`recovery` | 完整 | 完整，不裁剪 | 不适用 |
| `project_knowledge` | 按相关性选取 | 同左 | 不适用 |

`requirements` 是一个对象：`contract_revision`、`gate_enabled`（交付门是否启用）、`items`。每项包含 `id`、`revision`、`text`（最多 600 个字符，按字符边界截断）、`required`；有已采用行为检查且对应当前修订时带 `check`（参数以空格连接），有文件摘要标准时带 `path`；状态来自当前合同版本的最近一次交付评估，只会是 `pass`、`fail`、`stale`、`unknown`，没有可用评估时省略。排序为必需项优先，其次按需求编号。

大小上限是 32 项、16 KiB。超限时先丢弃可选项，再丢必需项，并在 `requirements_omitted` 给出 `count`、`not_assigned`（角色规则省略的可选项）、`truncated`（超限省略的项）和 `fetch`。`fetch` 只提示向主控或操作员询问，平台没有为此新增工具。`requirements` 与 `requirements_omitted` 属于增量投递的“当前”字段，每轮整体重发；也列入压缩的保留字段，压缩视图中不会被替换成摘要。

## 选择清单 `context.selection`

每个调度轮在写入 `context.snapshot`、`input`、`running` 的同一事务中记录一条，写在该任务上。顾问咨询在 `advice.started` 之后记录同类事件，写在调用者的任务上。

```json
{
  "schema_version": 1,
  "policy": "roles-1",
  "role": "lead | implementer | reviewer | advisor",
  "mode": "full | delta | recovery | compacted | advice",
  "sources": [
    {"key": "roster", "reason": "role_pack:...", "kept": 3, "of": 5},
    {"key": "requirements", "id": 7, "revision": 2, "reason": "required | assigned | active"},
    {"key": "project_knowledge", "id": 12, "state": "current | stale", "reason": "lexical_match | recent"},
    {"key": "reports", "count": 2, "reason": "always_kept"}
  ],
  "sources_omitted": 0,
  "omitted": [{"key": "roster", "count": 2, "reason": "role_pack:implementer", "fetch": "..."}]
}
```

清单不超过 16 KiB，来源最多 256 条。超过时从尾部截断并写入 `sources_omitted`；受角色规则影响的裁剪项和需求项排在前面，最后被截断。`mode` 是线上视图，清单本身描述的是完整快照的选择。

## 知识类型与失效

新增副表 `knowledge_basis(entry, type, paths, basis, verified, state, superseded_by)`，与 `project_knowledge` 一同创建。没有该表的旧库按空处理。

`SA_MEMO` 仍接受只含 `kind` 与 `text` 的形式，行为不变。可选键：

- `paths`：最多 8 个项目相对路径（校验同交付文件，重复项合并）。入库时从该任务的工作目录计算每个文件的 SHA-256，缺失记为 `missing`，不是普通文件、含链接、超过 4 MiB 记为 `unverifiable`。同时记录 `basis=files`、`verified` 为当前时间、`type`（memory 记为 fact，其余同 kind）。
- `supersedes`：本项目一个有效条目的编号。新条目写入后，旧条目被归档，`state=superseded`，`superseded_by` 指向新条目。条目不存在、已归档或属于其他项目时，新条目仍写入，旧条目不动，并记录 `knowledge.rejected` 原因 `supersedes_unavailable`。
- 其他键、超过 8 个路径或无效路径使整条备忘被拒绝（`invalid_fields`）。

每轮选取知识后，对带路径的条目从项目根目录重新计算摘要，最多检查 64 个条目，单文件 4 MiB，整轮读取上限 16 MiB，超出的按无法核验处理。有文件摘要与记录不同则状态为 `stale`，并写回副表。信封中该条目带 `status:"stale"` 与 `stale_paths`，并排在所有当前条目之后。文件恢复到记录的内容后，下一轮条目重新变为当前。无路径的条目不带 `status`，与旧行为一致。

过期知识只作为历史说明，不是当前证据。KNOWLEDGE 指南已写明可选键和这一点。操作员接口 `Engine::knowledge` 返回的条目新增可选字段 `state`、`paths`、`superseded_by`、`verified`，均有默认值。

## 限制

- 文件摘要只证明记录时与检查时的字节是否相同，不证明备忘内容正确；备忘由隔离工作区中的任务写入时，摘要来自该工作区，合入前项目中的文件可能不同，此时条目会显示过期。
- 需求状态来自最近一次已保存的评估，没有评估时不显示，不会自行运行检查。
- 顾问只看到必需项，不读取非必需项和协作清单；顾问仍然只读，没有新增总线。
- 路径超出 64 个条目或 16 MiB 读取预算的条目沿用上次保存的状态，不重新核验。
- 对话中的指令不会改变清单；选择策略版本固定为 `roles-1`。

## 验证

`cargo test -p sa-engine --test context_selection` 覆盖主控与协作者的需求标注、压缩保留、无合同工单、协作者名单裁剪与清单、增量投递字节减少及需求修订重发、顾问提示与清单、备忘过期与还原、`supersedes` 的项目范围。单元测试在 `context_selection.rs` 和 `knowledge.rs` 中。构建与测试结果见提交说明。
