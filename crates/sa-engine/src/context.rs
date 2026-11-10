//! Context compaction for prompts over the 6 MiB budget (E:352-360).
//!
//! Port of `native/src/context.rs::compact`. The native helper parsed the
//! packet into a key-sorted map and returned key-sorted JSON to Node, so the
//! compacted packet is emitted with recursively sorted keys to keep the
//! journaled `input` byte-identical in shape.

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

#[path = "context_delivery.rs"]
pub mod delivery;

pub const MAX_PROMPT_BYTES: usize = 6 * 1024 * 1024;

/// Kept verbatim. `harness_health` and `recovery` are Rust-port additions to
/// the envelope; they are small control data and are kept too.
const CONTROL: &[&str] = &[
    "ticket",
    "job",
    "turn",
    "depth",
    "agent",
    "sender",
    "project",
    "role",
    "permission_profile",
    "allow_lead_routing",
    "allowed_child_permissions",
    "harness_capabilities",
    "selected_execution",
    "claude_route",
    "can_assign_write",
    "this_turn_can_write",
    "harness_health",
    "recovery",
    "execution_directory",
    "isolated_writers",
    "workspace",
    "scm_feedback",
    "scm_notice",
];
const HISTORY: &[&str] = &["user_updates", "reports", "notes", "roster", "mcp_results", "plan"];

fn bytes(v: &Value) -> usize {
    serde_json::to_vec(v).expect("valid JSON").len()
}

fn excerpt(s: &str, budget: usize) -> Option<Value> {
    const GAP: &str = " […中段省略，见 full_context…] ";
    if budget <= GAP.len() + 2 {
        return None;
    }
    let mut n = (budget - GAP.len() - 2).min(s.len()) / 2;
    loop {
        let mut head = n;
        let mut tail = s.len() - n;
        while !s.is_char_boundary(head) {
            head -= 1;
        }
        while !s.is_char_boundary(tail) {
            tail += 1;
        }
        let result = Value::String(format!("{}{GAP}{}", &s[..head], &s[tail..]));
        if bytes(&result) <= budget {
            return Some(result);
        }
        n /= 2;
    }
}

fn preview(v: &Value, budget: usize) -> Option<Value> {
    if bytes(v) <= budget {
        return Some(v.clone());
    }
    if let Some(s) = v.as_str() {
        return excerpt(s, budget);
    }
    let source = v.as_object()?;
    let mut row = Map::new();
    for key in [
        "id",
        "sender",
        "state",
        "job",
        "phase",
        "milestone",
        "role",
        "mcp",
        "skills",
        "result",
        "body",
        "objective",
        "assignment",
        "learning",
        "summary",
    ] {
        if let Some(value) = source.get(key) {
            let used = bytes(&Value::Object(row.clone())) + key.len() + 5;
            let left = budget.saturating_sub(used);
            if bytes(value) <= left {
                row.insert(key.into(), value.clone());
            } else if let Some(short) = value.as_str().and_then(|s| excerpt(s, left)) {
                row.insert(key.into(), short);
            }
        }
    }
    if row.is_empty() {
        None
    } else {
        Some(Value::Object(row))
    }
}

fn sorted_map(m: &Map<String, Value>) -> Map<String, Value> {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    keys.into_iter().map(|k| (k.clone(), sort_keys(&m[k]))).collect()
}

/// Persist the complete, canonical packet independently of its prompt view.
pub fn archive(packet: &Map<String, Value>, directory: &Path) -> Result<Value> {
    let raw = serde_json::to_vec(&sorted_map(packet))?;
    let store = directory.join("context-objects");
    fs::create_dir_all(&store)?;
    ensure!(
        store.canonicalize()?.parent() == Some(directory.canonicalize()?.as_path()),
        "上下文目录发生重定向"
    );
    let digest = format!("{:x}", Sha256::digest(&raw));
    let dest = store.join(format!("{digest}.json"));
    let mut tmp = NamedTempFile::new_in(&store)?;
    tmp.write_all(&raw)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&dest).context("无法提交上下文原文")?;
    Ok(json!({"bytes":raw.len(),"sha256":digest,"object_path":dest,"view":"按需读取原始 UTF-8 JSON，以 SHA-256 核验。"}))
}

/// Recursively sort object keys (serde_json without `preserve_order`).
pub fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(sorted_map(m)),
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// Compact `packet` so that `guide + "\n" + packet` fits in `max_bytes`.
/// The raw packet is archived under `<directory>/context-objects/<sha>.json`.
pub fn compact(
    packet: &Map<String, Value>,
    guide: &str,
    directory: &Path,
    max_bytes: usize,
) -> Result<Map<String, Value>> {
    // The native helper received the packet already key-sorted.
    let packet = sorted_map(packet);
    let raw = serde_json::to_vec(&packet)?;
    let budget = max_bytes.saturating_sub(guide.len() + 1);
    if raw.len() <= budget {
        return Ok(packet);
    }
    let mut out = Map::new();
    for &k in CONTROL {
        if let Some(v) = packet.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    out.insert(
        "full_context".into(),
        archive(&packet, directory)?,
    );
    if let Some(checkpoint)=packet.get("checkpoint_context") {
        out.insert("checkpoint_context_ref".into(),json!({"source_ticket":checkpoint["source_ticket"],"source_seq":checkpoint["source_seq"],"source_sha256":checkpoint["source_sha256"],"notice":checkpoint["notice"],"json_pointer":"/checkpoint_context","view":"历史分支证据存于 full_context，按需读取；不是待重放指令。"}));
    }
    for &k in HISTORY {
        out.insert(k.into(), json!([]));
    }
    let used = serde_json::to_vec(&out)?.len();
    ensure!(used <= budget, "当前上限容不下授权范围及完整上下文引用");
    let mut left = budget - used;
    let current = [
        "user_updates",
        "mcp_results",
        "project_extensions",
        "plan",
        "objective",
        "goal",
        "assignment",
        "autonomy",
        "project_knowledge",
        "checkpoint_context",
        "next_step",
        "reports",
        "notes",
        "files",
        "delegate_models",
        "previous_reply",
        "roster",
    ];
    let entries: Vec<_> = current
        .into_iter()
        .filter_map(|k| {
            let v = packet.get(k)?;
            if HISTORY.contains(&k) {
                Some((k, v.as_array()?.last()?, true))
            } else {
                Some((k, v, false))
            }
        })
        .collect();
    let count = entries.len();
    for (i, (k, v, history)) in entries.into_iter().enumerate() {
        let overhead = if history { 0 } else { k.len() + 5 };
        if let Some(part) = preview(v, (left / (count - i)).saturating_sub(overhead)) {
            left -= bytes(&part) + overhead;
            out.insert(
                k.into(),
                if history {
                    Value::Array(vec![part])
                } else {
                    part
                },
            );
        }
    }
    for &k in HISTORY {
        let Some(rows) = packet.get(k).and_then(Value::as_array) else {
            continue;
        };
        let newest = out
            .get_mut(k)
            .and_then(Value::as_array_mut)
            .expect("history keys are arrays");
        if newest.is_empty() {
            continue;
        }
        let mut earlier = vec![];
        for row in rows.iter().rev().skip(1) {
            let size = bytes(row) + 1;
            if size > left {
                break;
            }
            left -= size;
            earlier.push(row.clone());
        }
        earlier.reverse();
        earlier.append(newest);
        *newest = earlier;
    }
    Ok(sorted_map(&out))
}
