//! One `SA_TOOL` call with a durable receipt (`invoke`, X:298-386).
//!
//! Receipt states: `pending` → `done` | `error` | `uncertain`. A call that
//! was issued but did not return (abort, timeout, lost connection) is
//! `uncertain`: the tool may have acted. The receipt key is
//! `(job, logical turn, id)`; a replay with the identical request is a no-op.

use crate::mcp::{self, CANCELLED};
use crate::validate::nonempty;
use crate::{Inner, ProjectExtensions, directory, scope_in};
use anyhow::{Result, anyhow, ensure};
use sa_engine::db::{Q, Who};
use sa_engine::protocol::{digest, stringify};
use sa_engine::services::{InvokeArgs, InvokeResult};
use serde_json::{Map, Value, json};

/// Results larger than this go to `context-objects/<sha256>.mcp.json` (X:358).
const MAX_INLINE_RESULT: usize = 512 * 1024;
const MAX_REQUEST: usize = 64_000;
const DETAIL: &str = "该服务以当前用户执行，可能访问工作区外的文件和网络。";

/// `current()` (X:334-340): the root turn is still running and no
/// `user.follow` arrived after its admission.
fn current(q: &Q, a: &InvokeArgs) -> Result<bool> {
    q.exists(
        "SELECT 1 FROM jobs j WHERE id=? AND ticket=? AND turn=? AND state='running' AND parent IS NULL
      AND NOT EXISTS(SELECT 1 FROM events e WHERE e.ticket=j.ticket AND kind='user.follow' AND seq>?)",
        &[&a.job, &a.ticket, &a.turn, &a.admission],
    )
}

/// `binding()` (X:328-331): the server row at the bound revision, if enabled.
fn binding(
    q: &Q,
    a: &InvokeArgs,
    server: &str,
    revision: &str,
) -> Result<Option<Map<String, Value>>> {
    ensure!(!a.cwd.is_empty(), "MCP 调用缺少工作目录");
    let cwd = directory::ticket_directory(q, a.ticket)?;
    let requested = directory::real_directory(std::path::Path::new(&a.cwd))?;
    ensure!(cwd == requested, "MCP 调用工作目录与工单不一致");
    let row = q.one(
        "SELECT * FROM studio_servers WHERE id=? AND enabled AND revision=?",
        &[&server, &revision],
    )?;
    if let Some(row) = &row {
        ensure!(
            directory::available(row, Some(&cwd)),
            "此 MCP 服务在当前工作目录不可用"
        );
    }
    Ok(row)
}

impl Inner {
    fn current(&self, a: &InvokeArgs) -> Result<bool> {
        self.db.with(|q| current(q, a))
    }

    fn binding(
        &self,
        a: &InvokeArgs,
        server: &str,
        revision: &str,
    ) -> Result<Option<Map<String, Value>>> {
        self.db.with(|q| binding(q, a, server, revision))
    }

    /// Store an oversized result as an immutable object (X:358-366).
    fn store_object(&self, raw: &str) -> Result<Value> {
        let sha256 = digest(raw);
        let folder = self.data_dir.join("context-objects");
        std::fs::create_dir_all(&folder)?;
        let file = folder.join(format!("{sha256}.mcp.json"));
        let mut f = std::fs::File::create(&file)?;
        std::io::Write::write_all(&mut f, raw.as_bytes())?;
        f.sync_all()?;
        Ok(json!({"object_path": file.to_string_lossy(), "sha256": sha256, "bytes": raw.len()}))
    }
}

impl ProjectExtensions {
    /// [`Extensions::invoke`](sa_engine::services::Extensions::invoke):
    /// returns `new_receipt: false` for an exact replay, `true` once a
    /// receipt was written (whatever its final state). Errors are raised
    /// only before a receipt exists (invalid command, replay with different
    /// arguments, profile, missing grant, stale turn).
    pub async fn invoke_call(&self, a: InvokeArgs) -> Result<InvokeResult> {
        let inner = &self.inner;
        let cmd = &a.command;
        let empty = json!({});
        let args = match cmd.get("arguments") {
            None => &empty,
            Some(v) => v,
        };
        ensure!(
            nonempty(cmd.get("id"), 96)
                && nonempty(cmd.get("server"), 100)
                && nonempty(cmd.get("tool"), 128)
                && args.is_object(),
            "MCP 调用格式无效"
        );
        let (id, server, tool) = (
            cmd["id"].as_str().unwrap_or_default(),
            cmd["server"].as_str().unwrap_or_default(),
            cmd["tool"].as_str().unwrap_or_default(),
        );
        let request = stringify(&json!({"server": server, "tool": tool, "arguments": args}));
        ensure!(request.len() <= MAX_REQUEST, "MCP 参数过大");
        let existing = inner.db.with(|q| {
            q.one(
                "SELECT request,state FROM studio_tool_receipts WHERE job=? AND logical=? AND key=?",
                &[&a.job, &a.logical_turn, &id],
            )
        })?;
        if let Some(row) = existing {
            ensure!(
                row["request"].as_str() == Some(request.as_str()),
                "MCP 幂等标识已用于不同参数"
            );
            return Ok(InvokeResult { new_receipt: false });
        }
        ensure!(
            !matches!(a.profile.as_str(), "readonly" | "legacy"),
            "当前权限不允许 MCP 调用"
        );
        let scope = inner.db.with(|q| scope_in(q, a.ticket))?;
        let allowed = scope["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| {
                s["id"].as_str() == Some(server)
                    && s["tools"]
                        .as_array()
                        .is_some_and(|t| t.iter().any(|t| t["name"].as_str() == Some(tool)))
            })
            .cloned();
        let Some(allowed) = allowed else {
            return Err(anyhow!("此工具未获工单授权"));
        };
        let revision = allowed["revision"].as_str().unwrap_or_default().to_string();
        let label = allowed["name"].as_str().unwrap_or_default().to_string();
        inner.binding(&a, server, &revision)?;
        ensure!(!a.cancel.is_cancelled(), CANCELLED);
        ensure!(inner.current(&a)?, "旧轮次已停止");
        inner.db.with(|q| {
            q.run(
                "INSERT INTO studio_tool_receipts VALUES(?,?,?,?,NULL,'pending')",
                &[&a.job, &a.logical_turn, &id, &request],
            )
        })?;
        let who = Who {
            ticket: a.ticket,
            id: a.job,
            turn: a.turn,
        };
        let mut issued = false;
        let mut returned = false;
        let outcome: Result<Value> = async {
            ensure!(inner.binding(&a, server, &revision)?.is_some(), "MCP 服务已禁用或已更改");
            if !matches!(a.profile.as_str(), "full" | "auto") {
                let action = json!({"tool": format!("MCP {label} / {tool}"), "arguments": args, "detail": DETAIL});
                ensure!((a.approve)(action).await, "用户未批准调用");
            }
            ensure!(!a.cancel.is_cancelled(), CANCELLED);
            ensure!(inner.current(&a)?, "旧轮次已停止");
            let Some(row) = inner.binding(&a, server, &revision)? else {
                return Err(anyhow!("MCP 服务已禁用或已更改"));
            };
            let config = mcp::ServerConfig::from_row(&row)?;
            let mut lease = inner.pool.acquire(a.ticket, server, &revision, &a.cwd, &config, &a.cancel).await?;
            let revoked = lease.revoked();
            let called = async {
                ensure!(!a.cancel.is_cancelled(), CANCELLED);
                ensure!(
                    inner.current(&a)? && inner.binding(&a, server, &revision)?.is_some(),
                    "指令或服务授权已改变"
                );
                inner
                    .db
                    .with(|q| q.log(who, "mcp.started", &json!({"id": id, "server": server, "tool": tool})))?;
                issued = true;
                tokio::select! {
                    biased;
                    () = revoked.cancelled() => Err(anyhow!("MCP 连接授权已撤销")),
                    result = mcp::call_tool(lease.client(), tool, args, &a.cancel) => result,
                }
            }
            .await;
            // Only a returned protocol result may retain the connection. A
            // transport failure/timeout/cancellation never retries the tool.
            if called.is_ok() && !a.cancel.is_cancelled() && !revoked.is_cancelled() {
                lease.keep();
            }
            drop(lease);
            let result = called?;
            returned = true;
            Ok(result)
        }
        .await;
        let (state, result) = match outcome {
            Ok(result) => {
                let state = if result.get("isError").and_then(Value::as_bool) == Some(true) {
                    "error"
                } else {
                    "done"
                };
                let raw = stringify(&result);
                if raw.len() <= MAX_INLINE_RESULT {
                    (state, result)
                } else {
                    match inner.store_object(&raw) {
                        Ok(object) => (state, object),
                        Err(e) => ("error", json!({"error": e.to_string()})),
                    }
                }
            }
            Err(e) => (
                if issued && !returned {
                    "uncertain"
                } else {
                    "error"
                },
                json!({"error": e.to_string()}),
            ),
        };
        inner.db.tx(|q| {
            q.run(
                "UPDATE studio_tool_receipts SET state=?,result=? WHERE job=? AND logical=? AND key=?",
                &[&state, &stringify(&result), &a.job, &a.logical_turn, &id],
            )?;
            let seq = q.log(
                who,
                "mcp.result",
                &json!({"id": id, "server": server, "tool": tool, "state": state, "result": result}),
            )?;
            if returned && a.admission != 0 && current(q, &a)? {
                q.run(
                    "UPDATE continuous_runs SET probe=?,success=CASE WHEN ?='done' THEN ? ELSE success END
        WHERE ticket=? AND phase='discover' AND boundary_seq<?",
                    &[&seq, &state, &seq, &a.ticket, &a.admission],
                )?;
            }
            Ok(())
        })?;
        Ok(InvokeResult { new_receipt: true })
    }
}
