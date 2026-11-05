//! Admission snapshot of a job and the per-turn envelope (S:302-375).

use crate::db::{Q, Who, int_of, opt_int, opt_str, str_of};
use crate::adapter::AGENTS;
use crate::protocol as p;
use anyhow::Result;
use serde_json::{Map, Value, json};

/// The candidate row selected at admission (E:143-151) plus `admission`.
#[derive(Clone, Debug)]
pub struct JobCtx {
    pub id: i64,
    pub ticket: i64,
    pub parent: Option<i64>,
    pub sender: Option<String>,
    pub agent: String,
    pub body: String,
    pub writing: bool,
    pub depth: i64,
    pub turn: i64,
    pub role: String,
    pub result: Option<String>,
    pub session: Option<String>,
    pub dispatch_turn: Option<i64>,
    pub route_override: Option<String>,
    pub permission_override: Option<String>,
    pub cwd: String,
    pub claude_route: String,
    pub follow_config: i64,
    pub agent_routes: String,
    pub allow_lead_routing: bool,
    pub writes: bool,
    pub ticket_profile: String,
    pub profile: String,
    pub auto_cycles: i64,
    pub bound_route: Option<String>,
    pub continuous: bool,
    pub floor: i64,
    /// Seq of this turn's `turn.admitted` event.
    pub admission: i64,
}

impl JobCtx {
    pub fn from_row(r: &Map<String, Value>) -> Self {
        Self {
            id: int_of(r, "id"),
            ticket: int_of(r, "ticket"),
            parent: opt_int(r, "parent"),
            sender: opt_str(r, "sender"),
            agent: str_of(r, "agent").into(),
            body: str_of(r, "body").into(),
            writing: int_of(r, "writing") != 0,
            depth: int_of(r, "depth"),
            turn: int_of(r, "turn"),
            role: str_of(r, "role").into(),
            result: opt_str(r, "result"),
            session: opt_str(r, "session"),
            dispatch_turn: opt_int(r, "dispatch_turn"),
            route_override: opt_str(r, "route_override"),
            permission_override: opt_str(r, "permission_override"),
            cwd: str_of(r, "cwd").into(),
            claude_route: str_of(r, "claude_route").into(),
            follow_config: int_of(r, "follow_config"),
            agent_routes: opt_str(r, "agent_routes").unwrap_or_else(|| "{}".into()),
            allow_lead_routing: int_of(r, "allow_lead_routing") != 0,
            writes: int_of(r, "writes") != 0,
            ticket_profile: str_of(r, "ticket_profile").into(),
            profile: str_of(r, "profile").into(),
            auto_cycles: int_of(r, "auto_cycles"),
            bound_route: opt_str(r, "bound_route"),
            continuous: r.get("continuous_ticket").is_some_and(|v| !v.is_null()),
            floor: int_of(r, "floor"),
            admission: 0,
        }
    }

    pub fn who(&self) -> Who {
        Who {
            ticket: self.ticket,
            id: self.id,
            turn: self.turn,
        }
    }

    /// Idempotency key for SA_SEND/NOTE/PROGRESS/TOOL: `dispatch_turn || turn`.
    pub fn logical(&self) -> i64 {
        match self.dispatch_turn {
            Some(t) if t != 0 => t,
            _ => self.turn,
        }
    }

    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// `profile==='auto' && parent===null` (the auto root of fail/commit).
    pub fn auto_root(&self) -> bool {
        self.profile == "auto" && self.parent.is_none()
    }

    /// Continuous primary: continuous ticket ∧ root (E:265).
    pub fn primary(&self) -> bool {
        self.continuous && self.parent.is_none()
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// Resilience fields of a child report (`failure`), derived from journaled
/// retry events since the child's last reply. `None` when not applicable.
fn failure_of(q: &Q, child: i64, state: &str) -> Result<Option<Value>> {
    let rows = q.all(
        "SELECT kind,payload FROM events WHERE job=? AND kind IN ('turn.retry_scheduled','route.rotated','turn.gave_up')
      AND seq>coalesce((SELECT max(seq) FROM events WHERE job=? AND kind='reply'),0) ORDER BY seq",
        &[&child, &child],
    )?;
    let failed = matches!(state, "failed" | "draining");
    if rows.is_empty() && !failed {
        return Ok(None);
    }
    let mut attempts = 0i64;
    let mut class = Value::Null;
    let mut gave_up = false;
    for r in &rows {
        let payload = parse(str_of(r, "payload"));
        match str_of(r, "kind") {
            "turn.gave_up" => {
                gave_up = true;
                class = payload.get("class").cloned().unwrap_or(Value::Null);
                attempts = attempts.max(payload.get("attempts").and_then(Value::as_i64).unwrap_or(0));
            }
            "route.rotated" => {
                attempts += 1;
                class = payload.get("reason").cloned().unwrap_or(Value::Null);
            }
            _ => {
                attempts += 1;
                class = payload.get("class").cloned().unwrap_or(Value::Null);
            }
        }
    }
    if class.is_null() && failed {
        class = json!("other");
    }
    let retryable = !gave_up && !failed && !p::is_terminal(state);
    Ok(Some(json!({"class": class, "attempts": attempts, "retryable": retryable})))
}

/// `recovery` envelope field: the latest interruption of this job since its
/// last reply (or follow), or null. Replaces overwriting `previous_reply`.
fn recovery_of(q: &Q, j: &JobCtx) -> Result<Value> {
    let row = q.one(
        "SELECT kind,payload,turn FROM events WHERE job=? AND kind IN
      ('turn.retry_scheduled','route.rotated','turn.route_wait','auto.recover','interrupted')
      AND seq>coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow')),0)
      ORDER BY seq DESC LIMIT 1",
        &[&j.id, &j.id],
    )?;
    let Some(row) = row else { return Ok(Value::Null) };
    let payload = parse(str_of(&row, "payload"));
    let kind = str_of(&row, "kind");
    let class = match kind {
        "route.rotated" => payload.get("reason").cloned(),
        "auto.recover" | "interrupted" => Some(json!("interrupted")),
        _ => payload.get("class").cloned(),
    }
    .unwrap_or(Value::Null);
    let attempt = q.int(
        "SELECT count(*) FROM events WHERE job=? AND kind IN ('turn.retry_scheduled','route.rotated')
      AND seq>coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow')),0)",
        &[&j.id, &j.id],
    )?;
    Ok(json!({
        "kind": kind,
        "class": class,
        "attempt": attempt,
        "previous_turn": row.get("turn").cloned().unwrap_or(Value::Null),
        "session_resumed": j.session.is_some(),
        "side_effects_possible": true,
    }))
}

/// `envelope(j)` (S:302-375) in exact key order, plus the Rust-port fields
/// `harness_health` and `recovery` after `autonomy`, and `project_knowledge`
/// after them when the project has active entries.
pub fn envelope(q: &Q, j: &JobCtx, harness_health: Value) -> Result<Map<String, Value>> {
    let t = q
        .one("SELECT * FROM tickets WHERE id=?", &[&j.ticket])?
        .unwrap_or_default();
    let c = q.one("SELECT * FROM continuous_runs WHERE ticket=?", &[&j.ticket])?;
    let floor = c.as_ref().map(|c| int_of(c, "boundary_job")).unwrap_or(0);
    let mut reports = vec![];
    for r in q.all(
        "SELECT j.id,j.agent AS sender,j.role,j.body AS assignment,j.state,j.result,f.replacement
      FROM jobs j LEFT JOIN job_failovers f ON f.original=j.id WHERE j.parent=? AND j.id>? ORDER BY j.id",
        &[&j.id, &floor],
    )? {
        let failure = failure_of(q, int_of(&r, "id"), str_of(&r, "state"))?;
        let mut r = r;
        let replacement = r.remove("replacement").filter(|v| !v.is_null());
        if let Some(f) = failure {
            r.insert("failure".into(), f);
        }
        // Rust-port: an automatically failed-over child names its sibling.
        if let Some(by) = replacement {
            r.insert("replaced_by".into(), by);
        }
        reports.push(Value::Object(r));
    }
    let recent: Vec<Value> = if c.is_some() {
        let mut rows = q.all(
            "SELECT seq,kind,payload FROM events WHERE ticket=? AND kind IN ('auto.milestone','auto.discovered')
      ORDER BY seq DESC LIMIT 8",
            &[&j.ticket],
        )?;
        rows.reverse();
        rows.iter()
            .map(|r| json!({"seq": r["seq"], "kind": r["kind"], "data": parse(str_of(r, "payload"))}))
            .collect()
    } else {
        vec![]
    };
    let next = q.one(
        "SELECT payload FROM events WHERE job=? AND kind='auto.next' AND seq>coalesce(
      (SELECT max(seq) FROM events WHERE ticket=? AND kind IN ('user.follow','auto.finish','auto.milestone','auto.discovered','auto.exhausted')),0)
      ORDER BY seq DESC LIMIT 1",
        &[&j.id, &j.ticket],
    )?;
    let files = q.one("SELECT manifest FROM job_files WHERE job=?", &[&j.id])?;
    let objective = match t.get("objective") {
        Some(Value::String(s)) if !s.is_empty() => json!(s),
        _ => t.get("goal").cloned().unwrap_or(Value::Null),
    };
    let routing = int_of(&t, "allow_lead_routing") != 0;
    let agent_routes = parse(t.get("agent_routes").and_then(Value::as_str).unwrap_or("{}"));
    let selected = j
        .route_override
        .as_deref()
        .map(parse)
        .filter(p::truthy_value)
        .or_else(|| agent_routes.get(&j.agent).cloned().filter(p::truthy_value))
        .unwrap_or(Value::Null);
    let mut caps = Map::new();
    for a in AGENTS {
        caps.insert(
            a.into(),
            json!({"read_project": true, "write_project": p::can_write(a, &j.profile),
                "native_images": crate::images::supports_native_images(a)}),
        );
    }
    let user_updates: Vec<Value> = q
        .all(
            "SELECT payload FROM events WHERE ticket=? AND kind='user.follow' ORDER BY seq",
            &[&j.ticket],
        )?
        .iter()
        .map(|r| parse(str_of(r, "payload")))
        .collect();
    let roster: Vec<Value> = q
        .all(
            "SELECT id AS job,agent,state,role,parent FROM jobs WHERE ticket=? AND (parent IS NULL OR id>?) ORDER BY id",
            &[&j.ticket, &floor],
        )?
        .into_iter()
        .map(Value::Object)
        .collect();
    let autonomy = match &c {
        Some(c) => json!({
            "phase": c["phase"],
            "milestone": c["milestone"],
            "learning": c["learning"],
            "recent": recent,
            "last_inspection_seq": c["probe"],
        }),
        None => Value::Null,
    };
    let mut m = Map::new();
    m.insert("ticket".into(), json!(j.ticket));
    m.insert("job".into(), json!(j.id));
    m.insert("turn".into(), json!(j.turn));
    m.insert("depth".into(), json!(j.depth));
    m.insert("agent".into(), json!(j.agent));
    m.insert("sender".into(), json!(j.sender));
    m.insert("role".into(), json!(j.role));
    m.insert("project".into(), t.get("cwd").cloned().unwrap_or(Value::Null));
    m.insert("goal".into(), t.get("goal").cloned().unwrap_or(Value::Null));
    m.insert("assignment".into(), json!(j.body));
    m.insert("objective".into(), objective);
    m.insert("previous_reply".into(), json!(j.result));
    m.insert("reports".into(), Value::Array(reports));
    m.insert("permission_profile".into(), json!(j.profile));
    m.insert("allow_lead_routing".into(), json!(routing && j.is_root()));
    m.insert(
        "allowed_child_permissions".into(),
        json!(if routing && j.is_root() {
            p::child_profiles(&j.profile)
        } else {
            vec![]
        }),
    );
    m.insert(
        "can_assign_write".into(),
        json!(int_of(&t, "writes") != 0 && j.profile != "readonly"),
    );
    m.insert(
        "this_turn_can_write".into(),
        json!(p::is_full(&j.profile) || j.writing),
    );
    m.insert("harness_capabilities".into(), Value::Object(caps));
    m.insert("selected_execution".into(), selected);
    m.insert(
        "claude_route".into(),
        t.get("claude_route").cloned().unwrap_or(Value::Null),
    );
    m.insert(
        "files".into(),
        files
            .map(|f| parse(str_of(&f, "manifest")))
            .unwrap_or_else(|| json!([])),
    );
    m.insert("notes".into(), Value::Array(q.notes(j.id, false)?));
    m.insert("user_updates".into(), Value::Array(user_updates));
    m.insert("roster".into(), Value::Array(roster));
    m.insert(
        "next_step".into(),
        next.map(|n| parse(str_of(&n, "payload"))).unwrap_or(Value::Null),
    );
    m.insert("autonomy".into(), autonomy);
    m.insert("harness_health".into(), harness_health);
    m.insert("recovery".into(), recovery_of(q, j)?);
    let knowledge = crate::knowledge::context(q, &j.cwd)?;
    if !knowledge.is_empty() {
        m.insert("project_knowledge".into(), Value::Array(knowledge));
    }
    Ok(m)
}
