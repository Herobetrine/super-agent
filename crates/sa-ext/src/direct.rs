//! Tool access for every task of a ticket through the collaboration bus
//! (`docs/dev/collab-bus.md`). Unlike `SA_TOOL`, the result returns within
//! the caller's turn. Authorization is the ticket's MCP scope, checked with
//! the same binding, directory and pool rules as [`invoke`](crate::invoke).

use crate::mcp::{self, CANCELLED};
use crate::{ProjectExtensions, directory, scope_in};
use anyhow::{Result, anyhow, ensure};
use sa_engine::db::Who;
use sa_engine::protocol::stringify;
use sa_engine::services::InvokeArgs;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Longest tool description returned by [`ProjectExtensions::catalog_for`]
/// without asking for one tool's full entry.
const PREVIEW: usize = 240;
const MAX_REQUEST: usize = 64_000;

/// The live MCP grant of the bus caller.
#[derive(Clone, Debug)]
pub struct DirectCaller {
    pub ticket: i64,
    pub job: i64,
    pub turn: i64,
}

fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(PREVIEW).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

impl ProjectExtensions {
    fn caller_profile(&self, c: &DirectCaller) -> Result<String> {
        self.inner.db.with(|q| {
            let row = q
                .one(
                    "SELECT coalesce(j.permission_override,t.permission_profile) AS profile FROM jobs j
                     JOIN tickets t ON t.id=j.ticket WHERE j.id=? AND j.ticket=? AND j.turn=? AND j.state='running'",
                    &[&c.job, &c.ticket, &c.turn],
                )?
                .ok_or_else(|| anyhow!("本轮已结束"))?;
            Ok(row["profile"].as_str().unwrap_or_default().to_string())
        })
    }

    /// Tools the ticket authorized and that are currently available. Without
    /// `server`/`tool` each entry carries a description preview only; naming
    /// one tool returns its full description and input schema.
    pub fn catalog_for(&self, c: &DirectCaller, server: Option<&str>, tool: Option<&str>) -> Result<Value> {
        let profile = self.caller_profile(c)?;
        ensure!(!matches!(profile.as_str(), "readonly" | "legacy"), "当前权限不允许 MCP 调用");
        let context = self.context_value(c.ticket, &profile)?;
        let mut out = vec![];
        for s in context["mcp"].as_array().into_iter().flatten() {
            if server.is_some_and(|id| s["server"].as_str() != Some(id)) {
                continue;
            }
            for t in s["tools"].as_array().into_iter().flatten() {
                if tool.is_some_and(|name| t["name"].as_str() != Some(name)) {
                    continue;
                }
                let description = t["description"].as_str().unwrap_or_default();
                let mut entry = json!({"server": s["server"], "server_name": s["name"], "tool": t["name"]});
                if tool.is_some() {
                    entry["description"] = json!(description);
                    entry["input_schema"] = t.get("inputSchema").cloned().unwrap_or(json!({}));
                } else {
                    entry["description"] = json!(preview(description));
                }
                out.push(entry);
            }
        }
        Ok(json!({"tools": out}))
    }

    /// Call one authorized tool and return its MCP result. Only full-access
    /// and auto turns may call: the bus has no approval channel.
    pub async fn call_direct(
        &self,
        c: &DirectCaller,
        server: &str,
        tool: &str,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let inner = &self.inner;
        ensure!(args.is_object(), "`arguments` 必须是对象");
        let request = stringify(&json!({"server": server, "tool": tool, "arguments": args}));
        ensure!(request.len() <= MAX_REQUEST, "MCP 参数过大");
        let profile = self.caller_profile(c)?;
        ensure!(
            matches!(profile.as_str(), "full" | "auto"),
            "协作总线只在完全访问或自动档位调用 MCP 工具；其他档位请由主控通过 SA_TOOL 调用"
        );
        let (scope, cwd) = inner.db.with(|q| Ok((scope_in(q, c.ticket)?, directory::ticket_directory(q, c.ticket)?)))?;
        let allowed = scope["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| {
                s["id"].as_str() == Some(server)
                    && s["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"].as_str() == Some(tool)))
            })
            .cloned()
            .ok_or_else(|| anyhow!("此工具未获工单授权"))?;
        let revision = allowed["revision"].as_str().unwrap_or_default().to_string();
        // Reuse the SA_TOOL binding check: same ticket directory, enabled
        // server at the granted revision, directory scope.
        let probe = InvokeArgs {
            ticket: c.ticket,
            job: c.job,
            turn: c.turn,
            admission: 0,
            logical_turn: c.turn,
            profile: profile.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            cancel: cancel.clone(),
            approve: Arc::new(|_| Box::pin(async { false })),
            command: Value::Null,
        };
        let row = inner.binding(&probe, server, &revision)?.ok_or_else(|| anyhow!("MCP 服务已禁用或已更改"))?;
        ensure!(!cancel.is_cancelled(), CANCELLED);
        let who = Who { ticket: c.ticket, id: c.job, turn: c.turn };
        let id = format!("bus-{}", uuid::Uuid::new_v4().simple());
        inner.db.with(|q| {
            q.log(who, "mcp.started", &json!({"id": id, "server": server, "tool": tool, "via": "bus"}))
        })?;
        let config = mcp::ServerConfig::from_row(&row)?;
        let outcome: Result<Value> = async {
            let mut lease = inner.pool.acquire(c.ticket, server, &revision, &probe.cwd, &config, cancel).await?;
            let revoked = lease.revoked();
            let result = tokio::select! {
                biased;
                () = revoked.cancelled() => Err(anyhow!("MCP 连接授权已撤销")),
                result = mcp::call_tool(lease.client(), tool, args, cancel) => result,
            };
            if result.is_ok() && !cancel.is_cancelled() && !revoked.is_cancelled() {
                lease.keep();
            }
            result
        }
        .await;
        let (state, logged) = match &outcome {
            Ok(result) if result.get("isError").and_then(Value::as_bool) == Some(true) => ("error", result.clone()),
            Ok(result) => ("done", result.clone()),
            Err(e) => ("error", json!({"error": e.to_string()})),
        };
        let logged = if stringify(&logged).len() > 512 * 1024 { inner.store_object(&stringify(&logged))? } else { logged };
        inner.db.with(|q| {
            q.log(
                who,
                "mcp.result",
                &json!({"id": id, "server": server, "tool": tool, "state": state, "result": logged, "via": "bus"}),
            )
        })?;
        outcome
    }
}
