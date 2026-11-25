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
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;

/// Journal kinds shown verbatim by `Studio.snapshot` (`VISIBLE` in studio.cjs).
pub(crate) const VISIBLE: &[&str] = &[
    "user",
    "user.follow",
    "delta",
    "reply",
    "send",
    "ask",
    "answer",
    "ask.withdrawn",
    "report",
    "continued",
    "skipped",
    "check.started",
    "check.result",
    "check.bounced",
    "auto.stalled",
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
    "turn.quota_wait",
    "turn.deferred",
    "turn.gave_up",
    "turn.loop_warning",
    "turn.loop_stopped",
    "workspace.result",
    "workspace.error",
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
        "turn.loop_warning" => ("重复操作，建议改变方法", "run", "running"),
        "turn.loop_stopped" => ("已停止重复空转", "run", "failed"),
        "workspace.result" => ("工作区验证或合入已返回", "task", "completed"),
        "workspace.error" => ("工作区操作未完成", "task", "failed"),
        "running" => ("已开始执行本轮", "run", "running"),
        "adapter.permission" => ("已应用执行权限", "run", "running"),
        "adapter.route" => ("已绑定执行路由", "run", "running"),
        "files" => ("附件快照已就绪", "run", "running"),
        "send" => ("已派发协作任务", "task", "running"),
        "ask" => ("等待你回答问题", "task", "waiting"),
        "answer" => ("已收到问题回答", "task", "completed"),
        "ask.withdrawn" => ("未回答的问题已撤回", "task", "stopped"),
        "report" => ("已提交任务结论", "task", "completed"),
        "continued" => ("协作任务已安排继续", "task", "running"),
        "skipped" => ("前置任务未完成，已跳过任务", "task", "stopped"),
        "check.started" => ("正在运行完成检查", "task", "running"),
        "check.result" => ("完成检查已通过", "task", "completed"),
        "check.bounced" => ("完成检查未通过，已退回修复", "task", "running"),
        "auto.stalled" => ("主控重复下一步，等待你决定", "task", "waiting"),
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
        "protocol.rejected" => ("控制指令被拒绝，该指令未执行", "task", "running"),
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
    if row.kind == "check.result" && row.status != Some("done") {
        label = "完成检查未通过";
        state = "failed";
    }
    if row.kind == "report" {
        match row.status {
            Some("partial") => {
                label = "任务部分完成";
                state = "stopped";
            }
            Some("blocked") => {
                label = "任务受阻，等待处理";
                state = "stopped";
            }
            _ => {}
        }
    }
    Some(Activity::new(label, category, state))
}

/// Quota metadata is projected to fixed operational labels only.
fn quota_activity(kind: &str, payload: Option<&str>) -> Option<Activity> {
    if !matches!(kind, "turn.quota_wait" | "turn.deferred") {
        return None;
    }
    let payload = payload.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
    let reason = payload.as_ref().and_then(|p| p.get("reason")).and_then(serde_json::Value::as_str)
        .unwrap_or(if kind == "turn.quota_wait" { "quota_window" } else { "" });
    let label = match reason {
        "quota_window" => "等待额度重置",
        "quota_reserve" => "为主控保留额度",
        _ => return None,
    };
    Some(Activity::new(label, "run", "waiting"))
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
    // Absorbed in seq order by PROGRESS_SCAN. Only supplemental activity
    // belongs in the feed; visible originals already have an SQL row.
    feed_seqs: Vec<i64>,
    feed_by_job: HashMap<Option<i64>, Vec<i64>>,
    #[cfg(test)]
    pub(crate) feed_turn_checks: std::cell::Cell<usize>,
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
            quota_activity(&row.kind, row.payload.as_deref()).or_else(|| {
                platform_activity(&PlatformRow {
                    kind: &row.kind,
                    summary: row.summary.as_deref(),
                    status: row.status.as_deref(),
                })
            })
        };
        if let Some(item) = item {
            let visible_original = VISIBLE.contains(&row.kind.as_str());
            if !visible_original {
                self.feed_seqs.push(row.seq);
                self.feed_by_job.entry(row.job).or_default().push(row.seq);
            }
            self.rows.insert(
                row.seq,
                ProgressRow {
                    ts: row.ts,
                    job: row.job,
                    turn: row.turn,
                    agent: row.agent,
                    visible_original,
                    item,
                },
            );
        }
        self.seq = row.seq;
    }

    pub fn newer_feed(&self, after: i64, end: i64, resumed: i64) -> &[i64] {
        let low = after.max(resumed.saturating_sub(1));
        let seqs = &self.feed_seqs;
        crate::feed::page_slice(
            &seqs[seqs.partition_point(|s| *s <= low)..seqs.partition_point(|s| *s <= end)],
            after,
        )
    }

    pub fn append_history(
        &self,
        ids: &mut Vec<i64>,
        after: i64,
        fold: &crate::feed::Fold,
        job: Option<i64>,
        round: Option<(&str, i64)>,
    ) {
        let Some(seqs) = self.feed_by_job.get(&job) else {
            return;
        };
        let seqs = &seqs
            [seqs.partition_point(|s| *s <= after)..seqs.partition_point(|s| *s < fold.resumed)];
        let shown = |s: &&i64| {
            if job.is_none() {
                return true;
            }
            #[cfg(test)]
            self.feed_turn_checks.set(self.feed_turn_checks.get() + 1);
            !crate::model::folded_from_feed(
                "activity",
                **s,
                job,
                self.rows[*s].turn,
                fold.root,
                fold.resumed,
                round,
            )
        };
        if after == 0 {
            ids.extend(
                seqs.iter()
                    .rev()
                    .filter(shown)
                    .take(crate::feed::PAGE)
                    .copied(),
            );
        } else {
            ids.extend(seqs.iter().filter(shown).take(crate::feed::PAGE).copied());
        }
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
        shown: &dyn Fn(i64, Option<i64>, Option<i64>) -> bool,
    ) -> Vec<EventView> {
        if after >= end || limit == 0 {
            return Vec::new();
        }
        let range = self
            .rows
            .range((Bound::Excluded(after), Bound::Included(end)));
        let keep = |(seq, r): &(&i64, &ProgressRow)| {
            !(except_visible && r.visible_original) && shown(**seq, r.job, r.turn)
        };
        let view = |(seq, r): (&i64, &ProgressRow)| r.view(*seq);
        if reverse {
            range.rev().filter(keep).take(limit).map(view).collect()
        } else {
            range.filter(keep).take(limit).map(view).collect()
        }
    }
}

impl ProgressRow {
    pub(crate) fn view(&self, seq: i64) -> EventView {
        EventView {
            seq,
            kind: "activity".to_owned(),
            ts: self.ts,
            job: self.job,
            turn: self.turn,
            agent: self.agent.clone(),
            payload: self.item.to_value(),
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
    fn quota_waits_are_waiting_progress_and_bookkeeping_stays_hidden() {
        for kind in ["turn.quota_wait", "turn.deferred"] {
            assert!(VISIBLE.contains(&kind));
            for (reason, label) in [
                ("quota_window", "等待额度重置"),
                ("quota_reserve", "为主控保留额度"),
            ] {
                let payload = serde_json::json!({"reason":reason,"until":1_800_000_000,"route":{"provider":"PRIVATE"}}).to_string();
                let item = quota_activity(kind, Some(&payload)).unwrap();
                assert_eq!((item.label.as_str(), item.category, item.state), (label, "run", "waiting"));
                assert!(!item.to_value().to_string().contains("PRIVATE"));
            }
        }
        assert_eq!(quota_activity("turn.deferred", Some(r#"{"reason":"circuit_open"}"#)), None);
        assert_eq!(quota_activity("turn.quota_wait", None).unwrap().label, "等待额度重置");
        assert!(!VISIBLE.contains(&"quota.window"));
        assert_eq!(platform_activity(&PlatformRow { kind: "quota.window", summary: None, status: None }), None);
        let mut entry = Entry::default();
        for (index, kind, payload) in [
            (1, "quota.window", r#"{"used":0.92,"provider":"PRIVATE"}"#),
            (2, "native", r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"usedPercent":92}}}"#),
            (3, "turn.quota_wait", r#"{"reason":"quota_reserve","until":1800000000}"#),
        ] {
            entry.absorb(ScanRow { seq: index, job: Some(1), turn: Some(1), ts: index as f64, kind: kind.into(), agent: Some("codex".into()), payload: Some(payload.into()), summary: None, status: None });
        }
        let rows = entry.read(0, 3, false, 10, false, &|_, _, _| true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload["label"], "为主控保留额度");
        assert_eq!(rows[0].payload["state"], "waiting");
        assert!(entry.read(0, 3, false, 10, true, &|_, _, _| true).is_empty());
    }

    #[test]
    fn stage_labels_and_overrides() {
        assert_eq!(act("plan.add", None).label, "已添加计划项");
        assert_eq!(act("reply", None).state, "completed");
        let rejection = act("protocol.rejected", None);
        assert_eq!(
            (rejection.label.as_str(), rejection.category, rejection.state),
            ("控制指令被拒绝，该指令未执行", "task", "running")
        );
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

    #[test]
    fn completion_checks_and_stalls_have_fixed_progress_labels() {
        for (kind, status, label, state) in [
            ("check.started", None, "正在运行完成检查", "running"),
            ("check.result", Some("done"), "完成检查已通过", "completed"),
            ("check.result", Some("failed"), "完成检查未通过", "failed"),
            ("check.bounced", None, "完成检查未通过，已退回修复", "running"),
            ("auto.stalled", None, "主控重复下一步，等待你决定", "waiting"),
        ] {
            assert!(VISIBLE.contains(&kind));
            let item = act(kind, status);
            assert_eq!((item.label.as_str(), item.state), (label, state));
        }
    }

    #[test]
    fn collaboration_protocol_events_have_fixed_progress_labels() {
        for (kind, label, state) in [
            ("ask", "等待你回答问题", "waiting"),
            ("answer", "已收到问题回答", "completed"),
            ("ask.withdrawn", "未回答的问题已撤回", "stopped"),
            ("report", "已提交任务结论", "completed"),
            ("continued", "协作任务已安排继续", "running"),
            ("skipped", "前置任务未完成，已跳过任务", "stopped"),
        ] {
            assert!(VISIBLE.contains(&kind));
            let item = act(kind, None);
            assert_eq!((item.label.as_str(), item.state), (label, state));
        }
        assert_eq!(act("report", Some("partial")).label, "任务部分完成");
        assert_eq!(act("report", Some("blocked")).state, "stopped");
    }
}
