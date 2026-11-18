//! Role-aware context selection (architecture-evolution §9.2, policy `roles-1`).
//!
//! The envelope builder gathers everything; this module decides what each
//! role needs and records the decision. It never changes authority, receipts
//! or the incremental-delivery baseline: selection runs on the packet before
//! it is snapshotted, so a delta is always computed between two selected
//! packets and the snapshot stays the lossless record of what was delivered.
//!
//! - `requirements`: the active (non-withdrawn) delivery requirements, with
//!   the contract revision, gate flag and current evidence status. A
//!   collaborator receives every required item plus the items assigned to
//!   its own job. Required items are never trimmed by role.
//! - `roster`: a collaborator keeps its parent, itself, its direct children
//!   and its prerequisites. The count of omitted rows stays visible.
//! - `context.selection`: one journal event per turn lists sources and
//!   omissions ([`manifest`]).

use crate::db::Q;
use crate::job::JobCtx;
use anyhow::Result;
use sa_store::delivery::{self, RequirementView};
use serde_json::{Map, Value, json};

pub const POLICY: &str = "roles-1";
pub const SCHEMA_VERSION: i64 = 1;
/// Requirement items per envelope.
pub const MAX_ITEMS: usize = 32;
/// Serialized size of `requirements`.
pub const MAX_BYTES: usize = 16 * 1024;
/// Characters of one requirement text.
pub const TEXT_CHARS: usize = 600;
/// Size of the advisor's requirements section.
pub const ADVISOR_BYTES: usize = 4 * 1024;
/// Serialized size of one `context.selection` payload.
pub const MANIFEST_BYTES: usize = 16 * 1024;
const MANIFEST_SOURCES: usize = 256;

const FETCH_REQUIREMENTS: &str = "其余需求项未放入本包；请向主控或操作员询问，不要假定它们不存在";
const FETCH_ROSTER: &str = "其余任务的过程记录不在本包；需要时向主控询问，协作总线可用时可调用 roster";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Lead,
    Implementer,
    Reviewer,
    Advisor,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Lead => "lead",
            Role::Implementer => "implementer",
            Role::Reviewer => "reviewer",
            Role::Advisor => "advisor",
        }
    }
}

/// Structural role: root is the lead, a writing child implements, any other
/// child reads or reviews.
pub fn role_of(job: &JobCtx) -> Role {
    if job.parent.is_none() {
        Role::Lead
    } else if job.writing {
        Role::Implementer
    } else {
        Role::Reviewer
    }
}

struct Contract {
    revision: i64,
    gate_enabled: bool,
    items: Vec<RequirementView>,
    /// Unmet `(requirement, status)` of an assessment of this exact revision.
    unmet: Option<Vec<(i64, String)>>,
}

/// Active requirements of a ticket that has a delivery contract.
fn contract(q: &Q, ticket: i64) -> Result<Option<Contract>> {
    use rusqlite::OptionalExtension;
    let Some(revision) = q
        .conn()
        .query_row("SELECT revision FROM delivery_contracts WHERE ticket=?", [ticket], |r| r.get::<_, i64>(0))
        .optional()?
    else {
        return Ok(None);
    };
    let mut items = delivery::requirements(q.conn(), ticket)?;
    items.retain(|r| !r.withdrawn);
    if items.is_empty() {
        return Ok(None);
    }
    let gate = delivery::gate_metadata(q.conn(), ticket)?;
    // Only an assessment of this exact contract revision speaks for it.
    let unmet = gate
        .latest_assessment
        .filter(|a| a.contract_revision == revision && matches!(a.status.as_str(), "ready" | "partial"))
        .map(|a| a.unmet.iter().map(|e| (e.requirement_id, e.status.clone())).collect());
    Ok(Some(Contract { revision, gate_enabled: gate.enabled, items, unmet }))
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn status_of(contract: &Contract, item: &RequirementView) -> Option<&'static str> {
    let unmet = contract.unmet.as_ref()?;
    if let Some((_, status)) = unmet.iter().find(|(id, _)| *id == item.id) {
        return Some(match status.as_str() {
            "fail" => "fail",
            "stale" => "stale",
            _ => "unknown",
        });
    }
    // A required item absent from a ready/partial assessment's unmet list met it.
    item.required.then_some("pass")
}

fn item_json(contract: &Contract, item: &RequirementView, assigned: Option<bool>) -> Value {
    let mut row = Map::new();
    row.insert("id".into(), json!(item.id));
    row.insert("revision".into(), json!(item.revision));
    row.insert("text".into(), json!(clip(&item.text, TEXT_CHARS)));
    row.insert("required".into(), json!(item.required));
    if let Some(check) = item.behavior.as_ref().filter(|b| b.requirement_revision == item.revision) {
        row.insert("check".into(), json!(check.argv.join(" ")));
    }
    if let Some(path) = &item.path {
        row.insert("path".into(), json!(path));
    }
    if let Some(status) = status_of(contract, item) {
        row.insert("status".into(), json!(status));
    }
    if let Some(assigned) = assigned {
        row.insert("assigned".into(), json!(assigned));
    }
    Value::Object(row)
}

/// `requirements` and, when something is left out, `requirements_omitted`.
/// `None` for a ticket without a contract or without active items.
pub fn requirements(q: &Q, job: &JobCtx) -> Result<Option<(Value, Option<Value>)>> {
    let Some(contract) = contract(q, job.ticket)? else { return Ok(None) };
    let lead = job.parent.is_none();
    let mut not_assigned = 0usize;
    let mut candidates: Vec<(&RequirementView, Option<bool>)> = vec![];
    for item in &contract.items {
        let assigned = item.job == Some(job.id);
        if lead {
            candidates.push((item, None));
        } else if item.required || assigned {
            candidates.push((item, Some(assigned)));
        } else {
            not_assigned += 1;
        }
    }
    // Required items come first, so bounds can only drop optional ones.
    candidates.sort_by_key(|(item, _)| (!item.required, item.id));
    let mut items = vec![];
    let mut used = 256;
    let mut truncated = 0usize;
    for (item, assigned) in candidates {
        let row = item_json(&contract, item, assigned);
        let size = row.to_string().len() + 1;
        if items.len() >= MAX_ITEMS || used + size > MAX_BYTES {
            truncated += 1;
            continue;
        }
        used += size;
        items.push(row);
    }
    let packet = json!({
        "contract_revision": contract.revision,
        "gate_enabled": contract.gate_enabled,
        "items": items,
    });
    let omitted = (truncated + not_assigned > 0).then(|| {
        json!({
            "count": truncated + not_assigned,
            "not_assigned": not_assigned,
            "truncated": truncated,
            "fetch": FETCH_REQUIREMENTS,
        })
    });
    Ok(Some((packet, omitted)))
}

/// Bounded "当前需求" section for an advisor prompt: required active items
/// only. Returns the text, its manifest sources and the omitted count.
pub fn advisor_section(q: &Q, ticket: i64) -> Result<Option<(String, Vec<Value>, usize)>> {
    let Some(contract) = contract(q, ticket)? else { return Ok(None) };
    let required: Vec<_> = contract.items.iter().filter(|i| i.required).collect();
    let optional = contract.items.len() - required.len();
    if required.is_empty() {
        return Ok(None);
    }
    let mut text = format!("当前需求（合同版本 {}，仅必需项）：", contract.revision);
    let mut sources = vec![];
    let mut dropped = 0usize;
    for item in &required {
        let line = format!("\n- #{} r{}: {}", item.id, item.revision, clip(&item.text.replace('\n', " "), TEXT_CHARS));
        if text.len() + line.len() > ADVISOR_BYTES - 96 {
            dropped += 1;
            continue;
        }
        text.push_str(&line);
        sources.push(json!({"key": "requirements", "id": item.id, "revision": item.revision, "reason": "required"}));
    }
    if dropped > 0 {
        text.push_str(&format!("\n（另有 {dropped} 项必需需求因篇幅未列出，请向调用方确认）"));
    }
    Ok(Some((text, sources, dropped + optional)))
}

/// What [`select`] left out, for the manifest.
#[derive(Default)]
pub struct Selection {
    pub omitted: Vec<Value>,
    pub trimmed: Vec<Value>,
}

/// Apply the role rules to a built envelope. The lead keeps everything. A
/// collaborator's `roster` shrinks to its parent, itself, its direct
/// children and its prerequisites; the omitted count stays in
/// `roster_omitted`. Reports, prerequisites, files, notes, checks, recovery
/// and `user_updates` are never touched.
pub fn select(packet: &mut Map<String, Value>, job: &JobCtx) -> Selection {
    let mut out = Selection::default();
    let role = role_of(job);
    if role == Role::Lead {
        return out;
    }
    let needed: Vec<i64> = packet
        .get("prerequisites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p["job"].as_i64())
        .collect();
    if let Some(Value::Array(rows)) = packet.get_mut("roster") {
        let before = rows.len();
        rows.retain(|row| {
            let id = row["job"].as_i64();
            id == Some(job.id)
                || (id.is_some() && id == job.parent)
                || row["parent"].as_i64() == Some(job.id)
                || id.is_some_and(|id| needed.contains(&id))
        });
        let kept = rows.len();
        if kept < before {
            let count = before - kept;
            let reason = format!("role_pack:{}", role.as_str());
            packet.insert(
                "roster_omitted".into(),
                json!({"count": count, "fetch": FETCH_ROSTER}),
            );
            out.omitted.push(json!({"key": "roster", "count": count, "reason": reason, "fetch": FETCH_ROSTER}));
            out.trimmed.push(json!({
                "key": "roster", "reason": "role_pack:parent,self,children,prerequisites", "kept": kept, "of": before,
            }));
        }
    }
    out
}

/// The `context.selection` payload of one turn. `mode` is the wire view:
/// `full`, `delta`, `recovery` or `compacted`.
pub fn manifest(role: Role, packet: &Map<String, Value>, selection: &Selection, mode: &str) -> Value {
    let mut sources: Vec<Value> = selection.trimmed.clone();
    let mut omitted: Vec<Value> = selection.omitted.clone();
    if let Some(items) = packet.get("requirements").and_then(|r| r["items"].as_array()) {
        for item in items {
            let reason = match (item["required"].as_bool(), item["assigned"].as_bool()) {
                (Some(true), _) => "required",
                (_, Some(true)) => "assigned",
                _ => "active",
            };
            sources.push(json!({"key": "requirements", "id": item["id"], "revision": item["revision"], "reason": reason}));
        }
    }
    if let Some(o) = packet.get("requirements_omitted") {
        omitted.push(json!({
            "key": "requirements", "count": o["count"],
            "reason": if o["truncated"].as_u64().unwrap_or(0) > 0 { "size_bound_or_role_pack" } else { "role_pack:not_assigned" },
            "fetch": FETCH_REQUIREMENTS,
        }));
    }
    if let Some(rows) = packet.get("project_knowledge").and_then(Value::as_array) {
        for row in rows {
            let state = row["status"].as_str().unwrap_or("current");
            let reason = if row["relevance"].as_i64().unwrap_or(0) > 0 { "lexical_match" } else { "recent" };
            sources.push(json!({"key": "project_knowledge", "id": row["id"], "state": state, "reason": reason}));
        }
    }
    for key in ["user_updates", "reports", "prerequisites", "notes", "files"] {
        if let Some(rows) = packet.get(key).and_then(Value::as_array).filter(|r| !r.is_empty()) {
            sources.push(json!({"key": key, "count": rows.len(), "reason": "always_kept"}));
        }
    }
    finish(role, mode, sources, omitted)
}

/// Manifest of an advisor consult: the requirements it was shown, and what
/// it was not.
pub fn advisor_manifest(sources: Vec<Value>, omitted_items: usize, brief_bytes: usize) -> Value {
    let mut sources = sources;
    sources.push(json!({"key": "brief", "bytes": brief_bytes, "reason": "caller"}));
    let omitted = if omitted_items > 0 {
        vec![json!({"key": "requirements", "count": omitted_items, "reason": "advisor_required_only", "fetch": FETCH_REQUIREMENTS})]
    } else {
        vec![]
    };
    finish(Role::Advisor, "advice", sources, omitted)
}

fn finish(role: Role, mode: &str, mut sources: Vec<Value>, omitted: Vec<Value>) -> Value {
    let total = sources.len();
    sources.truncate(MANIFEST_SOURCES);
    let build = |sources: &[Value]| {
        let mut m = json!({
            "schema_version": SCHEMA_VERSION, "policy": POLICY, "role": role.as_str(), "mode": mode,
            "sources": sources, "omitted": omitted,
        });
        if sources.len() < total {
            m["sources_omitted"] = json!(total - sources.len());
        }
        m
    };
    let mut m = build(&sources);
    while m.to_string().len() > MANIFEST_BYTES && !sources.is_empty() {
        let keep = sources.len() - sources.len().div_ceil(8);
        sources.truncate(keep);
        m = build(&sources);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: i64, parent: Option<i64>, writing: bool) -> JobCtx {
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("ticket".into(), json!(1));
        if let Some(p) = parent {
            row.insert("parent".into(), json!(p));
        }
        row.insert("writing".into(), json!(writing as i64));
        JobCtx::from_row(&row)
    }

    fn packet() -> Map<String, Value> {
        let rows = [(1, None), (2, Some(1)), (3, Some(1)), (4, Some(2)), (5, Some(3)), (6, Some(1))];
        let mut m = Map::new();
        m.insert(
            "roster".into(),
            Value::Array(rows.iter().map(|(id, parent)| json!({"job": id, "agent": "codex", "state": "running", "role": "", "parent": parent})).collect()),
        );
        m.insert("prerequisites".into(), json!([{"job": 6}]));
        m.insert("user_updates".into(), json!(["改成中文"]));
        m
    }

    #[test]
    fn roles_are_structural() {
        assert_eq!(role_of(&job(1, None, true)), Role::Lead);
        assert_eq!(role_of(&job(2, Some(1), true)), Role::Implementer);
        assert_eq!(role_of(&job(3, Some(1), false)), Role::Reviewer);
    }

    #[test]
    fn collaborator_roster_keeps_parent_self_children_and_prerequisites() {
        let mut p = packet();
        let sel = select(&mut p, &job(2, Some(1), true));
        let ids: Vec<i64> = p["roster"].as_array().unwrap().iter().map(|r| r["job"].as_i64().unwrap()).collect();
        // parent 1, self 2, child 4, prerequisite 6; sibling 3 and its child 5 dropped.
        assert_eq!(ids, vec![1, 2, 4, 6]);
        assert_eq!(p["roster_omitted"]["count"], 2);
        assert_eq!(sel.omitted[0]["count"], 2);
        assert_eq!(p["user_updates"], json!(["改成中文"]));
    }

    #[test]
    fn lead_keeps_everything() {
        let mut p = packet();
        let before = p.clone();
        let sel = select(&mut p, &job(1, None, false));
        assert_eq!(p, before);
        assert!(sel.omitted.is_empty());
    }

    #[test]
    fn manifest_is_bounded_and_counts_truncation() {
        let mut p = Map::new();
        let rows: Vec<Value> = (0..2000)
            .map(|id| json!({"id": id, "relevance": 0, "body": "x"}))
            .collect();
        p.insert("project_knowledge".into(), Value::Array(rows));
        let m = manifest(Role::Lead, &p, &Selection::default(), "full");
        assert!(m.to_string().len() <= MANIFEST_BYTES);
        assert!(m["sources_omitted"].as_u64().unwrap() > 0);
        assert_eq!(m["policy"], "roles-1");
    }

    #[test]
    fn clip_is_char_boundary_safe() {
        let text = "字".repeat(700);
        let out = clip(&text, TEXT_CHARS);
        assert_eq!(out.chars().count(), TEXT_CHARS);
        assert!(out.ends_with('…'));
    }
}
