//! Super Agent project extensions (port of `runtime/extensions.cjs` and
//! `runtime/skills.cjs`): the skill catalog, configured MCP servers, the
//! per-ticket extension scope and durable `SA_TOOL` receipts.
//!
//! [`ProjectExtensions`] serves two callers:
//! - the scheduler, through [`sa_engine::services::Extensions`]
//!   (`prepare`/`bind`/`context`/`results`/`invoke`);
//! - the desktop, through the management API mirroring `runtime/studio.cjs`
//!   (`extensions`, `extensionDefaults`, `mcpSave`, `mcpRemove`, `mcpProbe`,
//!   `registerSkill`, `forgetSkill`, `skillPreview`, snapshot `extensions`).
//!   Results are `serde_json::Value`s shaped like the JS results.
//!
//! Tables and column layouts are those of the JS (X:100-107), so an existing
//! `hub.sqlite3` stays compatible in both directions. The service keeps its
//! own SQLite connection to the journal (WAL); `bind` writes through the
//! engine's connection inside the ticket creation transaction.
//!
//! Secrets: an HTTP server stores only the *name* of an `MCP_*` environment
//! variable; the token is read at connect time and never stored, journaled
//! or returned. stdio servers inherit only the MCP SDK's safe default
//! environment.

pub mod mcp;
pub mod skills;
pub mod validate;

mod invoke;
mod pool;

use anyhow::{Result, bail, ensure};
use futures::future::BoxFuture;
use rusqlite::Connection;
use sa_engine::db::{Db, Q, str_of};
use sa_engine::protocol::{stringify, utf16_len};
use sa_engine::services::{Extensions, InvokeArgs, InvokeResult};
use serde_json::{Map, Value, json};
use skills::{IMPORTED, SkillHomes};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use validate::{Choices, choices, endpoint, executable, nonempty, token_env};

/// Extension tables (X:100-106).
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS studio_skill_dirs(id TEXT PRIMARY KEY,folder TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_servers(id TEXT PRIMARY KEY,name TEXT NOT NULL,command TEXT NOT NULL,args TEXT NOT NULL,
        enabled INTEGER NOT NULL,revision TEXT NOT NULL,tools TEXT,transport TEXT NOT NULL DEFAULT 'stdio',url TEXT NOT NULL DEFAULT '',token_env TEXT NOT NULL DEFAULT '');
      CREATE TABLE IF NOT EXISTS studio_extension_defaults(singleton INTEGER PRIMARY KEY CHECK(singleton=1),selected TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_ticket_extensions(ticket INTEGER PRIMARY KEY REFERENCES tickets,scope TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_tool_receipts(job INTEGER NOT NULL REFERENCES jobs,logical INTEGER NOT NULL,key TEXT NOT NULL,
        request TEXT NOT NULL,result TEXT,state TEXT NOT NULL,PRIMARY KEY(job,logical,key));";

/// Columns added to `studio_servers` after its first release; added only
/// when an old journal lacks them.
const SERVER_ADDITIONS: [(&str, &str); 3] = [
    ("transport", "TEXT NOT NULL DEFAULT 'stdio'"),
    ("url", "TEXT NOT NULL DEFAULT ''"),
    ("token_env", "TEXT NOT NULL DEFAULT ''"),
];

const EMPTY_SCOPE: &str = r#"{"skills":[],"servers":[]}"#;
/// Largest tool catalog kept per server (X:225).
const MAX_TOOLS: usize = 150;
const MAX_CATALOG: usize = 256_000;
/// Distinct `tools/list` cursors accepted per probe (X:229).
const MAX_PAGES: usize = 16;

/// Host environment of the service.
#[derive(Clone, Debug)]
pub struct ExtOptions {
    /// Skill search roots (`~`, `$CODEX_HOME`).
    pub homes: SkillHomes,
    /// `PATH` used to resolve bare stdio server program names.
    pub path: Option<OsString>,
}

impl ExtOptions {
    pub fn from_process() -> Self {
        Self {
            homes: SkillHomes::from_process(),
            path: std::env::var_os("PATH"),
        }
    }
}

pub(crate) struct Inner {
    pub(crate) db: Db,
    pub(crate) data_dir: PathBuf,
    opts: ExtOptions,
    pub(crate) pool: pool::Pool,
}

/// The extension service. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct ProjectExtensions {
    pub(crate) inner: Arc<Inner>,
}

fn parse(text: &str) -> Result<Value> {
    Ok(serde_json::from_str(text)?)
}

fn text<'a>(row: &'a Map<String, Value>, key: &str) -> &'a str {
    str_of(row, key)
}

/// `clearDefault(group, id)` (X:122-126); call inside a transaction.
fn clear_default(q: &Q, group: &str, id: &str) -> Result<()> {
    let mut d = defaults_in(q)?;
    if let Some(Value::Array(list)) = d.get_mut(group) {
        list.retain(|x| x.as_str() != Some(id));
    }
    q.run(
        "INSERT OR REPLACE INTO studio_extension_defaults VALUES(1,?)",
        &[&stringify(&d)],
    )?;
    Ok(())
}

fn defaults_in(q: &Q) -> Result<Value> {
    let row = q.one(
        "SELECT selected FROM studio_extension_defaults WHERE singleton=1",
        &[],
    )?;
    parse(row.as_ref().map_or(EMPTY_SCOPE, |r| text(r, "selected")))
}

impl ProjectExtensions {
    /// Open the service on the journal directory (`<data_dir>/hub.sqlite3`,
    /// the same directory passed to [`sa_engine::Engine::open`]) with the
    /// process environment. Creates the extension tables if missing.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(data_dir, ExtOptions::from_process())
    }

    /// [`open`](Self::open) with explicit skill roots and `PATH`.
    pub fn open_with(data_dir: impl AsRef<Path>, opts: ExtOptions) -> Result<Self> {
        let data_dir = std::path::absolute(data_dir.as_ref())?;
        let db = Db::open(&data_dir)?;
        db.tx(|q| {
            q.conn().execute_batch(SCHEMA)?;
            let mut stmt = q.conn().prepare("PRAGMA table_info(studio_servers)")?;
            let existing: HashSet<String> = stmt
                .query_map([], |r| r.get::<_, String>("name"))?
                .collect::<rusqlite::Result<_>>()?;
            for (name, definition) in SERVER_ADDITIONS {
                if !existing.contains(name) {
                    q.conn().execute_batch(&format!(
                        "ALTER TABLE studio_servers ADD COLUMN {name} {definition}"
                    ))?;
                }
            }
            Ok(())
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                data_dir,
                opts,
                pool: pool::Pool::default(),
            }),
        })
    }

    /// Close the service's journal connection. In-flight MCP calls fail
    /// their final write; their server processes are torn down on drop.
    pub fn close(&self) {
        self.inner.pool.clear();
        self.inner.db.close();
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    fn imported_folders(&self, q: &Q) -> Result<Vec<String>> {
        Ok(q.all("SELECT folder FROM studio_skill_dirs", &[])?
            .iter()
            .map(|r| text(r, "folder").to_string())
            .collect())
    }

    fn scan(&self, cwd: Option<&Path>) -> Result<skills::Catalog> {
        let imported = self.inner.db.with(|q| self.imported_folders(q))?;
        Ok(skills::scan(&self.inner.opts.homes, cwd, &imported))
    }

    /// `defaults()` (X:108-113): `{skills: [id], servers: [id]}` preselected
    /// for new tickets.
    pub fn defaults(&self) -> Result<Value> {
        self.inner.db.with(defaults_in)
    }

    /// `extensions({cwd})` → `list(cwd)` (X:114-133):
    /// `{skills: [{id,name,description,source,imported}],
    ///   servers: [{id,name,command,args,enabled,tools,transport,url,tokenEnv}],
    ///   defaults}`; `tools` is the list of probed tool names or `null`.
    /// `cwd` adds the project's `.agents/skills` and `.claude/skills`.
    pub fn list(&self, cwd: Option<&Path>) -> Result<Value> {
        let skills: Vec<Value> = self.scan(cwd)?.iter().map(skills::Skill::meta).collect();
        let rows = self.inner.db.with(|q| {
            q.all(
                "SELECT id,name,command,args,enabled,tools,transport,url,token_env AS tokenEnv FROM studio_servers ORDER BY name",
                &[],
            )
        })?;
        let mut servers = vec![];
        for mut r in rows {
            let args = parse(text(&r, "args"))?;
            let enabled = r.get("enabled").and_then(Value::as_i64).unwrap_or(0) != 0;
            let tools = match r.get("tools").and_then(Value::as_str) {
                Some(t) => Value::Array(
                    parse(t)?
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|t| t.get("name").cloned().unwrap_or(Value::Null))
                        .collect(),
                ),
                None => Value::Null,
            };
            r.insert("args".into(), args);
            r.insert("enabled".into(), json!(enabled));
            r.insert("tools".into(), tools);
            servers.push(Value::Object(r));
        }
        Ok(json!({"skills": skills, "servers": servers, "defaults": self.defaults()?}))
    }

    /// `extensionDefaults({skills, servers, cwd})` → `setDefaults` (X:134-138).
    /// Every id must be usable now (same checks as ticket creation).
    pub fn set_defaults(&self, raw: &Value, cwd: Option<&Path>) -> Result<()> {
        let ids = choices(raw)?;
        self.prepare_choices(&ids, cwd)?;
        self.inner.db.with(|q| {
            q.run(
                "INSERT OR REPLACE INTO studio_extension_defaults VALUES(1,?)",
                &[&stringify(&ids.to_value())],
            )?;
            Ok(())
        })
    }

    /// `registerSkill({folder})` → `import(folder)` (X:139-145): remember a
    /// skill folder outside the scanned roots; returns the skill id.
    pub fn import_skill(&self, folder: &Path) -> Result<String> {
        let s = skills::read_skill(folder, IMPORTED, true)?;
        let dir = Path::new(&s.filename)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.inner.db.with(|q| {
            q.run(
                "INSERT OR REPLACE INTO studio_skill_dirs VALUES(?,?)",
                &[&s.id, &dir],
            )?;
            Ok(())
        })?;
        Ok(s.id)
    }

    /// `forgetSkill({id})` → `forget(id)` (X:146-151): drop an imported
    /// folder and its default selection.
    pub fn forget_skill(&self, id: &str) -> Result<()> {
        self.inner.db.tx(|q| {
            q.run("DELETE FROM studio_skill_dirs WHERE id=?", &[&id])?;
            clear_default(q, "skills", id)
        })
    }

    /// `skillPreview({id, cwd})` → `preview(id, cwd)` (X:152-156):
    /// `{name, body, source}`.
    pub fn preview(&self, id: &str, cwd: Option<&Path>) -> Result<Value> {
        let catalog = self.scan(cwd)?;
        let Some(s) = catalog.get(id) else {
            bail!("技能不存在");
        };
        Ok(json!({"name": s.name, "body": s.body, "source": s.source}))
    }

    /// `mcpSave(row)` → `saveServer(row)` (X:157-200); returns the server id.
    ///
    /// `row`: `{id?, name, transport?: "stdio"|"http", command?, args?: [str],
    /// url?, tokenEnv?, enabled?: bool}`. A new revision (which revokes every
    /// ticket binding) is issued when launch parameters change or the server
    /// is enabled/disabled; the probed tool list survives only unchanged
    /// launch parameters.
    pub fn save_server(&self, row: &Value) -> Result<String> {
        let empty = json!([]);
        let args = match row.get("args") {
            None => &empty,
            Some(v) => v,
        };
        let enabled = match row.get("enabled") {
            None => Some(false),
            Some(v) => v.as_bool(),
        };
        let args_ok = args.as_array().is_some_and(|a| {
            a.len() <= 64
                && a.iter().all(|s| {
                    s.as_str()
                        .is_some_and(|s| utf16_len(s) <= 8192 && !s.contains('\0'))
                })
        });
        let (true, Some(enabled), true) = (nonempty(row.get("name"), 100), enabled, args_ok) else {
            bail!("MCP 服务参数无效");
        };
        let name = row["name"].as_str().unwrap_or_default();
        let transport = match row.get("transport") {
            None | Some(Value::Null) => "stdio",
            Some(Value::String(t)) if t == "stdio" || t == "http" => t.as_str(),
            Some(_) => bail!("不支持的 MCP 传输"),
        };
        let command = if transport == "stdio" {
            executable(row.get("command"), self.inner.opts.path.as_deref())?
        } else {
            String::new()
        };
        let url = if transport == "http" {
            endpoint(row.get("url"))?.to_string()
        } else {
            String::new()
        };
        let token = match (transport, row.get("tokenEnv")) {
            ("http", Some(Value::String(s))) => s.clone(),
            ("http", None | Some(Value::Null | Value::Bool(false))) => String::new(),
            ("http", Some(v)) if v.as_f64() == Some(0.0) => String::new(),
            ("http", Some(_)) => bail!("请使用 MCP_ 开头的服务专用环境变量名"),
            _ => String::new(),
        };
        token_env(&token)?;
        let argv = stringify(args);
        let wanted = match row.get("id") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            None | Some(Value::Null | Value::Bool(false)) => None,
            Some(Value::String(_)) => None,
            Some(v) if v.as_f64() == Some(0.0) => None,
            Some(_) => bail!("此服务已删除"),
        };
        let saved = self.inner.db.tx(|q| {
            let old = match &wanted {
                Some(id) => {
                    let old = q.one("SELECT * FROM studio_servers WHERE id=?", &[id])?;
                    ensure!(old.is_some(), "此服务已删除");
                    old
                }
                None => None,
            };
            let same = old.as_ref().is_some_and(|o| {
                text(o, "args") == argv
                    && text(o, "command") == command
                    && text(o, "transport") == transport
                    && text(o, "url") == url
                    && text(o, "token_env") == token
            });
            let id = old
                .as_ref()
                .map_or_else(|| uuid::Uuid::new_v4().to_string(), |o| text(o, "id").to_string());
            // Changing launch parameters or disabling a service permanently
            // revokes existing bindings.
            let revision = match &old {
                Some(o) if same && o.get("enabled").and_then(Value::as_i64) == Some(enabled as i64) => {
                    text(o, "revision").to_string()
                }
                _ => uuid::Uuid::new_v4().to_string(),
            };
            let tools = old
                .as_ref()
                .filter(|_| same)
                .and_then(|o| o.get("tools").and_then(Value::as_str).map(str::to_string));
            q.run(
                "INSERT OR REPLACE INTO studio_servers(id,name,command,args,enabled,revision,tools,transport,url,token_env) VALUES(?,?,?,?,?,?,?,?,?,?)",
                &[&id, &name, &command, &argv, &(enabled as i64), &revision, &tools, &transport, &url, &token],
            )?;
            if !enabled || !same {
                clear_default(q, "servers", &id)?;
            }
            Ok((id, old.as_ref().is_some_and(|o| text(o, "revision") != revision)))
        })?;
        if saved.1 {
            self.inner.pool.revoke(&saved.0);
        }
        Ok(saved.0)
    }

    /// `mcpRemove({id})` → `remove(id)` (X:201-206).
    pub fn remove(&self, id: &str) -> Result<()> {
        self.inner.db.tx(|q| {
            q.run("DELETE FROM studio_servers WHERE id=?", &[&id])?;
            clear_default(q, "servers", id)
        })?;
        self.inner.pool.revoke(id);
        Ok(())
    }

    /// `mcpProbe({id})` → `probe(id)` (X:207-238): connect, page through
    /// `tools/list` (≤150 tools, ≤256 000 JSON chars, ≤16 cursors), store
    /// `[{name, description, inputSchema}]` if the server was not changed
    /// meanwhile, and return `[{name, description}]`.
    pub async fn probe(&self, id: &str) -> Result<Value> {
        let s = self
            .inner
            .db
            .with(|q| q.one("SELECT * FROM studio_servers WHERE id=?", &[&id]))?;
        let Some(s) = s else { bail!("未知服务") };
        let config = mcp::ServerConfig::from_row(&s)?;
        let client = mcp::connect(&config, &CancellationToken::new()).await?;
        let listed = async {
            let mut list: Vec<Value> = vec![];
            let mut cursor: Option<String> = None;
            let mut seen = HashSet::new();
            loop {
                let (tools, next) = mcp::list_page(&client, cursor.take()).await?;
                for t in tools {
                    let mut tool = Map::new();
                    tool.insert("name".into(), t.get("name").cloned().unwrap_or(Value::Null));
                    let description = t
                        .get("description")
                        .filter(|d| d.as_str().is_some_and(|d| !d.is_empty()));
                    tool.insert(
                        "description".into(),
                        description.cloned().unwrap_or(json!("")),
                    );
                    if let Some(schema) = t.get("inputSchema") {
                        tool.insert("inputSchema".into(), schema.clone());
                    }
                    list.push(Value::Object(tool));
                }
                ensure!(
                    list.len() <= MAX_TOOLS && utf16_len(&stringify(&json!(list))) <= MAX_CATALOG,
                    "工具目录超过上限"
                );
                match next.filter(|c| !c.is_empty()) {
                    Some(c) => {
                        ensure!(!seen.contains(&c) && seen.len() < MAX_PAGES, "工具分页异常");
                        seen.insert(c.clone());
                        cursor = Some(c);
                    }
                    None => break,
                }
            }
            Ok(list)
        }
        .await;
        mcp::close(client).await;
        let tools = listed?;
        let revision = text(&s, "revision").to_string();
        let changed = self.inner.db.with(|q| {
            q.run(
                "UPDATE studio_servers SET tools=? WHERE id=? AND revision=?",
                &[&stringify(&json!(tools)), &id, &revision],
            )
        })?;
        ensure!(changed > 0, "探测期间服务配置已改变");
        Ok(Value::Array(
            tools
                .iter()
                .map(|t| json!({"name": t["name"], "description": t["description"]}))
                .collect(),
        ))
    }

    /// `prepare(raw, cwd)` for parsed choices (X:239-255): the frozen scope
    /// `{skills: [{id,name,source,body,sha256}], servers: [{id,name,revision,tools}]}`.
    fn prepare_choices(&self, d: &Choices, cwd: Option<&Path>) -> Result<Value> {
        let mut skills = vec![];
        if !d.skills.is_empty() {
            let entries = self.scan(cwd)?;
            for id in &d.skills {
                let Some(s) = entries.get(id) else {
                    bail!("所选技能在当前项目不可用，请重新选择");
                };
                skills.push(json!({
                    "id": id,
                    "name": s.name,
                    "source": s.filename,
                    "body": s.body,
                    "sha256": sa_engine::protocol::digest(&s.body),
                }));
            }
        }
        let servers = self.inner.db.with(|q| {
            let mut out = vec![];
            for id in &d.servers {
                let row = q.one(
                    "SELECT * FROM studio_servers WHERE id=? AND enabled=1",
                    &[id],
                )?;
                let Some(row) = row.filter(|r| r.get("tools").is_some_and(Value::is_string)) else {
                    bail!("请先启用服务并连接读取工具");
                };
                out.push(json!({
                    "id": id,
                    "name": row["name"],
                    "revision": row["revision"],
                    "tools": parse(text(&row, "tools"))?,
                }));
            }
            Ok(out)
        })?;
        Ok(json!({"skills": skills, "servers": servers}))
    }

    /// `prepare(raw ?? defaults(), cwd)` (X:239-255).
    pub fn prepare_scope(&self, raw: Option<&Value>, cwd: Option<&Path>) -> Result<Value> {
        let d = match raw.filter(|r| !r.is_null()) {
            Some(r) => choices(r)?,
            None => choices(&self.defaults()?)?,
        };
        self.prepare_choices(&d, cwd)
    }

    /// `scope(ticket)` (X:259-264): the scope frozen at ticket creation.
    pub fn scope(&self, ticket: i64) -> Result<Value> {
        self.inner.db.with(|q| scope_in(q, ticket))
    }

    /// Snapshot `extensions` → `summary(ticket)` (X:265-269):
    /// `{skills: [{id,name}], servers: [{id,name}]}`.
    pub fn summary(&self, ticket: i64) -> Result<Value> {
        self.inner.db.with(|q| summary_in(q.conn(), ticket))
    }

    /// `context(ticket, profile)` (X:270-284): `{skills, mcp: [{server,name,tools}]}`;
    /// `mcp` lists only servers whose bound revision is still enabled and is
    /// empty for `readonly`/`legacy`.
    pub fn context_value(&self, ticket: i64, profile: &str) -> Result<Value> {
        self.inner.db.with(|q| {
            let scope = scope_in(q, ticket)?;
            let mut mcp = vec![];
            if !matches!(profile, "readonly" | "legacy") {
                for s in scope["servers"].as_array().into_iter().flatten() {
                    let live = q.exists(
                        "SELECT 1 FROM studio_servers WHERE id=? AND enabled AND revision=?",
                        &[&s["id"].as_str(), &s["revision"].as_str()],
                    )?;
                    if live {
                        mcp.push(
                            json!({"server": s["id"], "name": s["name"], "tools": s["tools"]}),
                        );
                    }
                }
            }
            Ok(json!({"skills": scope["skills"], "mcp": mcp}))
        })
    }

    /// `results(job)` (X:285-297): receipts of the latest logical turn plus
    /// the last 10, as `{id, state, result}`.
    pub fn results_of(&self, job: i64) -> Result<Vec<Value>> {
        let rows = self.inner.db.with(|q| {
            q.all(
                "SELECT key,state,result FROM studio_tool_receipts WHERE job=? AND
      (logical=(SELECT max(logical) FROM studio_tool_receipts WHERE job=?) OR rowid IN
      (SELECT rowid FROM studio_tool_receipts WHERE job=? ORDER BY rowid DESC LIMIT 10)) ORDER BY rowid",
                &[&job, &job, &job],
            )
        })?;
        rows.iter()
            .map(|r| {
                let result = match r.get("result").and_then(Value::as_str) {
                    Some(t) => parse(t)?,
                    None => json!("此前调用已提交但结果未知。核对外部状态后再决定下一步。"),
                };
                Ok(json!({"id": r["key"], "state": r["state"], "result": result}))
            })
            .collect()
    }
}

/// [`ProjectExtensions::summary`] on any connection, e.g. the read-only
/// snapshot connection of an observing desktop. A journal without the
/// extension tables yields the empty summary.
pub fn summary_in(conn: &Connection, ticket: i64) -> Result<Value> {
    let q = Q(conn);
    let scope = if q.exists(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_ticket_extensions'",
        &[],
    )? {
        scope_in(&q, ticket)?
    } else {
        parse(EMPTY_SCOPE)?
    };
    let brief = |k: &str| -> Vec<Value> {
        scope[k]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| json!({"id": s["id"], "name": s["name"]}))
            .collect()
    };
    Ok(json!({"skills": brief("skills"), "servers": brief("servers")}))
}

fn scope_in(q: &Q, ticket: i64) -> Result<Value> {
    let row = q.one(
        "SELECT scope FROM studio_ticket_extensions WHERE ticket=?",
        &[&ticket],
    )?;
    parse(row.as_ref().map_or(EMPTY_SCOPE, |r| text(r, "scope")))
}

impl Extensions for ProjectExtensions {
    fn prepare(&self, params: Option<Value>, dir: &Path) -> BoxFuture<'static, Result<Value>> {
        let prepared = self.prepare_scope(params.as_ref(), Some(dir));
        Box::pin(async move { prepared })
    }

    /// `bind(ticket, scope)` (X:256-258) on the engine's connection.
    fn bind(&self, conn: &Connection, ticket: i64, prepared: &Value) -> Result<()> {
        conn.execute(
            "INSERT INTO studio_ticket_extensions VALUES(?,?)",
            rusqlite::params![ticket, stringify(prepared)],
        )?;
        Ok(())
    }

    /// Fails closed (no skills, no MCP) if the journal cannot be read.
    fn context(&self, ticket: i64, profile: &str) -> Value {
        self.context_value(ticket, profile)
            .unwrap_or_else(|_| json!({"skills": [], "mcp": []}))
    }

    fn results(&self, job: i64) -> Vec<Value> {
        self.results_of(job).unwrap_or_default()
    }

    fn invoke(&self, args: InvokeArgs) -> BoxFuture<'static, Result<InvokeResult>> {
        let this = self.clone();
        Box::pin(async move { this.invoke_call(args).await })
    }
}

#[cfg(test)]
mod tests;
