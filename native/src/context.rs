use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::PathBuf};
use tempfile::NamedTempFile;

#[derive(Deserialize)]
pub struct Compact {
    packet: Map<String, Value>,
    guide: String,
    directory: PathBuf,
    #[serde(default = "default_budget")]
    max_bytes: usize,
}
fn default_budget() -> usize {
    6 * 1024 * 1024
}
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
pub fn compact(req: Compact) -> Result<Value> {
    let raw = serde_json::to_vec(&req.packet)?;
    let budget = req.max_bytes.saturating_sub(req.guide.len() + 1);
    if raw.len() <= budget {
        return Ok(Value::Object(req.packet));
    }
    let store = req.directory.join("context-objects");
    fs::create_dir_all(&store)?;
    ensure!(
        store.canonicalize()?.parent() == Some(req.directory.canonicalize()?.as_path()),
        "上下文目录发生重定向"
    );
    let digest = format!("{:x}", Sha256::digest(&raw));
    let dest = store.join(format!("{digest}.json"));
    let mut tmp = NamedTempFile::new_in(&store)?;
    tmp.write_all(&raw)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&dest).context("无法提交上下文原文")?;
    let mut out = Map::new();
    for &k in CONTROL {
        if let Some(v) = req.packet.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    out.insert("full_context".into(),json!({"bytes":raw.len(),"sha256":digest,"object_path":dest,"view":"按需读取原始 UTF-8 JSON，以 SHA-256 核验。"}));
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
            let v = req.packet.get(k)?;
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
        let Some(rows) = req.packet.get(k).and_then(Value::as_array) else {
            continue;
        };
        let newest = out.get_mut(k).unwrap().as_array_mut().unwrap();
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
    Ok(Value::Object(out))
}
