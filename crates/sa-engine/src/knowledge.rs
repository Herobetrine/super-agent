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
use sa_store::knowledge::{self as store, KINDS, KnowledgeEntry, MAX_BODY};
use serde_json::{Value, json};
use std::collections::HashSet;
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
    let mut result = vec![];
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
    Ok(result)
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
    fn excerpts_are_utf16_bounded() {
        assert_eq!(excerpt("abc", 3), "abc");
        assert_eq!(excerpt("abcd", 3), "ab…");
        assert!(utf16_len(&excerpt(&"😀".repeat(10), 5)) <= 5);
        assert_eq!(one_line(" a \n\t b  "), "a b");
    }
}
