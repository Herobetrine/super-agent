//! Port of `runtime/progress.cjs`: the derived per-ticket progress projection.
//!
//! These rows retain only operational labels. Agent text, arguments,
//! credentials, thinking deltas and command output stay out of the projection.
//!
//! JS keeps the derived rows in a per-connection TEMP table; this port keeps
//! them in memory per `Store`, which is equivalent (both are per-connection,
//! non-persistent, keyed by `seq`) and never touches the database file.

use crate::activity::Activity;
use crate::model::EventView;
use crate::native_activity::{NativeRow, NativeState};
use std::collections::BTreeMap;
use std::ops::Bound;

/// Journal kinds shown verbatim by `Studio.snapshot` (`VISIBLE` in studio.cjs).
pub(crate) const VISIBLE: &[&str] = &[
    "user",
    "user.follow",
    "delta",
    "reply",
    "send",
    "role",
    "note",
    "note.rejected",
    "failed",
    "cancelled",
    "interrupted",
    "auto.goal",
    "auto.next",
    "auto.learn",
    "auto.milestone",
    "auto.discovered",
    "auto.exhausted",
    "auto.retry",
    "auto.backoff",
    "auto.finish",
    "auto.recover",
    "approval.request",
    "approval.decision",
    "mcp.result",
    // Rust-port resilience events (spec §9.2 G).
    "turn.stalled",
    "turn.retry_scheduled",
    "turn.route_wait",
    "turn.gave_up",
    "route.rotated",
    "harness.health",
];

/// `[label, category, state = 'running']` per journal kind.
fn stage(kind: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match kind {
        "user" => ("已提交目标", "run", "running"),
        "user.follow" => ("已追加指令", "run", "running"),
        "turn.admitted" => ("调度器已接单", "run", "running"),
        "route.requested" => ("已选择执行端和模型", "run", "running"),
        "input" => ("已准备本轮上下文", "run", "running"),
        "running" => ("已开始执行本轮", "run", "running"),
        "adapter.permission" => ("已应用执行权限", "run", "running"),
        "adapter.route" => ("已绑定执行路由", "run", "running"),
        "files" => ("附件快照已就绪", "run", "running"),
        "send" => ("已派发协作任务", "task", "running"),
        "reply" => ("本轮已返回答复", "run", "completed"),
        "role" => ("已更新任务分工", "task", "running"),
        "note" => ("已投递定向留言", "task", "running"),
        "note.accepted" => ("执行端已接收留言", "task", "completed"),
        "note.rejected" => ("留言投递被拒绝", "task", "failed"),
        "retry" => ("任务已安排重试", "run", "running"),
        "approval.request" => ("等待操作审批", "approval", "running"),
        "approval.decision" => ("审批已处理", "approval", "completed"),
        "approval.unattended_decline" => ("无人值守审批已拒绝", "approval", "failed"),
        "auto.goal" => ("已更新阶段目标", "task", "running"),
        "auto.next" => ("已安排下一步", "task", "running"),
        "auto.learn" => ("已记录工作经验", "task", "completed"),
        "auto.milestone" => ("已记录阶段结果", "task", "completed"),
        "auto.discovered" => ("已发现新的工作", "task", "running"),
        "auto.exhausted" => ("已提交穷尽检查", "task", "completed"),
        "auto.finish" => ("已提交阶段结论", "task", "completed"),
        "auto.recover" => ("正在复核中断状态", "task", "running"),
        "auto.retry" => ("已安排延后重试", "task", "running"),
        "auto.backoff" => ("等待退避后继续", "task", "running"),
        "auto.protocol" => ("控制指令需要修正", "task", "running"),
        "routes.changed" => ("已更新工单模型绑定", "run", "running"),
        "failed" => ("任务执行失败", "run", "failed"),
        "cancelled" => ("任务已取消", "run", "stopped"),
        "partial" => ("任务部分完成", "task", "running"),
        "interrupted" => ("任务中断，等待处理", "run", "stopped"),
        "draining" => ("等待现有子任务结束", "run", "running"),
        "mcp.result" => ("已收到 MCP 工具回执", "tool", "completed"),
        "plan.add" => ("已添加计划项", "task", "running"),
        "plan.done" => ("已勾选计划项", "task", "completed"),
        "plan.reopen" => ("已重新打开计划项", "task", "running"),
        _ => return None,
    })
}

/// A non-native journal row. `summary`/`status` are the SQL `json_extract`
/// results, kept only when SQLite returned TEXT (JS `typeof === 'string'`).
pub(crate) struct PlatformRow<'a> {
    pub kind: &'a str,
    pub summary: Option<&'a str>,
    pub status: Option<&'a str>,
}

/// `platformActivity(row)`
pub(crate) fn platform_activity(row: &PlatformRow) -> Option<Activity> {
    if row.kind == "job.progress"
        && let Some(summary) = row.summary
    {
        return Some(Activity::new(
            format!("进展汇报 · {summary}"),
            "report",
            "reported",
        ));
    }
    if row.kind == "mcp.started" {
        return Some(Activity::new(
            "正在调用已授权的 MCP 工具",
            "tool",
            "running",
        ));
    }
    let (mut label, category, mut state) = stage(row.kind)?;
    if row.status == Some("deny") {
        label = "本次操作未获批准";
        state = "stopped";
    }
    if row.kind == "mcp.result" && row.status != Some("done") {
        label = "MCP 工具调用未成功";
        state = "failed";
    }
    if matches!(row.kind, "auto.milestone" | "auto.exhausted") && row.status == Some("blocked") {
        label = "已记录阻塞原因";
        state = "stopped";
    }
    Some(Activity::new(label, category, state))
}

/// Row of the `progress_view` projection.
pub(crate) struct ProgressRow {
    pub ts: f64,
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub agent: Option<String>,
    /// The original journal kind is one of `VISIBLE`.
    pub visible_original: bool,
    pub item: Activity,
}

/// Per-ticket cache entry (`seen.get(ticket)`).
#[derive(Default)]
pub(crate) struct Entry {
    /// Highest journal seq already projected.
    pub seq: i64,
    pub native: NativeState,
    pub rows: BTreeMap<i64, ProgressRow>,
}

/// Raw row fetched by the progress scan.
pub(crate) struct ScanRow {
    pub seq: i64,
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub ts: f64,
    pub kind: String,
    pub agent: Option<String>,
    pub payload: Option<String>,
    pub summary: Option<String>,
    pub status: Option<String>,
}

impl Entry {
    /// Project one scanned row (`build` loop body).
    pub fn absorb(&mut self, row: ScanRow) {
        let item = if row.kind == "native" {
            self.native.apply(&NativeRow {
                seq: row.seq,
                job: row.job,
                turn: row.turn,
                ts: row.ts,
                kind: &row.kind,
                payload: row.payload.as_deref(),
            })
        } else {
            platform_activity(&PlatformRow {
                kind: &row.kind,
                summary: row.summary.as_deref(),
                status: row.status.as_deref(),
            })
        };
        if let Some(item) = item {
            self.rows.insert(
                row.seq,
                ProgressRow {
                    ts: row.ts,
                    job: row.job,
                    turn: row.turn,
                    agent: row.agent,
                    visible_original: VISIBLE.contains(&row.kind.as_str()),
                    item,
                },
            );
        }
        self.seq = row.seq;
    }

    /// `read(ticket, after, end, reverse, limit, except)` where `except` is
    /// either `VISIBLE` (`except_visible`) or empty.
    pub fn read(
        &self,
        after: i64,
        end: i64,
        reverse: bool,
        limit: usize,
        except_visible: bool,
    ) -> Vec<EventView> {
        if after >= end || limit == 0 {
            return Vec::new();
        }
        let range = self
            .rows
            .range((Bound::Excluded(after), Bound::Included(end)));
        let keep = |(_, r): &(&i64, &ProgressRow)| !(except_visible && r.visible_original);
        let view = |(seq, r): (&i64, &ProgressRow)| EventView {
            seq: *seq,
            kind: "activity".to_owned(),
            ts: r.ts,
            job: r.job,
            turn: r.turn,
            agent: r.agent.clone(),
            payload: r.item.to_value(),
        };
        if reverse {
            range.rev().filter(keep).take(limit).map(view).collect()
        } else {
            range.filter(keep).take(limit).map(view).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(kind: &str, status: Option<&str>) -> Activity {
        platform_activity(&PlatformRow {
            kind,
            summary: None,
            status,
        })
        .unwrap()
    }

    #[test]
    fn stage_labels_and_overrides() {
        assert_eq!(act("plan.add", None).label, "已添加计划项");
        assert_eq!(act("reply", None).state, "completed");
        let deny = act("approval.decision", Some("deny"));
        assert_eq!(
            (deny.label.as_str(), deny.state),
            ("本次操作未获批准", "stopped")
        );
        let mcp = act("mcp.result", Some("error"));
        assert_eq!(
            (mcp.label.as_str(), mcp.state),
            ("MCP 工具调用未成功", "failed")
        );
        assert_eq!(act("mcp.result", Some("done")).label, "已收到 MCP 工具回执");
        let blocked = act("auto.milestone", Some("blocked"));
        assert_eq!(
            (blocked.label.as_str(), blocked.state),
            ("已记录阻塞原因", "stopped")
        );
        let report = platform_activity(&PlatformRow {
            kind: "job.progress",
            summary: Some("半程"),
            status: None,
        })
        .unwrap();
        assert_eq!(
            (report.label.as_str(), report.category, report.state),
            ("进展汇报 · 半程", "report", "reported")
        );
        assert_eq!(
            platform_activity(&PlatformRow {
                kind: "job.progress",
                summary: None,
                status: None
            }),
            None
        );
        assert_eq!(act("mcp.started", None).category, "tool");
        assert_eq!(
            platform_activity(&PlatformRow {
                kind: "delta",
                summary: None,
                status: None
            }),
            None
        );
    }
}
