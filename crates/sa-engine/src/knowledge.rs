//! Shared per-project knowledge base (Rust-port addition, spec §4.4).
//!
//! Entries live in `project_knowledge` (see [`sa_store::knowledge`]) and are
//! keyed by the ticket's canonical `cwd`, so every agent of every ticket in
//! the same project reads and writes the same set. Sources:
//! - `SA_MEMO` lines in the final reply of any job ([`parse_memo`]);
//! - one `progress` entry when a root job ends `done` or `partial`
//!   ([`root_progress`]);
//! - operator edits through [`crate::Engine`].
//!
//! Task-relevant active entries are added to each turn's envelope as
//! `project_knowledge` ([`context_for`]), with recent context as fallback.

use crate::db::{Q, Who, now, str_of, who_of};
use crate::job::JobCtx;
use crate::protocol::{self as p, digest, utf16_len};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use sa_store::knowledge::{self as store, KINDS, KnowledgeEntry, MAX_BODY, PathDigest};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

/// Accepted `SA_MEMO` lines per turn; later ones are rejected.
pub const MAX_PER_TURN: usize = 8;
/// Body budget of the `project_knowledge` envelope field (UTF-16 units).
pub const CONTEXT_CHARS: usize = 6000;
/// Entries in the envelope field at most.
pub const CONTEXT_ENTRIES: usize = 48;
/// Newest active rows considered for the envelope.
const CONTEXT_SCAN: i64 = 256;
/// Excerpt lengths of the automatic progress entry.
const GOAL_EXCERPT: usize = 300;
const FOLLOW_EXCERPT: usize = 200;
const RESULT_EXCERPT: usize = 1200;

/// Control characters other than tab and newline, and bidi overrides.
static UNSAFE_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[[\p{Cc}&&[^\t\n]]\x{202a}-\x{202e}\x{2066}-\x{2069}]").expect("valid regex")
});

/// A new entry.
pub struct NewEntry<'a> {
    pub cwd: &'a str,
    pub kind: &'a str,
    pub body: &'a str,
    pub ticket: Option<i64>,
    pub job: Option<i64>,
    pub agent: &'a str,
}

fn trim(s: &str) -> &str {
    s.trim_matches(p::js_whitespace)
}

/// Validated, trimmed body, or `None`.
pub fn valid_body(body: &str) -> Option<&str> {
    let body = trim(body);
    (p::valid_str(body, MAX_BODY) && !UNSAFE_BODY.is_match(body)).then_some(body)
}

/// `SA_MEMO {"kind","text"}`: exactly these two keys, a known kind and a
/// non-blank text of at most [`MAX_BODY`] UTF-16 units. Returns the kind and
/// the trimmed text.
pub fn parse_memo(cmd: &Value) -> Option<(&str, &str)> {
    let o = cmd.as_object().filter(|o| o.len() == 2)?;
    let kind = o.get("kind")?.as_str().filter(|k| KINDS.contains(k))?;
    let body = valid_body(o.get("text")?.as_str()?)?;
    Some((kind, body))
}

/// Paths one `SA_MEMO` may depend on.
pub const MAX_MEMO_PATHS: usize = 8;
/// Entries whose recorded files are re-hashed per envelope.
pub const MAX_BASIS_CHECKS: usize = 64;
/// Largest file hashed for a basis; larger files are unverifiable.
pub const MAX_BASIS_FILE: u64 = 4 * 1024 * 1024;
/// Total bytes read by one envelope's freshness check.
const BASIS_READ_BUDGET: u64 = 16 * 1024 * 1024;

/// A parsed `SA_MEMO` with the optional typed-basis keys.
#[derive(Debug, PartialEq)]
pub struct Memo<'a> {
    pub kind: &'a str,
    pub text: &'a str,
    pub paths: Vec<String>,
    pub supersedes: Option<i64>,
}

/// `SA_MEMO {"kind","text"[,"paths"][,"supersedes"]}`. The two-key form is
/// exactly [`parse_memo`]. `paths` is at most [`MAX_MEMO_PATHS`] project
/// relative files (delivery path validation, duplicates dropped);
/// `supersedes` a positive entry id. Any other key rejects the memo.
pub fn parse_memo_ext(cmd: &Value) -> Option<Memo<'_>> {
    let o = cmd.as_object()?;
    if o.keys().any(|k| !matches!(k.as_str(), "kind" | "text" | "paths" | "supersedes")) {
        return None;
    }
    let kind = o.get("kind")?.as_str().filter(|k| KINDS.contains(k))?;
    let text = valid_body(o.get("text")?.as_str()?)?;
    let mut paths: Vec<String> = vec![];
    if let Some(list) = o.get("paths") {
        let list = list.as_array().filter(|l| l.len() <= MAX_MEMO_PATHS)?;
        for item in list {
            let path = sa_store::delivery::normalize_path(item.as_str()?).ok()?;
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let supersedes = match o.get("supersedes") {
        None => None,
        Some(v) => Some(v.as_i64().filter(|id| *id > 0)?),
    };
    Some(Memo { kind, text, paths, supersedes })
}

/// SHA-256 of a project file for a knowledge basis: lowercase hex,
/// `missing` when absent, `unverifiable` for links, escapes, non-files,
/// files over [`MAX_BASIS_FILE`] or an exhausted read budget (`left`).
pub fn digest_path(root: &Path, rel: &str, left: &mut u64) -> String {
    const UNVERIFIABLE: &str = "unverifiable";
    let Ok(root) = root.canonicalize() else { return UNVERIFIABLE.into() };
    let file = root.join(rel);
    let meta = match std::fs::symlink_metadata(&file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "missing".into(),
        Err(_) => return UNVERIFIABLE.into(),
    };
    if !meta.is_file() || meta.len() > MAX_BASIS_FILE || meta.len() > *left {
        return UNVERIFIABLE.into();
    }
    if !file.canonicalize().is_ok_and(|c| c.starts_with(&root)) {
        return UNVERIFIABLE.into();
    }
    match std::fs::read(&file) {
        Ok(bytes) if bytes.len() as u64 <= MAX_BASIS_FILE => {
            *left = left.saturating_sub(bytes.len() as u64);
            sa_store::delivery::sha256(&bytes)
        }
        _ => UNVERIFIABLE.into(),
    }
}

/// Digests of `paths` under `root`. Call outside any transaction.
pub fn digest_paths(root: &Path, paths: &[String]) -> Vec<PathDigest> {
    let mut left = BASIS_READ_BUDGET;
    paths
        .iter()
        .map(|p| PathDigest { path: p.clone(), sha256: digest_path(root, p, &mut left) })
        .collect()
}

/// Record the file basis of a new entry; `verified` is now, not `created`.
pub fn record_basis(q: &Q, id: i64, kind: &str, digests: &[PathDigest]) -> Result<()> {
    let kind = if kind == "memory" { "fact" } else { kind };
    q.run(
        "INSERT OR REPLACE INTO knowledge_basis(entry,type,paths,basis,verified,state) VALUES(?,?,?,'files',?,'current')",
        &[&id, &kind, &serde_json::to_string(digests)?, &now()],
    )?;
    Ok(())
}

/// Mark `old` superseded by `new`: archived, state `superseded`. Only an
/// active entry of the same project qualifies. Returns whether it applied.
pub fn supersede(q: &Q, new: i64, old: i64) -> Result<bool> {
    if new == old {
        return Ok(false);
    }
    let (Some(a), Some(b)) = (
        q.one("SELECT cwd FROM project_knowledge WHERE id=?", &[&new])?,
        q.one("SELECT cwd,archived FROM project_knowledge WHERE id=?", &[&old])?,
    ) else {
        return Ok(false);
    };
    if str_of(&a, "cwd") != str_of(&b, "cwd") || b["archived"].as_i64().unwrap_or(1) != 0 {
        return Ok(false);
    }
    q.run("UPDATE project_knowledge SET archived=1 WHERE id=?", &[&old])?;
    q.run(
        "INSERT INTO knowledge_basis(entry,state,superseded_by) VALUES(?,'superseded',?)
         ON CONFLICT(entry) DO UPDATE SET state='superseded',superseded_by=excluded.superseded_by",
        &[&old, &new],
    )?;
    Ok(true)
}

/// Insert an entry unless the project already has an active entry with the
/// same body, or the same job already recorded it (a replay after the
/// operator archived it). Logs `knowledge.add {id, kind}` on `log_on`.
/// Returns the new id, or `None` for a duplicate.
pub fn insert(q: &Q, e: &NewEntry, log_on: Option<Who>) -> Result<Option<i64>> {
    // A worker may execute in an isolated worktree. Its shared knowledge still
    // belongs to the originating ticket's project, not that disposable path.
    let project = match e.ticket {
        Some(ticket) => {
            let row = q
                .one("SELECT cwd FROM tickets WHERE id=?", &[&ticket])?
                .ok_or_else(|| anyhow!("知识来源工单不存在"))?;
            str_of(&row, "cwd").to_string()
        }
        None => e.cwd.to_string(),
    };
    let sha = digest(e.body);
    let duplicate = q.exists(
        "SELECT 1 FROM project_knowledge WHERE cwd=? AND sha256=? AND (archived=0 OR (job IS NOT NULL AND job=?)) LIMIT 1",
        &[&project, &sha, &e.job],
    )?;
    if duplicate {
        return Ok(None);
    }
    let id = q.insert(
        "INSERT INTO project_knowledge(cwd,kind,body,ticket,job,agent,created,sha256) VALUES(?,?,?,?,?,?,?,?)",
        &[&project, &e.kind, &e.body, &e.ticket, &e.job, &e.agent, &now(), &sha],
    )?;
    if let Some(who) = log_on {
        q.log(who, "knowledge.add", &json!({"id": id, "kind": e.kind}))?;
    }
    Ok(Some(id))
}

/// The first `max` UTF-16 units of `s`, with `…` when shortened.
fn excerpt(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        used += c.len_utf16();
        if used >= max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// One line: whitespace runs collapsed.
fn one_line(s: &str) -> String {
    s.split(p::js_whitespace)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Automatic `progress` entry when a root job ends `done` or `partial`:
/// goal (and the latest follow-up) plus the reply without control lines.
pub fn root_progress(q: &Q, job: &JobCtx, state: &str, text: &str) -> Result<()> {
    let label = match state {
        "done" => "完成",
        "partial" => "部分完成",
        _ => return Ok(()),
    };
    let goal = q
        .one("SELECT goal FROM tickets WHERE id=?", &[&job.ticket])?
        .map(|t| str_of(&t, "goal").to_string())
        .unwrap_or_default();
    let follow = q
        .one(
            "SELECT payload FROM events WHERE ticket=? AND kind='user.follow' ORDER BY seq DESC LIMIT 1",
            &[&job.ticket],
        )?
        .and_then(|r| serde_json::from_str::<Value>(str_of(&r, "payload")).ok())
        .and_then(|v| v.as_str().map(str::to_string));
    let reply: Vec<&str> = text.split('\n').filter(|l| !l.starts_with("SA_")).collect();
    let reply = one_line(&reply.join("\n"));
    let mut body = format!(
        "工单 #{}（{label}）目标：{}",
        job.ticket,
        excerpt(&one_line(&goal), GOAL_EXCERPT)
    );
    if let Some(f) = follow {
        body.push_str(&format!(
            "\n最新追加：{}",
            excerpt(&one_line(&f), FOLLOW_EXCERPT)
        ));
    }
    let reply = if reply.is_empty() {
        "（无正文）".to_string()
    } else {
        excerpt(&reply, RESULT_EXCERPT)
    };
    body.push_str(&format!("\n结果：{reply}"));
    insert(
        q,
        &NewEntry {
            cwd: &job.cwd,
            kind: "progress",
            body: &body,
            ticket: Some(job.ticket),
            job: Some(job.id),
            agent: &job.agent,
        },
        Some(job.who()),
    )?;
    Ok(())
}

/// `project_knowledge` envelope field: newest first, `{id, kind, body,
/// agent, ticket}`. Each kind first gets an equal share of
/// [`CONTEXT_CHARS`] for its newest entries; the rest of the budget goes to
/// the next newest entries of any kind.
pub fn context(q: &Q, cwd: &str) -> Result<Vec<Value>> {
    let rows = store::active(q.conn(), cwd, CONTEXT_SCAN)?;
    let size = |e: &KnowledgeEntry| utf16_len(&e.body);
    let mut chosen = vec![false; rows.len()];
    let (mut total, mut count) = (0, 0);
    let share = CONTEXT_CHARS / KINDS.len();
    for kind in KINDS {
        let mut used = 0;
        for (i, e) in rows.iter().enumerate().filter(|(_, e)| e.kind == kind) {
            if used + size(e) > share || count >= CONTEXT_ENTRIES {
                break;
            }
            used += size(e);
            count += 1;
            chosen[i] = true;
        }
        total += used;
    }
    for (i, e) in rows.iter().enumerate() {
        if !chosen[i] && total + size(e) <= CONTEXT_CHARS && count < CONTEXT_ENTRIES {
            total += size(e);
            count += 1;
            chosen[i] = true;
        }
    }
    Ok(rows
        .iter()
        .zip(chosen)
        .filter(|(_, c)| *c)
        .map(|(e, _)| json!({"id": e.id, "kind": e.kind, "body": e.body, "agent": e.agent, "ticket": e.ticket}))
        .collect())
}

/// Task-aware context. Search all active project history before applying a
/// candidate limit, so older relevant lessons are not hidden by recent progress.
/// Relevant entries are first; remaining budget uses the existing kind-balanced
/// recent selection. No match preserves the legacy order and selection.
/// `created` records source time, never independently verified time.
pub fn context_for(q: &Q, cwd: &str, query: &str) -> Result<Vec<Value>> {
    let matched = store::relevant(q.conn(), cwd, query, CONTEXT_SCAN as usize)?;
    let recent = store::active(q.conn(), cwd, CONTEXT_SCAN)?;
    let fallback = context(q, cwd)?;
    let mut result: Vec<Value> = vec![];
    let mut bodies = HashSet::new();
    let mut total = 0;
    let mut push = |e: &KnowledgeEntry, relevance: i64| {
        let size = utf16_len(&e.body);
        if result.len() >= CONTEXT_ENTRIES
            || total + size > CONTEXT_CHARS
            || !bodies.insert(e.body.clone())
        {
            return;
        }
        total += size;
        result.push(json!({
            "id": e.id, "kind": e.kind, "body": e.body, "agent": e.agent, "ticket": e.ticket,
            "job": e.job, "created": e.created, "relevance": relevance,
        }));
    };
    for entry in &matched {
        push(&entry.entry, entry.score);
    }
    for value in &fallback {
        if let Some(entry) = recent
            .iter()
            .find(|entry| value["id"].as_i64() == Some(entry.id))
        {
            push(entry, 0);
        }
    }
    apply_basis(q, cwd, &mut result)?;
    Ok(result)
}

/// Re-hash the files recorded with entries chosen for the envelope (at most
/// [`MAX_BASIS_CHECKS`], bounded bytes) and mark changed ones stale, persisting
/// the state. Stale entries follow current ones; entries without recorded
/// paths are untouched. A file that is unverifiable now or when recorded
/// never makes an entry stale by itself.
fn apply_basis(q: &Q, cwd: &str, result: &mut [Value]) -> Result<()> {
    let ids: Vec<i64> = result.iter().filter_map(|v| v["id"].as_i64()).collect();
    let bases = store::bases(q.conn(), &ids)?;
    if bases.is_empty() {
        return Ok(());
    }
    let root = Path::new(cwd);
    let mut left = BASIS_READ_BUDGET;
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut checked = 0;
    for value in result.iter_mut() {
        let Some(id) = value["id"].as_i64() else { continue };
        let Some(basis) = bases.get(&id).filter(|b| !b.paths.is_empty()) else { continue };
        let mut state = basis.state.clone();
        let mut changed: Vec<String> = vec![];
        if checked < MAX_BASIS_CHECKS {
            checked += 1;
            for recorded in &basis.paths {
                let current = cache
                    .entry(recorded.path.clone())
                    .or_insert_with(|| digest_path(root, &recorded.path, &mut left))
                    .clone();
                if recorded.sha256 != "unverifiable" && current != "unverifiable" && current != recorded.sha256 {
                    changed.push(recorded.path.clone());
                }
            }
            state = if changed.is_empty() { store::STATE_CURRENT } else { store::STATE_STALE }.into();
            if state != basis.state {
                q.run(
                    "UPDATE knowledge_basis SET state=?,verified=? WHERE entry=?",
                    &[&state, &now(), &id],
                )
                .ok();
            }
        }
        if state == store::STATE_STALE {
            value["status"] = json!("stale");
            if !changed.is_empty() {
                value["stale_paths"] = json!(changed);
            }
        }
    }
    // Stable: current entries keep their order, stale ones follow.
    result.sort_by_key(|v| v["status"] == "stale");
    Ok(())
}

/// Operator entry (`agent='operator'`). With a ticket, the ticket must belong
/// to the project and `knowledge.add` is logged on its root job.
pub fn operator_add(q: &Q, cwd: &str, kind: &str, body: &str, ticket: Option<i64>) -> Result<i64> {
    if !KINDS.contains(&kind) {
        bail!("知识类型无效");
    }
    let Some(body) = valid_body(body) else {
        bail!("知识内容为空、含控制字符或超过 {MAX_BODY} 字");
    };
    let root = match ticket {
        Some(t) => {
            let row = q
                .one("SELECT cwd FROM tickets WHERE id=?", &[&t])?
                .ok_or_else(|| anyhow!("工单不存在"))?;
            if str_of(&row, "cwd") != cwd {
                bail!("工单不属于该项目");
            }
            Some(q.root(t)?)
        }
        None => None,
    };
    let entry = NewEntry {
        cwd,
        kind,
        body,
        ticket,
        job: None,
        agent: "operator",
    };
    insert(q, &entry, root.as_ref().map(who_of))?.ok_or_else(|| anyhow!("项目知识库已有相同内容"))
}

/// Archive or restore an entry. Logs `knowledge.archived {id, archived}` on
/// the root job of the entry's ticket when it has one.
pub fn set_archived(q: &Q, id: i64, archived: bool) -> Result<()> {
    let row = q
        .one(
            "SELECT cwd,sha256,ticket,archived FROM project_knowledge WHERE id=?",
            &[&id],
        )?
        .ok_or_else(|| anyhow!("知识条目不存在"))?;
    if (row["archived"].as_i64().unwrap_or(0) != 0) == archived {
        return Ok(());
    }
    if !archived
        && q.exists(
            "SELECT 1 FROM project_knowledge WHERE cwd=? AND sha256=? AND archived=0",
            &[&str_of(&row, "cwd"), &str_of(&row, "sha256")],
        )?
    {
        bail!("项目知识库已有相同内容的有效条目");
    }
    q.run(
        "UPDATE project_knowledge SET archived=? WHERE id=?",
        &[&(archived as i64), &id],
    )?;
    if !archived {
        // A restored entry is no longer superseded.
        q.run(
            "UPDATE knowledge_basis SET state='current',superseded_by=NULL WHERE entry=? AND state='superseded'",
            &[&id],
        )?;
    }
    if let Some(ticket) = row["ticket"].as_i64()
        && let Some(root) = q.one(
            "SELECT * FROM jobs WHERE ticket=? AND parent IS NULL",
            &[&ticket],
        )?
    {
        q.log(
            who_of(&root),
            "knowledge.archived",
            &json!({"id": id, "archived": archived}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_fields() {
        let ok = json!({"kind": "lesson", "text": "  用 cargo.cjs 构建\n不要直接 cargo  "});
        assert_eq!(
            parse_memo(&ok),
            Some(("lesson", "用 cargo.cjs 构建\n不要直接 cargo"))
        );
        assert!(parse_memo(&json!({"kind": "secret", "text": "x"})).is_none());
        assert!(parse_memo(&json!({"kind": "memory", "text": " \u{3000} "})).is_none());
        assert!(parse_memo(&json!({"kind": "memory", "text": "x", "id": "k"})).is_none());
        assert!(parse_memo(&json!({"kind": "memory", "text": "a\u{0}b"})).is_none());
        assert!(parse_memo(&json!({"kind": "memory", "text": "a\u{202e}b"})).is_none());
        assert!(parse_memo(&json!({"kind": "memory", "text": "字".repeat(MAX_BODY)})).is_some());
        assert!(
            parse_memo(&json!({"kind": "memory", "text": "😀".repeat(MAX_BODY / 2 + 1)})).is_none()
        );
        assert!(parse_memo(&json!(["memory", "x"])).is_none());
    }

    #[test]
    fn extended_memo_keys_are_optional_and_validated() {
        let two = json!({"kind": "memory", "text": "x"});
        assert_eq!(parse_memo_ext(&two), Some(Memo { kind: "memory", text: "x", paths: vec![], supersedes: None }));
        let full = json!({"kind": "lesson", "text": "x", "paths": ["a/b.rs", "a\\b.rs", "c.rs"], "supersedes": 7});
        let memo = parse_memo_ext(&full).unwrap();
        assert_eq!((memo.paths, memo.supersedes), (vec!["a/b.rs".to_string(), "c.rs".to_string()], Some(7)));
        for bad in [
            json!({"kind": "memory", "text": "x", "paths": ["../x"]}),
            json!({"kind": "memory", "text": "x", "paths": ["/abs"]}),
            json!({"kind": "memory", "text": "x", "paths": "a"}),
            json!({"kind": "memory", "text": "x", "paths": [1]}),
            json!({"kind": "memory", "text": "x", "paths": (0..9).map(|i| format!("f{i}")).collect::<Vec<_>>()}),
            json!({"kind": "memory", "text": "x", "supersedes": 0}),
            json!({"kind": "memory", "text": "x", "supersedes": "1"}),
            json!({"kind": "memory", "text": "x", "other": 1}),
        ] {
            assert!(parse_memo_ext(&bad).is_none(), "{bad}");
        }
        // The two-key parser is unchanged.
        assert!(parse_memo(&full).is_none());
    }

    #[test]
    fn path_digests_distinguish_missing_unverifiable_and_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut left = u64::MAX;
        assert_eq!(digest_path(dir.path(), "a.txt", &mut left), sa_store::delivery::sha256(b"abc"));
        assert_eq!(digest_path(dir.path(), "nope.txt", &mut left), "missing");
        assert_eq!(digest_path(dir.path(), "sub", &mut left), "unverifiable");
        let mut tiny = 2;
        assert_eq!(digest_path(dir.path(), "a.txt", &mut tiny), "unverifiable");
        std::fs::write(dir.path().join("big.bin"), vec![0u8; MAX_BASIS_FILE as usize + 1]).unwrap();
        assert_eq!(digest_path(dir.path(), "big.bin", &mut left), "unverifiable");
    }

    #[test]
    fn excerpts_are_utf16_bounded() {
        assert_eq!(excerpt("abc", 3), "abc");
        assert_eq!(excerpt("abcd", 3), "ab…");
        assert!(utf16_len(&excerpt(&"😀".repeat(10), 5)) <= 5);
        assert_eq!(one_line(" a \n\t b  "), "a b");
    }
}
