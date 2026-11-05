//! Synthetic journal used by `--demo` for development and screenshots.
//!
//! Covers every rendering path: user and follow-up messages, lead and peer
//! replies with markdown and SA_* control lines, progress rows, failures,
//! approvals, a continuous run, plan items and native subagent tasks.

use crate::data::DataSource;
use anyhow::{Result, bail};
use sa_store::{
    Approval, ContinuousRun, EventView, JobView, JobsPage, LatestActivity, NativeActivity,
    NativeTask, PlanItem, Snapshot, TicketDetail, TicketSummary,
};
use serde_json::{Value, json};
use std::time::Instant;

struct Ticket {
    detail: TicketDetail,
    jobs: Vec<JobView>,
    events: Vec<EventView>,
    approvals: Vec<Approval>,
    run: Option<ContinuousRun>,
    plan: Vec<PlanItem>,
    native: NativeActivity,
}

pub struct DemoSource {
    tickets: Vec<Ticket>,
    started: Instant,
    live_added: usize,
}

fn route(provider: &str, model: &str, mode: &str) -> Option<Value> {
    Some(json!({"provider": provider, "model": model, "mode": mode}))
}

#[allow(clippy::too_many_arguments)]
fn job(
    id: i64,
    agent: &str,
    parent: Option<i64>,
    role: &str,
    state: &str,
    turn: i64,
    body: &str,
    result: Option<&str>,
    r: Option<Value>,
) -> JobView {
    JobView {
        id,
        agent: agent.into(),
        parent,
        role: role.into(),
        state: state.into(),
        turn,
        writing: false,
        ready_after: 0.0,
        permission_override: None,
        body: body.into(),
        result: result.map(str::to_string),
        route: r,
    }
}

/// Builds events with increasing sequence numbers and timestamps.
struct Log {
    seq: i64,
    ts: f64,
    events: Vec<EventView>,
}

impl Log {
    fn new(seq: i64, ts: f64) -> Self {
        Self {
            seq,
            ts,
            events: Vec::new(),
        }
    }
    fn push(
        &mut self,
        dt: f64,
        kind: &str,
        job: i64,
        turn: i64,
        agent: Option<&str>,
        payload: Value,
    ) {
        self.seq += 1;
        self.ts += dt;
        self.events.push(EventView {
            seq: self.seq,
            kind: kind.into(),
            ts: self.ts,
            job: Some(job),
            turn: Some(turn),
            agent: agent.map(str::to_string),
            payload,
        });
    }
    fn act(&mut self, dt: f64, job: i64, turn: i64, agent: &str, payload: Value) {
        self.push(dt, "activity", job, turn, Some(agent), payload);
    }
}

fn detail(
    id: i64,
    goal: &str,
    cwd: &str,
    lead: &str,
    created: f64,
    profile: &str,
    state: &str,
) -> TicketDetail {
    TicketDetail {
        id,
        goal: goal.into(),
        objective: None,
        cwd: cwd.into(),
        lead: lead.into(),
        created,
        permission_profile: profile.into(),
        agent_routes: json!({}),
        allow_lead_routing: true,
        state: state.into(),
    }
}

const LEAD_REPLY_1: &str = r#"我先梳理了现有桌面端的结构，结论如下。

## 现状
- **界面层**：egui 窗口在后台线程读取只读快照，操作经调度线程按顺序执行。
- **数据层**：工单日志保存在 SQLite（`hub.sqlite3`），事件按 `seq` 递增，可以增量读取。
- **调度层**：仍由现有工作台负责，迁移期间新界面只做只读观察。

## 拆分计划
1. 新建 `sa-store`，按 `runtime/studio.cjs` 的查询语义读取工单。
2. 新建 `sa-desktop`，用 egui 渲染侧栏、对话流与项目面板。
3. 在后台线程读取数据库，界面线程只消费通道里的结果。

```rust
let (tx, rx) = std::sync::mpsc::channel();
std::thread::spawn(move || loop {
    let snap = store.snapshot(ticket, cursor)?;
    tx.send(snap)?;
});
```

| 模块 | 负责人 | 状态 |
| --- | --- | --- |
| 数据层 | Codex | 审查中 |
| 性能分析 | DeepSeek | 执行中 |
| 截图核验 | Antigravity | 等待执行 |

SA_PROGRESS {"id":"survey","summary":"已完成现有结构梳理，开始拆分数据层与界面层"}
SA_SEND {"id":"review-store","to":"codex","role":"代码审查","body":"审查 sa-store 的只读查询是否与 studio.cjs 等价"}
SA_SEND {"id":"perf","to":"deepseek","role":"性能分析","body":"测量 180 条事件全量刷新的耗时"}
SA_GOAL {"objective":"桌面端以纯 Rust 只读观察全部工单，并与现有调度保持兼容"}"#;

const PEER_REPLY: &str = r#"审查完成，`sa-store` 的查询与 `studio.cjs` 基本等价，有两处需要注意：

1. `snapshot` 在 `after > 0` 时应按 `seq ASC` 取前 180 条，否则追更时会跳过事件。
2. `history` 的 LIKE 转义需要同时处理 `\`、`%` 和 `_`。

其余部分（任务排序、审批筛选、计划读取）均已对照测试通过。
SA_PROGRESS {"id":"review","summary":"查询语义已核对，发现 2 处边界问题"}"#;

const LEAD_REPLY_2: &str = r#"收到补充要求。暗色主题已按同一套品牌色派生，正文与背景的对比度保持在 **7:1** 以上，次要文字不低于 *4.5:1*。

目前进展：
- 侧栏按项目文件夹分组，显示状态与相对时间；
- 对话流支持 Markdown、代码块和表格，控制指令折叠为紧凑标签；
- 进展记录按连续片段聚合，可展开查看来源。

下一步会补齐窄窗口下的布局，并核对中文字形是否完整。
SA_PROGRESS {"id":"ui","summary":"主体界面完成，开始核对窄窗口与暗色主题"}
SA_NEXT {"body":"在 1000×760 窗口下核对检查器与对话列的宽度分配"}
SA_LEARN {"lesson":"egui 的标签在右对齐布局中不会自动收缩，气泡需要先排版再定位"}"#;

impl DemoSource {
    pub fn new() -> Self {
        let now = crate::labels::now_secs();
        let tickets = vec![
            main_ticket(now),
            simple(
                41,
                "定位账单导出偶发超时的问题，并给出可复现的最小用例",
                "D:\\Work\\billing-service",
                "codex",
                now - 3.5 * 3600.0,
                "waiting",
                "已复现超时：导出任务在持有连接池锁的同时等待对象存储响应。\n\n正在等待两位协作者分别验证修复方案。",
                86,
            ),
            simple(
                40,
                "检查工作台的协作进展与验证记录",
                "E:\\Projects\\Super Agent",
                "codex",
                now - 5.0 * 3600.0,
                "done",
                "已整理工具调用和成员汇报。现在可以在项目面板按任务查看对应记录。\n\n界面验证覆盖了桌面宽度、窄窗口和历史对话。",
                1,
            ),
            simple(
                39,
                "为文档站点补充中文搜索",
                "D:\\Work\\docs-site",
                "deepseek",
                now - 26.0 * 3600.0,
                "queued",
                "",
                1,
            ),
            simple(
                38,
                "整理桌面端的项目导航",
                "E:\\Projects\\Super Agent",
                "claude",
                now - 2.2 * 86400.0,
                "cancelled",
                "",
                2,
            ),
            simple(
                36,
                "升级 tokio 并修复编译错误",
                "D:\\Work\\billing-service",
                "antigravity",
                now - 3.4 * 86400.0,
                "failed",
                "",
                3,
            ),
            simple(
                35,
                "整理 API 参考文档的示例代码",
                "D:\\Work\\docs-site",
                "claude",
                now - 9.0 * 86400.0,
                "partial",
                "示例代码已整理 14 / 18 篇，剩余 4 篇依赖尚未发布的接口，已在文末标注。",
                2,
            ),
            simple(
                31,
                "汇总本周读书笔记",
                "C:\\Users\\me\\notes",
                "deepseek",
                now - 21.0 * 86400.0,
                "done",
                "已按主题汇总 6 篇笔记，并在每节末尾列出原始出处。",
                1,
            ),
        ];
        Self {
            tickets,
            started: Instant::now(),
            live_added: 0,
        }
    }

    /// Appends a progress row every few seconds to the running ticket so the
    /// incremental path and auto-scroll can be observed.
    fn tick(&mut self) {
        let due = (self.started.elapsed().as_secs() / 4) as usize;
        let labels = [
            "调用 Grep",
            "Grep 已返回",
            "调用 Read",
            "Read 已返回",
            "调用 Bash",
            "Bash 已返回",
        ];
        while self.live_added < due.min(40) {
            let t = &mut self.tickets[0];
            let seq = t.events.last().map(|e| e.seq).unwrap_or(0) + 1;
            let label = labels[self.live_added % labels.len()];
            let now = crate::labels::now_secs();
            t.events.push(EventView {
                seq,
                kind: "activity".into(),
                ts: now,
                job: Some(101),
                turn: Some(3),
                agent: Some("claude".into()),
                payload: json!({"label": label, "category": "tool",
                    "state": if label.contains("调用") {"running"} else {"completed"}}),
            });
            t.native.latest = Some(LatestActivity {
                job: 101,
                turn: 3,
                label: label.into(),
                ts: now,
            });
            self.live_added += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn simple(
    id: i64,
    goal: &str,
    cwd: &str,
    lead: &str,
    created: f64,
    state: &str,
    reply: &str,
    peers: i64,
) -> Ticket {
    let root = id * 100;
    let r = match lead {
        "codex" => route("openai", "gpt-5.2-codex", "cli"),
        "claude" => route("anthropic", "claude-opus-4-5", "cli"),
        "deepseek" => route("deepseek", "deepseek-v3.2", "cli"),
        _ => route("google", "gemini-3-pro", "desktop"),
    };
    let mut jobs = vec![job(
        root,
        lead,
        None,
        "",
        state,
        1,
        goal,
        (!reply.is_empty()).then_some(reply),
        r,
    )];
    let agents = ["codex", "claude", "deepseek", "antigravity"];
    for i in 1..peers {
        let agent = agents[(i as usize) % 4];
        let st = match (state, i % 7) {
            ("waiting", 0) => "running",
            ("waiting", 3) => "queued",
            ("failed", _) => "failed",
            _ => "done",
        };
        jobs.push(job(
            root + i,
            agent,
            Some(if i > 4 && i % 3 == 0 {
                root + i - 2
            } else {
                root
            }),
            &format!("协作者 {i} · 验证分片 {}", (i % 12) + 1),
            st,
            1,
            &format!("对分片 {} 重放导出请求并记录耗时分布。", (i % 12) + 1),
            (st == "done").then_some("分片内 200 次请求均在 1.2 秒内完成，未复现超时。"),
            route("openai", "gpt-5.2-codex", "cli"),
        ));
    }
    let mut log = Log::new(id * 1000, created);
    log.push(0.0, "user", root, 1, None, json!(goal));
    log.act(
        4.0,
        root,
        1,
        lead,
        json!({"label":"调度器已接单","category":"run","state":"running"}),
    );
    log.act(
        3.0,
        root,
        1,
        lead,
        json!({"label":"已选择执行端和模型","category":"run","state":"running"}),
    );
    match state {
        "cancelled" => {
            log.act(
                40.0,
                root,
                1,
                lead,
                json!({"label":"调用 Glob","category":"tool","state":"running"}),
            );
            log.push(
                30.0,
                "interrupted",
                root,
                1,
                None,
                json!("执行端进程意外退出，等待处理"),
            );
            log.push(60.0, "cancelled", root, 1, None, json!("用户停止了工单"));
        }
        "failed" => {
            log.push(
                80.0,
                "send",
                root,
                1,
                None,
                json!({"to":"claude","role":"编译修复","body":"修复 tokio 升级后的编译错误"}),
            );
            log.push(120.0, "failed", root + 1, 1, None,
                json!("cargo build 失败：error[E0599]: no method named `block_on` found for struct `Handle`"));
            log.push(
                20.0,
                "failed",
                root,
                1,
                None,
                json!("协作任务全部失败，工单终止。"),
            );
        }
        "queued" => {}
        _ => {
            if !reply.is_empty() {
                log.act(
                    30.0,
                    root,
                    1,
                    lead,
                    json!({"label":"调用 Bash","category":"tool","state":"running"}),
                );
                log.act(
                    8.0,
                    root,
                    1,
                    lead,
                    json!({"label":"Bash 已返回","category":"tool","state":"completed"}),
                );
                log.act(1.0, root, 1, lead,
                    json!({"label":"进展汇报 · 已核对三类进展记录，任务来源与时间都可追溯。","category":"report","state":"reported"}));
                log.push(20.0, "reply", root, 1, None, json!({"text": reply}));
            }
        }
    }
    let state_owned = state.to_string();
    Ticket {
        detail: detail(
            id,
            goal,
            cwd,
            lead,
            created,
            if id == 41 { "ask" } else { "readonly" },
            &state_owned,
        ),
        jobs,
        events: log.events,
        approvals: vec![],
        run: None,
        plan: vec![],
        native: NativeActivity::default(),
    }
}

fn main_ticket(now: f64) -> Ticket {
    let created = now - 52.0 * 60.0;
    let goal = "把桌面端迁移到纯 Rust：先做一个只读观察界面，实时显示全部工单与进展，\n界面保持现有品牌风格，并与正在运行的调度器兼容。";
    let mut log = Log::new(4200, created);
    let (lead, root) = ("claude", 101);
    log.push(0.0, "user", root, 1, None, json!(goal));
    log.act(
        2.0,
        root,
        1,
        lead,
        json!({"label":"调度器已接单","category":"run","state":"running"}),
    );
    log.act(
        1.0,
        root,
        1,
        lead,
        json!({"label":"已选择执行端和模型","category":"run","state":"running"}),
    );
    log.act(
        3.0,
        root,
        1,
        lead,
        json!({"label":"Claude Code 已启动","category":"run","state":"running"}),
    );
    log.act(
        6.0,
        root,
        1,
        lead,
        json!({"label":"调用 Read","category":"tool","state":"running","tool":"Read"}),
    );
    log.act(
        2.0,
        root,
        1,
        lead,
        json!({"label":"Read 已返回","category":"tool","state":"completed","tool":"Read"}),
    );
    log.act(4.0, root, 1, lead, json!({"label":"调用 Grep ×3","category":"tool","state":"running","tool":"Grep",
        "steps":[{"tool":"Grep","state":"completed"},{"tool":"Grep","state":"completed"},{"tool":"Grep","state":"running"}]}));
    log.act(
        5.0,
        root,
        1,
        lead,
        json!({"label":"Explore · 梳理桌面端结构","category":"task","state":"completed",
        "taskId":"toolu_01JZ","title":"梳理桌面端结构"}),
    );
    log.push(40.0, "reply", root, 1, None, json!({"text": LEAD_REPLY_1}));
    log.push(
        1.0,
        "send",
        root,
        1,
        None,
        json!({"to":"codex","child":102,"role":"代码审查","body":"审查 sa-store 的只读查询"}),
    );
    log.push(
        1.0,
        "send",
        root,
        1,
        None,
        json!({"to":"deepseek","child":103,"role":"性能分析","body":"测量全量刷新耗时"}),
    );
    log.push(
        1.0,
        "send",
        root,
        1,
        None,
        json!({"to":"antigravity","child":104,"role":"截图核验","body":"核对界面截图"}),
    );
    log.push(
        1.0,
        "send",
        root,
        1,
        None,
        json!({"to":"codex","child":105,"role":"单元测试","body":"运行 cargo test"}),
    );
    log.push(
        2.0,
        "auto.goal",
        root,
        1,
        None,
        json!({"objective":"桌面端以纯 Rust 只读观察全部工单，并与现有调度保持兼容"}),
    );
    log.act(
        30.0,
        102,
        1,
        "codex",
        json!({"label":"已开始执行本轮","category":"run","state":"running"}),
    );
    log.act(
        4.0,
        102,
        1,
        "codex",
        json!({"label":"调用 Bash","category":"tool","state":"running","tool":"Bash"}),
    );
    log.act(
        9.0,
        102,
        1,
        "codex",
        json!({"label":"Bash 已返回","category":"tool","state":"completed","tool":"Bash"}),
    );
    log.act(
        3.0,
        103,
        1,
        "deepseek",
        json!({"label":"已开始执行本轮","category":"run","state":"running"}),
    );
    log.act(
        2.0,
        102,
        1,
        "codex",
        json!({"label":"调用 apply_patch","category":"tool","state":"running"}),
    );
    log.act(
        6.0,
        102,
        1,
        "codex",
        json!({"label":"apply_patch 已返回","category":"tool","state":"completed"}),
    );
    log.act(1.0, 102, 1, "codex", json!({"label":"进展汇报 · 查询语义已核对，发现 2 处边界问题","category":"report","state":"reported"}));
    log.push(30.0, "reply", 102, 1, None, json!({"text": PEER_REPLY}));
    log.push(20.0, "failed", 105, 1, None, json!("cargo test 失败：3 个用例未通过\n\nthread 'feed::tests::groups_progress' panicked at src/feed.rs:212:9"));
    log.push(
        5.0,
        "approval.request",
        102,
        1,
        None,
        json!({"approval":7,"action":{"tool":"Bash","command":"cargo clean -p sa-desktop"}}),
    );
    log.push(
        10.0,
        "mcp.result",
        root,
        1,
        None,
        json!({"id":"m1","server":"github","tool":"search_issues","state":"done"}),
    );
    log.push(
        4.0,
        "note",
        root,
        1,
        None,
        json!({"job":103,"body":"请同时记录 Release 构建下的帧耗时"}),
    );
    log.push(
        30.0,
        "user.follow",
        root,
        2,
        None,
        json!("顺便确认暗色主题下的对比度，以及窄窗口时检查器的表现。"),
    );
    log.act(
        3.0,
        root,
        2,
        lead,
        json!({"label":"已追加指令","category":"run","state":"running"}),
    );
    for (i, (label, state)) in [
        ("调用 Read", "running"),
        ("Read 已返回", "completed"),
        ("调用 Edit", "running"),
        ("Edit 已返回", "completed"),
        ("调用 Bash", "running"),
        ("Bash 返回错误", "failed"),
        ("调用 Bash", "running"),
        ("Bash 已返回", "completed"),
    ]
    .iter()
    .enumerate()
    {
        log.act(
            5.0 + i as f64,
            root,
            2,
            lead,
            json!({"label":label,"category":"tool","state":state}),
        );
    }
    log.act(
        4.0,
        root,
        2,
        lead,
        json!({"label":"general-purpose · 核对暗色主题对比度","category":"task","state":"running",
        "taskId":"toolu_02QX","title":"核对暗色主题对比度"}),
    );
    log.push(50.0, "reply", root, 2, None, json!({"text": LEAD_REPLY_2}));
    log.push(
        2.0,
        "auto.learn",
        root,
        2,
        None,
        json!({"lesson":"egui 的标签在右对齐布局中不会自动收缩，气泡需要先排版再定位"}),
    );
    log.push(
        2.0,
        "auto.milestone",
        root,
        2,
        None,
        json!({"status":"done","summary":"主体界面完成并通过截图核验"}),
    );
    log.act(
        20.0,
        103,
        1,
        "deepseek",
        json!({"label":"调用 Bash","category":"tool","state":"running","tool":"Bash"}),
    );
    log.act(
        6.0,
        106,
        1,
        "claude",
        json!({"label":"已开始执行本轮","category":"run","state":"running"}),
    );
    log.act(
        3.0,
        root,
        3,
        lead,
        json!({"label":"调度器已接单","category":"run","state":"running"}),
    );
    log.act(
        4.0,
        root,
        3,
        lead,
        json!({"label":"调用 Grep","category":"tool","state":"running","tool":"Grep"}),
    );
    log.push(
        2.0,
        "delta",
        root,
        3,
        None,
        json!("正在核对窄窗口布局：检查器打开时侧栏会自动收起，"),
    );
    log.push(
        1.0,
        "delta",
        root,
        3,
        None,
        json!("对话列保持不少于 520 像素……"),
    );

    let jobs = vec![
        job(
            101,
            "claude",
            None,
            "",
            "running",
            3,
            goal,
            None,
            route("anthropic", "claude-opus-4-5", "cli"),
        ),
        job(
            102,
            "codex",
            Some(101),
            "代码审查",
            "done",
            1,
            "审查 sa-store 的只读查询是否与 studio.cjs 等价",
            Some(PEER_REPLY),
            route("openai", "gpt-5.2-codex", "cli"),
        ),
        job(
            103,
            "deepseek",
            Some(101),
            "性能分析",
            "running",
            1,
            "测量 180 条事件全量刷新的耗时",
            None,
            route("deepseek", "deepseek-v3.2", "cli"),
        ),
        job(
            104,
            "antigravity",
            Some(101),
            "截图核验",
            "queued",
            1,
            "在 1440×900 与 1000×760 下核对界面截图",
            None,
            route("google", "gemini-3-pro", "desktop"),
        ),
        job(
            105,
            "codex",
            Some(101),
            "单元测试",
            "failed",
            1,
            "运行 cargo test -p sa-desktop 并汇报失败用例",
            Some("cargo test 失败：3 个用例未通过"),
            route("openai", "gpt-5.2-codex", "cli"),
        ),
        job(
            106,
            "claude",
            Some(103),
            "帧耗时采样",
            "running",
            1,
            "在 Release 构建下采样 600 帧并给出 P95 帧耗时",
            None,
            route("anthropic", "claude-sonnet-4-5", "cli"),
        ),
    ];
    let t0 = now - 600.0;
    let native = NativeActivity {
        tasks: vec![
            NativeTask {
                id: "toolu_01JZ".into(),
                job: 101,
                turn: 1,
                label: "Explore · 梳理桌面端结构".into(),
                state: "completed".into(),
                last_tool: "Grep".into(),
                ts: created + 20.0,
                title: Some("梳理桌面端结构".into()),
                tool_uses: Some(14),
            },
            NativeTask {
                id: "toolu_02QX".into(),
                job: 101,
                turn: 2,
                label: "general-purpose · 核对暗色主题对比度".into(),
                state: "running".into(),
                last_tool: "Read".into(),
                ts: t0,
                title: Some("核对暗色主题对比度".into()),
                tool_uses: Some(6),
            },
            NativeTask {
                id: "toolu_03MB".into(),
                job: 106,
                turn: 1,
                label: "Explore · 查找帧计时入口".into(),
                state: "failed".into(),
                last_tool: "Bash".into(),
                ts: t0 + 60.0,
                title: None,
                tool_uses: Some(3),
            },
        ],
        latest: Some(LatestActivity {
            job: 101,
            turn: 3,
            label: "调用 Grep · 搜索 ScrollArea".into(),
            ts: now - 12.0,
        }),
    };
    let mut d = detail(
        42,
        goal,
        "E:\\Projects\\Super Agent",
        lead,
        created,
        "auto",
        "running",
    );
    d.objective = Some("桌面端以纯 Rust 只读观察全部工单，并与现有调度保持兼容".into());
    d.agent_routes =
        json!({"claude":{"provider":"anthropic","model":"claude-opus-4-5","mode":"cli"}});
    Ticket {
        detail: d,
        jobs,
        events: log.events,
        approvals: vec![Approval {
            id: 7,
            job: 102,
            action: json!({"tool":"Bash","command":"cargo clean -p sa-desktop","cwd":"E:\\Projects\\Super Agent",
                "reason":"清理旧的构建产物后重新测量编译时间"}),
        }],
        run: Some(ContinuousRun {
            phase: "work".into(),
            milestone: 2,
            learning: "egui 的标签在右对齐布局中不会自动收缩，气泡需要先排版再定位".into(),
        }),
        plan: vec![
            PlanItem {
                id: 1,
                body: "梳理现有 Electron 界面与 IPC 契约".into(),
                done: true,
            },
            PlanItem {
                id: 2,
                body: "实现 sa-store 只读查询".into(),
                done: true,
            },
            PlanItem {
                id: 3,
                body: "完成 sa-desktop 只读观察界面".into(),
                done: false,
            },
            PlanItem {
                id: 4,
                body: "截图核验：浅色、暗色、窄窗口".into(),
                done: false,
            },
            PlanItem {
                id: 5,
                body: "接入调度与写入（下一个里程碑）".into(),
                done: false,
            },
        ],
        native,
    }
}

impl DataSource for DemoSource {
    fn label(&self) -> String {
        "演示数据 · 不读取真实工单".into()
    }

    fn history(&mut self, q: &str) -> Result<Vec<TicketSummary>> {
        self.tick();
        let q = q.to_lowercase();
        Ok(self
            .tickets
            .iter()
            .filter(|t| {
                q.is_empty()
                    || t.detail.goal.to_lowercase().contains(&q)
                    || t.detail.cwd.to_lowercase().contains(&q)
            })
            .map(|t| TicketSummary {
                id: t.detail.id,
                goal: t.detail.goal.chars().take(500).collect(),
                cwd: t.detail.cwd.clone(),
                lead: t.detail.lead.clone(),
                created: t.detail.created,
                state: t.detail.state.clone(),
                title: None,
                pinned: false,
                archived: false,
            })
            .collect())
    }

    fn snapshot(&mut self, ticket: i64, after: i64) -> Result<Snapshot> {
        self.tick();
        let Some(t) = self.tickets.iter().find(|t| t.detail.id == ticket) else {
            bail!("工单不存在")
        };
        let end = t.events.last().map(|e| e.seq).unwrap_or(0);
        let mut events: Vec<EventView> =
            t.events.iter().filter(|e| e.seq > after).cloned().collect();
        if after == 0 && events.len() > 180 {
            events.drain(..events.len() - 180);
        } else {
            events.truncate(180);
        }
        let cursor = if after > 0 && events.len() == 180 {
            events.last().map(|e| e.seq).unwrap_or(end)
        } else {
            end
        };
        let mut jobs = t.jobs.clone();
        jobs.sort_by_key(|j| {
            (
                j.parent.is_some(),
                !crate::labels::is_active(&j.state),
                -j.id,
            )
        });
        let total_jobs = jobs.len() as i64;
        jobs.truncate(80);
        Ok(Snapshot {
            ticket: t.detail.clone(),
            running_peers: t
                .jobs
                .iter()
                .filter(|j| j.parent.is_some() && j.state == "running")
                .count() as i64,
            jobs,
            total_jobs,
            native_activity: t.native.clone(),
            usage: Default::default(),
            events,
            cursor: cursor.max(after),
            approvals: t.approvals.clone(),
            run: t.run.clone(),
            plan: t.plan.clone(),
        })
    }

    fn jobs_page(&mut self, ticket: i64, before: Option<i64>) -> Result<JobsPage> {
        let Some(t) = self.tickets.iter().find(|t| t.detail.id == ticket) else {
            bail!("工单不存在")
        };
        let mut rows: Vec<JobView> = t
            .jobs
            .iter()
            .filter(|j| before.is_none_or(|b| j.id < b))
            .cloned()
            .collect();
        rows.sort_by_key(|j| -j.id);
        let more = rows.len() > 80;
        rows.truncate(80);
        Ok(JobsPage {
            next: rows.last().map(|j| j.id),
            jobs: rows,
            more,
        })
    }
}
