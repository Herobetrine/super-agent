//! Lossless incremental delivery within a confirmed successful native session.
//!
//! The baseline comes from an immutable input snapshot, never the live job
//! table at reply time. Failed requests cannot advance it. The original
//! envelope remains available to commit logic and in the journal.

use crate::adapter::Route;
use crate::db::{Q, int_of, str_of};
use crate::protocol::{digest, stringify};
use anyhow::{Result, ensure};
use serde_json::{Map, Value, json};

/// Small authority and current-state data are repeated even on continuation.
const CURRENT: &[&str] = &[
    "ticket", "job", "turn", "depth", "agent", "sender", "role", "project",
    "permission_profile", "allow_lead_routing", "allowed_child_permissions",
    "can_assign_write", "this_turn_can_write", "execution_directory",
    "selected_execution", "claude_route", "isolated_writers", "workspace",
    "harness_health", "recovery", "next_step", "scm_notice", "collaboration_preferences",
];
const APPEND: &[&str] = &["notes", "user_updates", "mcp_results"];
const DELTA_GUIDE: &str = "继续同一原生会话。context_delivery.mode=delta 时将本包合并到已确认上下文。未列字段沿用旧值，removed_fields 删除旧字段。array_modes 指定 append 追加、upsert 按 id（roster 按 job）更新。object_modes 指定逐字段合并并按其 removed_fields 删除子字段。其余字段整项替换。reports 只含变化项，不能据此认定其他任务消失。若旧上下文已被压缩或缺失，先按 full_context 读取完整快照；读取失败应明确报告缺失并请求恢复完整上下文。";

pub struct Baseline {
    pub packet: Map<String, Value>,
    pub guide_sha256: String,
    pub snapshot_sha256: String,
    pub reply: String,
    pub lazy_mcp_schemas: bool,
}

const LAZY_MCP_MIN_BYTES: usize = 16 * 1024;
const TOOL_DESCRIPTION_CHARS: usize = 120;

/// Change only the wire view. The caller attaches an archive of `snapshot`,
/// whose full descriptions and schemas still authorize and validate calls.
/// An omitted directory in a delta keeps its prior references, resolved
/// against the current full snapshot with the same unchanged directory.
pub fn defer_mcp_schemas(view: &mut Map<String, Value>, snapshot: &Map<String, Value>) -> bool {
    let Some(servers) = snapshot.get("project_extensions").and_then(|ext| ext.get("mcp")).and_then(Value::as_array) else {
        return false;
    };
    let eligible: Vec<_> = servers.iter().enumerate().filter_map(|(index, server)| {
        let tools = server.get("tools")?.as_array()?;
        (super::bytes(&server["tools"]) >= LAZY_MCP_MIN_BYTES
            && tools.iter().all(|tool| tool.get("name").and_then(Value::as_str).is_some()))
            .then_some(index)
    }).collect();
    if eligible.is_empty() { return false; }
    if let Some(delivered) = view.get_mut("project_extensions").and_then(|ext| ext.get_mut("mcp")).and_then(Value::as_array_mut) {
        for index in eligible {
            let Some(server) = delivered.get_mut(index) else { continue };
            let tools = servers[index]["tools"].as_array().expect("checked tool array");
            server["tools"] = Value::Array(tools.iter().enumerate().map(|(tool_index, tool)| {
                let description: String = tool["description"].as_str().unwrap_or_default()
                    .chars().take(TOOL_DESCRIPTION_CHARS).collect();
                json!({
                    "name":tool["name"],"description":description,
                    "schema_ref":format!("/project_extensions/mcp/{index}/tools/{tool_index}"),
                })
            }).collect());
        }
    }
    view.insert("mcp_schema_lookup".into(), json!({
        "source":"full_context","description_max_chars":TOOL_DESCRIPTION_CHARS,
        "instructions":"工具目录中的描述为短预览。使用工具前，以 Python/PowerShell JSON 解析在内存读取 full_context.object_path，验证 bytes 和 sha256，按 server 和 tool 精确筛选 project_extensions.mcp[].tools[] 或按 schema_ref JSON Pointer 定位，并核对工具名。只输出目标工具的完整 description 和 inputSchema，遵守完整描述及 schema 后再构造参数。禁止 cat/type/Get-Content 将整个快照回灌上下文。增量中沿用的 schema_ref 仍相对于当前 full_context。引用不改变现有授权范围。",
    }));
    true
}

/// Both adapters journal these native objects without wrapping them. Unknown
/// harness compaction formats still rely on the full-context recovery file.
pub fn native_compaction(event: &Value) -> bool {
    (event["type"] == "system" && event["subtype"] == "compact_boundary")
        || (event["type"] == "system" && event["subtype"] == "status" && event["status"] == "compacting")
        || (matches!(event["method"].as_str(), Some("item/started" | "item/completed"))
            && event["params"]["item"]["type"] == "contextCompaction")
        || event["method"] == "thread/compacted"
}

/// Readers needing the complete envelope follow the running event's explicit
/// snapshot link. Journals written before incremental delivery retain input.
pub fn full_input(q: &Q, job: i64, before: i64) -> Result<Option<Map<String, Value>>> {
    let running = q.one(
        "SELECT payload FROM events WHERE job=? AND kind='running' AND seq<? ORDER BY seq DESC LIMIT 1",
        &[&job, &before],
    )?;
    let running = running.map(|row| serde_json::from_str::<Value>(str_of(&row, "payload"))).transpose()?;
    let input = if let Some(seq) = running.as_ref().and_then(|r| r["snapshot_seq"].as_i64()) {
        let snapshot = q.one(
            "SELECT payload,sha256 FROM events WHERE seq=? AND job=? AND kind='context.snapshot' AND seq<?",
            &[&seq, &job, &before],
        )?;
        ensure!(snapshot.is_some(), "完整上下文快照缺失");
        snapshot
    } else {
        q.one(
            "SELECT payload,sha256 FROM events WHERE job=? AND kind='input' AND seq<? ORDER BY seq DESC LIMIT 1",
            &[&job, &before],
        )?
    };
    if let Some(row) = &input {
        ensure!(digest(str_of(row, "payload")) == str_of(row, "sha256"), "完整上下文快照摘要不匹配");
    }
    Ok(input)
}

/// Only the newest committed receipt can authorize elision. A changed native
/// session, route, damaged snapshot or compacted delivery forces full replay.
pub fn baseline(q: &Q, job: i64, session: Option<&str>, route: &Route) -> Result<Option<Baseline>> {
    let Some(session) = session.filter(|s| !s.is_empty()) else { return Ok(None) };
    let Some(row) = q.one(
        "SELECT payload FROM events WHERE job=? AND kind='context.delivered' ORDER BY seq DESC LIMIT 1",
        &[&job],
    )? else { return Ok(None) };
    let Ok(receipt) = serde_json::from_str::<Value>(str_of(&row, "payload")) else { return Ok(None) };
    if receipt["session"].as_str() != Some(session)
        || receipt["route"] != serde_json::to_value(route)?
        || receipt["complete"] != true
    {
        return Ok(None);
    }
    let Some(seq) = receipt["snapshot_seq"].as_i64() else { return Ok(None) };
    if q.exists(
        "SELECT 1 FROM events WHERE job=? AND kind='context.compacted' AND seq>? LIMIT 1",
        &[&job, &seq],
    )? { return Ok(None); }
    let Some(snapshot) = q.one(
        "SELECT payload FROM events WHERE seq=? AND job=? AND kind='context.snapshot'",
        &[&seq, &job],
    )? else { return Ok(None) };
    let Ok(packet) = serde_json::from_str::<Value>(str_of(&snapshot, "payload")) else { return Ok(None) };
    let Some(packet) = packet.as_object() else { return Ok(None) };
    let sha = digest(&stringify(&super::sort_keys(&Value::Object(packet.clone()))));
    if receipt["snapshot_sha256"].as_str() != Some(sha.as_str()) {
        return Ok(None);
    }
    let Some(reply_seq) = receipt["reply_seq"].as_i64() else { return Ok(None) };
    let Some(reply) = q.one(
        "SELECT payload FROM events WHERE seq=? AND job=? AND kind='reply'",
        &[&reply_seq, &job],
    )? else { return Ok(None) };
    let Ok(reply) = serde_json::from_str::<Value>(str_of(&reply, "payload")) else { return Ok(None) };
    if reply["session"].as_str() != Some(session) { return Ok(None); }
    Ok(Some(Baseline {
        packet: packet.clone(),
        guide_sha256: receipt["guide_sha256"].as_str().unwrap_or_default().into(),
        snapshot_sha256: sha,
        reply: reply["text"].as_str().unwrap_or_default().into(),
        lazy_mcp_schemas: receipt["lazy_mcp_schemas"].as_bool().unwrap_or(false),
    }))
}

/// Build a reversible view. Full values of newly received or changed reports
/// are retained. Non-prefix histories and removals use whole-array replacement.
pub fn delta(packet: &Map<String, Value>, guide: &str, old: &Baseline) -> (Map<String, Value>, String) {
    let mut out = Map::new();
    let mut modes = Map::new();
    let mut object_modes = Map::new();
    for (key, value) in packet {
        let previous = old.packet.get(key);
        if key == "autonomy"
            && let Some((current, before)) = value.as_object().zip(previous.and_then(Value::as_object))
        {
            let changed: Map<_, _> = current.iter()
                .filter(|(k, v)| k.as_str() == "phase" || before.get(*k) != Some(*v))
                .map(|(k, v)| (k.clone(), v.clone())).collect();
            let removed: Vec<_> = before.keys().filter(|k| !current.contains_key(*k)).cloned().collect();
            out.insert(key.clone(), Value::Object(changed));
            object_modes.insert(key.clone(), json!({"removed_fields":removed}));
            continue;
        }
        let small_goal = matches!(key.as_str(), "goal" | "objective" | "assignment")
            && stringify(value).len() <= 8192;
        if CURRENT.contains(&key.as_str()) || small_goal {
            out.insert(key.clone(), value.clone());
            continue;
        }
        if key == "previous_reply" && value.as_str() == Some(old.reply.as_str()) {
            continue;
        }
        if key != "previous_reply" && previous == Some(value) { continue; }
        let rows = value.as_array().zip(previous.and_then(Value::as_array));
        if let Some((rows, before)) = rows {
            if APPEND.contains(&key.as_str()) && rows.starts_with(before) {
                out.insert(key.clone(), json!(&rows[before.len()..]));
                modes.insert(key.clone(), json!("append"));
                continue;
            }
            if matches!(key.as_str(), "reports" | "roster") {
                let id = if key == "roster" { "job" } else { "id" };
                // Only unique, numeric identities permit an upsert view.
                let unique = |rows: &[Value]| {
                    let ids: std::collections::HashSet<_> = rows.iter().filter_map(|r| r[id].as_i64()).collect();
                    ids.len() == rows.len()
                };
                if unique(rows) && unique(before)
                    && before.iter().all(|r| rows.iter().any(|n| n[id] == r[id]))
                {
                    let changed: Vec<_> = rows.iter().filter(|r| !before.contains(r)).cloned().collect();
                    out.insert(key.clone(), json!(changed));
                    modes.insert(key.clone(), json!("upsert"));
                    continue;
                }
            }
        }
        out.insert(key.clone(), value.clone());
    }
    let removed: Vec<_> = old.packet.keys().filter(|key| !packet.contains_key(*key)).cloned().collect();
    out.insert("context_delivery".into(), json!({
        "version":1,"mode":"delta","baseline_sha256":old.snapshot_sha256,
        "guide_sha256":digest(guide),"removed_fields":removed,"array_modes":modes,"object_modes":object_modes,
        "previous_reply":"最新已成功回复即本会话上一条终稿，省略时沿用该终稿。"
    }));
    let delivered_guide = if old.guide_sha256 == digest(guide) {
        DELTA_GUIDE.to_string()
    } else {
        format!("{guide}\n{DELTA_GUIDE}")
    };
    (out, delivered_guide)
}

/// The receipt is written in the same transaction as its successful reply.
/// A reset by continuous-mode commit intentionally makes it unusable.
pub fn acknowledge(
    q: &Q, who: crate::db::Who, route: &Route, mut receipt: Value,
) -> Result<()> {
    let Some(reply) = q.one(
        "SELECT seq,payload FROM events WHERE job=? AND turn=? AND kind='reply' ORDER BY seq DESC LIMIT 1",
        &[&who.id, &who.turn],
    )? else { return Ok(()) };
    let payload: Value = serde_json::from_str(str_of(&reply, "payload"))?;
    receipt["reply_seq"] = json!(int_of(&reply,"seq"));
    receipt["session"] = payload["session"].clone();
    receipt["route"] = serde_json::to_value(route)?;
    q.log(who, "context.delivered", &receipt)?;
    Ok(())
}
