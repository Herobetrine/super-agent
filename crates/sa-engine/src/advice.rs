//! Cross-harness advisor (`docs/dev/collab-bus.md`): any task may consult
//! another harness and model through the collaboration bus. The advisor runs
//! one read-only turn in the ticket directory and returns guidance; it never
//! writes files or talks to the operator.

use crate::adapter::Route;
use crate::db::{Q, int_of, str_of};
use crate::protocol as p;
use anyhow::{Result, anyhow};
use serde_json::Value;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS bus_advisor(
  scope TEXT PRIMARY KEY, agent TEXT NOT NULL, route TEXT NOT NULL);";

/// Longest brief accepted from a caller, in bytes.
pub const MAX_BRIEF: usize = 32 * 1024;

/// Fixed preamble of every advisor turn; the caller's brief follows, so the
/// shared prefix stays cache-stable.
pub const GUIDE: &str = "You are the advisor of another AI agent that is working on a software task in this project. \
It paused to ask for your guidance; its brief follows. You run read-only: inspect files and search the code as needed to check its claims, \
but do not modify anything. Reply with concrete guidance it can act on now: the approach you recommend and why, the risks or mistakes you see, \
and the checks it should run before calling the work done. If the brief lacks information you need, say exactly what is missing. \
Answer in the language of the brief and keep it under 400 words unless the decision truly needs more.";

/// Advisor selected for a ticket.
#[derive(Clone, Debug, PartialEq)]
pub struct Advisor {
    pub agent: String,
    pub route: Route,
    /// `ticket`, `global` or `lead`.
    pub source: &'static str,
}

fn scope(ticket: Option<i64>) -> String {
    match ticket {
        Some(id) => format!("ticket:{id}"),
        None => "global".into(),
    }
}

fn stored(q: &Q, scope: &str) -> Result<Option<(String, Route)>> {
    let Some(row) = q.one("SELECT agent,route FROM bus_advisor WHERE scope=?", &[&scope])? else {
        return Ok(None);
    };
    let route: Value = serde_json::from_str(str_of(&row, "route"))?;
    let route = p::route(Some(&route)).map_err(|e| anyhow!("顾问路由无效：{e}"))?;
    Ok(Some((str_of(&row, "agent").to_string(), route)))
}

/// Ticket override, then the global default, then the lead's current route.
pub(crate) fn choose(q: &Q, ticket: i64) -> Result<Advisor> {
    if let Some((agent, route)) = stored(q, &scope(Some(ticket)))? {
        return Ok(Advisor { agent, route, source: "ticket" });
    }
    if let Some((agent, route)) = stored(q, &scope(None))? {
        return Ok(Advisor { agent, route, source: "global" });
    }
    let root = q.root(ticket)?;
    let bound = q
        .one("SELECT route FROM native_routes WHERE job=?", &[&int_of(&root, "id")])?
        .ok_or_else(|| anyhow!("主控还没有运行过，无法沿用它的路由；请在模型窗口设置顾问"))?;
    let route: Value = serde_json::from_str(str_of(&bound, "route"))?;
    let route = p::route(Some(&route)).map_err(|e| anyhow!("主控路由无效：{e}"))?;
    Ok(Advisor { agent: str_of(&root, "agent").to_string(), route, source: "lead" })
}

/// The stored choice for a scope (`None` ticket = global), for settings UIs.
pub fn get(q: &Q, ticket: Option<i64>) -> Result<Option<(String, Route)>> {
    stored(q, &scope(ticket))
}

/// Store or clear (`None`) the advisor of a scope.
pub(crate) fn set(q: &Q, ticket: Option<i64>, choice: Option<(&str, &Route)>) -> Result<()> {
    let scope = scope(ticket);
    match choice {
        None => {
            q.run("DELETE FROM bus_advisor WHERE scope=?", &[&scope])?;
        }
        Some((agent, route)) => {
            let route = serde_json::to_value(route)?;
            q.run(
                "INSERT INTO bus_advisor(scope,agent,route) VALUES(?,?,?)
                 ON CONFLICT(scope) DO UPDATE SET agent=excluded.agent,route=excluded.route",
                &[&scope, &agent, &p::stringify(&route)],
            )?;
        }
    }
    Ok(())
}
