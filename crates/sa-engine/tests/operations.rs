//! Operation receipts, fault injection and restart reconciliation with real
//! disposable Git repositories. No live model is involved.
mod common;
use common::*;
use sa_engine::EngineOptions;
use sa_engine::integration_queue::IntegrationItem;
use sa_engine::operations::{self, Failpoints, Fault, Operation, Owner, SimulatedCrash, kind};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

fn run_git(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let out = run_git(path, args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn init(path: &Path) {
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
}

fn head(path: &Path) -> String {
    git(path, &["rev-parse", "HEAD"])
}

/// Entries in the source branch reflog: one per ref update, so a second
/// fast-forward would be visible even if it targeted the same commit.
fn reflog(path: &Path) -> usize {
    git(path, &["reflog", "--format=%H"]).lines().count()
}

fn passing() -> Vec<String> {
    vec!["git".into(), "diff".into(), "--check".into()]
}

fn with_points(points: &Arc<Failpoints>) -> EngineOptions {
    EngineOptions { failpoints: Some(points.clone()), ..quiet() }
}

fn noop() -> Script {
    script(|_| async { Ok("done".into()) })
}

fn writer() -> Script {
    script(|c| async move {
        if c.depth() == 0 {
            if c.turn() == 1 {
                return Ok(send(json!({"id":"writer","write":true,"body":"write"})));
            }
        } else {
            std::fs::write(c.req.cwd.join("result.txt"), "result\n").unwrap();
        }
        Ok("done".into())
    })
}

fn item(fx: &Fx, ticket: i64) -> IntegrationItem {
    fx.engine.integration_queue(ticket).unwrap().into_iter().next().unwrap()
}

fn ops(fx: &Fx, ticket: i64) -> Vec<Operation> {
    fx.engine.operations(ticket).unwrap()
}

fn of_kind(fx: &Fx, ticket: i64, wanted: &str) -> Vec<Operation> {
    ops(fx, ticket).into_iter().filter(|op| op.kind == wanted).collect()
}

async fn wait(fx: &Fx, condition: impl Fn() -> bool) {
    common::until(&fx.engine, &fx.shared, Duration::from_secs(90), condition).await;
}

async fn queue(fx: &Fx) -> i64 {
    init(&fx.project);
    fx.engine.set_project_checks(&fx.project.to_string_lossy(), vec![passing()]).unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.start().unwrap();
    wait(fx, || fx.engine.integration_queue(ticket).unwrap().iter().any(|i| i.state == "awaiting_confirmation")).await;
    // The root's final turn runs on the source directory; confirmation would
    // correctly refuse while it is admitted.
    fx.finish(ticket, "done").await;
    ticket
}

fn token(item: &IntegrationItem) -> String {
    item.validation["confirmation_token"].as_str().unwrap().to_string()
}

/// Insert a receipt as if a previous process wrote it and then died.
fn crashed(fx: &Fx, ticket: i64, kind: &str, intent: Value) -> i64 {
    fx.engine.db().tx(|q| operations::begin(q, Owner::ticket(ticket), kind, intent)).unwrap()
}

fn state(fx: &Fx, ticket: i64, id: i64) -> Operation {
    ops(fx, ticket).into_iter().find(|op| op.id == id).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publication_crash_after_merge_is_confirmed_on_restart_without_a_second_merge() {
    let points = Failpoints::new();
    let mut fx = Fx::with(writer(), t1_catalog(), with_points(&points));
    let ticket = queue(&fx).await;
    let ready = item(&fx, ticket);
    let target = ready.validation["candidate_head"].as_str().unwrap().to_string();
    let before = reflog(&fx.project);
    points.set("integration.publish.after_effect", Fault::Crash);
    let error = fx.engine.confirm_integration(ready.job, &token(&ready)).await.unwrap_err();
    assert!(error.is::<SimulatedCrash>(), "{error:#}");
    // The effect happened; the journal still says publishing/started.
    assert_eq!(head(&fx.project), target);
    assert_eq!(reflog(&fx.project), before + 1);
    assert_eq!(item(&fx, ticket).state, "publishing");
    let publish = of_kind(&fx, ticket, kind::PUBLISH);
    assert_eq!(publish.len(), 1);
    assert_eq!(publish[0].state, "started");
    assert_eq!(publish[0].intent["target"], target.as_str());
    assert!(!publish[0].intent.to_string().contains("result\\n"), "intent carries no content");

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    let publish = state(&fx, ticket, publish[0].id);
    assert_eq!(publish.state, "confirmed");
    assert_eq!(publish.detail["reconciled"], true);
    assert_eq!(item(&fx, ticket).state, "merged");
    let w = fx.engine.workspaces(ticket).unwrap().into_iter().find(|w| w.job == ready.job).unwrap();
    assert_eq!(w.state, "merged");
    assert_eq!(w.validation["integrated_head"], target.as_str());
    assert_eq!(head(&fx.project), target);
    assert_eq!(reflog(&fx.project), before + 1, "no second merge");
    let confirmed = payloads(&fx, "SELECT payload FROM events WHERE kind='integration.confirmed'");
    assert_eq!(confirmed, vec![json!({"job": ready.job, "ok": true, "reconciled": true})]);
    let kinds: Vec<String> = fx.all(&format!("SELECT kind FROM events WHERE kind LIKE 'operation.%' AND json_extract(payload,'$.operation')={} ORDER BY seq", publish.id))
        .iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["operation.started", "operation.confirmed"]);
    // A confirmation after reconciliation does nothing.
    assert!(fx.engine.confirm_integration(ready.job, &token(&ready)).await.is_err());
    assert_eq!(reflog(&fx.project), before + 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publication_intent_saved_then_exit_is_failed_before_effect_and_can_be_confirmed() {
    let points = Failpoints::new();
    let mut fx = Fx::with(writer(), t1_catalog(), with_points(&points));
    let ticket = queue(&fx).await;
    let ready = item(&fx, ticket);
    let baseline = head(&fx.project);
    points.set("integration.publish.after_begin", Fault::Crash);
    assert!(fx.engine.confirm_integration(ready.job, &token(&ready)).await.unwrap_err().is::<SimulatedCrash>());
    assert_eq!(head(&fx.project), baseline);
    assert_eq!(item(&fx, ticket).state, "publishing");

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    let first = of_kind(&fx, ticket, kind::PUBLISH);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].state, "failed_before_effect");
    // Validation is still current, so the operator may confirm again.
    assert_eq!(item(&fx, ticket).state, "awaiting_confirmation");
    assert_eq!(head(&fx.project), baseline);
    fx.engine.confirm_integration(ready.job, &token(&ready)).await.unwrap();
    assert_eq!(item(&fx, ticket).state, "merged");
    assert_eq!(head(&fx.project), ready.validation["candidate_head"].as_str().unwrap());
    let all = of_kind(&fx, ticket, kind::PUBLISH);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].state, "confirmed");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipt_write_failures_never_publish_blindly_or_twice() {
    let points = Failpoints::new();
    let fx = Fx::with(writer(), t1_catalog(), with_points(&points));
    let ticket = queue(&fx).await;
    let ready = item(&fx, ticket);
    let baseline = head(&fx.project);
    // The intent cannot be saved: no effect is attempted.
    points.set("integration.publish.begin", Fault::Error);
    assert!(fx.engine.confirm_integration(ready.job, &token(&ready)).await.is_err());
    assert_eq!(head(&fx.project), baseline);
    assert!(of_kind(&fx, ticket, kind::PUBLISH).is_empty());
    assert_eq!(item(&fx, ticket).state, "stale");
    points.clear("integration.publish.begin");
    fx.engine.revalidate_integration(ready.job).unwrap();
    wait(&fx, || item(&fx, ticket).state == "awaiting_confirmation").await;
    let ready = item(&fx, ticket);
    let before = reflog(&fx.project);
    // The result receipt fails after the merge: the repository is inspected
    // and the publication recorded without merging again.
    points.set("integration.publish.receipt", Fault::Error);
    fx.engine.confirm_integration(ready.job, &token(&ready)).await.unwrap();
    assert!(points.hits().contains(&"integration.publish.receipt".to_string()));
    assert_eq!(item(&fx, ticket).state, "merged");
    assert_eq!(reflog(&fx.project), before + 1);
    let publish = of_kind(&fx, ticket, kind::PUBLISH);
    assert_eq!(publish.len(), 1);
    assert_eq!(publish[0].state, "confirmed");
    assert_eq!(publish[0].detail["head"], ready.validation["candidate_head"]);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_creation_crash_after_effect_is_confirmed_and_reused() {
    let points = Failpoints::new();
    let mut fx = Fx::with(writer(), t1_catalog(), with_points(&points));
    init(&fx.project);
    points.set("workspace.worktree_add.after_effect", Fault::Crash);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    wait(&fx, || points.hits().iter().any(|h| h == "workspace.worktree_add.after_effect")).await;
    let added = of_kind(&fx, ticket, kind::WORKTREE_ADD);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].state, "started");
    let path = added[0].intent["path"].as_str().unwrap().to_string();
    assert!(Path::new(&path).is_dir());
    let worktrees = git(&fx.project, &["worktree", "list", "--porcelain"]).matches("worktree ").count();

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    let added = state(&fx, ticket, added[0].id);
    assert_eq!(added.state, "confirmed", "{:?}", added.detail);
    assert_eq!(added.detail["head"], head(&fx.project).as_str());
    // Re-provisioning reuses the directory; no second worktree is created.
    for _ in 0..100 {
        fx.engine.step();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(of_kind(&fx, ticket, kind::WORKTREE_ADD).len(), 1);
    assert_eq!(git(&fx.project, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), worktrees);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_and_checkpoint_receipts_are_classified_from_repository_state() {
    let mut fx = Fx::new(noop(), t1_catalog());
    init(&fx.project);
    let ticket = fx.create(fx.params()).await;
    fx.engine.cancel(ticket).unwrap();
    let base = head(&fx.project);
    let home = fx.data.join("workspaces");
    std::fs::create_dir_all(&home).unwrap();
    let source = fx.project.to_string_lossy().to_string();
    let made = home.join("job-901");
    git(&fx.project, &["worktree", "add", "-b", "codex/made", &made.to_string_lossy(), &base]);
    git(&fx.project, &["branch", "codex/orphan"]);
    let intent = |path: &Path, branch: &str| json!({"job":0,"source":source,"path":path,"branch":branch,"base":base});
    let confirmed = crashed(&fx, ticket, kind::WORKTREE_ADD, intent(&made, "codex/made"));
    let absent = crashed(&fx, ticket, kind::WORKTREE_ADD, intent(&home.join("job-902"), "codex/absent"));
    let orphan = crashed(&fx, ticket, kind::WORKTREE_ADD, intent(&home.join("job-903"), "codex/orphan"));
    // Checkpoint commit in the isolated worktree.
    std::fs::write(made.join("work.txt"), "work\n").unwrap();
    git(&made, &["add", "."]);
    git(&made, &["commit", "-m", "Checkpoint isolated Super Agent task"]);
    let checkpoint = |pre: &str| json!({"job":0,"path":made,"branch":"codex/made","pre_head":pre});
    let committed = crashed(&fx, ticket, kind::CHECKPOINT, checkpoint(&base));
    let uncommitted = crashed(&fx, ticket, kind::CHECKPOINT, checkpoint(&head(&made)));
    let worktrees = git(&fx.project, &["worktree", "list", "--porcelain"]);

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    assert_eq!(state(&fx, ticket, confirmed).state, "confirmed");
    assert_eq!(state(&fx, ticket, absent).state, "failed_before_effect");
    let orphan = state(&fx, ticket, orphan);
    assert_eq!(orphan.state, "unknown_effect");
    assert!(orphan.detail["reason"].as_str().unwrap().contains("分支"));
    assert_eq!(state(&fx, ticket, committed).state, "confirmed");
    assert_eq!(state(&fx, ticket, uncommitted).state, "failed_before_effect");
    // Inspection is read-only: nothing created, removed or committed.
    assert_eq!(git(&fx.project, &["worktree", "list", "--porcelain"]), worktrees);
    assert!(!home.join("job-902").exists());
    assert_eq!(fx.shared.calls.lock().unwrap().len(), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_bootstrap_receipts_are_attached_and_reconciled() {
    let mut fx = Fx::new(noop(), t1_catalog());
    // A plain directory: creation initializes Git under receipts.
    let ticket = fx.create(fx.params()).await;
    fx.engine.cancel(ticket).unwrap();
    let bootstrap = ops(&fx, ticket);
    assert_eq!(
        bootstrap.iter().map(|op| (op.kind.as_str(), op.state.as_str())).collect::<Vec<_>>(),
        [(kind::PROJECT_INIT, "confirmed"), (kind::PROJECT_COMMIT, "confirmed")]
    );
    assert!(bootstrap.iter().all(|op| op.ticket == Some(ticket) && op.job.is_some()));
    let fresh = fx.dir.path().join("fresh");
    let unborn = fx.dir.path().join("unborn");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::create_dir_all(&unborn).unwrap();
    git(&unborn, &["init"]);
    let never = crashed(&fx, ticket, kind::PROJECT_INIT, json!({"directory": fresh}));
    let initialized = crashed(&fx, ticket, kind::PROJECT_INIT, json!({"directory": unborn}));
    let no_commit = crashed(&fx, ticket, kind::PROJECT_COMMIT, json!({"root": unborn}));
    let committed = crashed(&fx, ticket, kind::PROJECT_COMMIT, json!({"root": fx.project}));

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    assert_eq!(state(&fx, ticket, never).state, "failed_before_effect");
    assert_eq!(state(&fx, ticket, initialized).state, "confirmed");
    assert_eq!(state(&fx, ticket, no_commit).state, "failed_before_effect");
    assert_eq!(state(&fx, ticket, committed).state, "confirmed");
    assert!(!fresh.join(".git").exists(), "reconciliation never initializes");
    assert!(run_git(&unborn, &["rev-parse", "--verify", "HEAD"]).status.code() != Some(0));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_dependency_merge_stays_unknown_unaborted_and_never_blocks_stop() {
    let mut fx = Fx::new(noop(), t1_catalog());
    init(&fx.project);
    let ticket = fx.create(fx.params()).await;
    fx.engine.cancel(ticket).unwrap();
    let base = head(&fx.project);
    let home = fx.data.join("workspaces");
    std::fs::create_dir_all(&home).unwrap();
    let dependent = home.join("job-950");
    let prerequisite = home.join("job-951");
    git(&fx.project, &["worktree", "add", "-b", "codex/dependent", &dependent.to_string_lossy(), &base]);
    git(&fx.project, &["worktree", "add", "-b", "codex/prerequisite", &prerequisite.to_string_lossy(), &base]);
    std::fs::write(dependent.join("base.txt"), "dependent\n").unwrap();
    git(&dependent, &["commit", "-am", "dependent"]);
    std::fs::write(prerequisite.join("base.txt"), "prerequisite\n").unwrap();
    git(&prerequisite, &["commit", "-am", "prerequisite"]);
    let pre_head = head(&dependent);
    let merge_head = head(&prerequisite);
    // The merge stops with a conflict, as an interrupted merge would.
    assert!(!run_git(&dependent, &["merge", "--no-ff", "--no-edit", &merge_head]).status.success());
    let id = crashed(&fx, ticket, kind::DEPENDENCY_MERGE,
        json!({"job":0,"path":dependent,"branch":"codex/dependent","pre_head":pre_head,"merge_head":merge_head,"prerequisite":0}));

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    let merge = state(&fx, ticket, id);
    assert_eq!(merge.state, "unknown_effect");
    assert!(merge.detail["reason"].as_str().unwrap().contains("MERGE_HEAD"));
    // Never aborted automatically.
    assert_eq!(git(&dependent, &["rev-parse", "MERGE_HEAD"]), merge_head);
    assert_eq!(head(&dependent), pre_head);
    // Stop is accepted and final despite the unknown effect.
    fx.engine.cancel(ticket).unwrap();
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    let calls = fx.shared.calls.lock().unwrap().len();
    assert_eq!(calls, 0);
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    for _ in 0..20 {
        fx.engine.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(root_state(&fx.engine, ticket), "cancelled", "a stopped ticket stays stopped");
    assert_eq!(fx.shared.calls.lock().unwrap().len(), calls, "no model execution after restart");
    assert_eq!(state(&fx, ticket, id).state, "unknown_effect");
    assert_eq!(git(&dependent, &["rev-parse", "MERGE_HEAD"]), merge_head);
    // The operator records the check.
    assert!(fx.engine.resolve_operation(id, "").is_err());
    fx.engine.resolve_operation(id, "已人工核对冲突并保留现场").unwrap();
    let resolved = state(&fx, ticket, id);
    assert_eq!(resolved.state, "resolved");
    assert_eq!(resolved.detail["previous"]["reconciled"], true);
    assert!(fx.engine.resolve_operation(id, "again").is_err());
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='operation.resolved'"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_validation_cancel_is_requested_then_confirmed_terminated() {
    let mut fx = Fx::new(writer(), t1_catalog());
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    fx.restart(quiet()).await;
    let w = fx.engine.workspaces(ticket).unwrap().remove(0);
    let slow: Vec<String> = if cfg!(windows) {
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 60"].map(String::from).to_vec()
    } else {
        ["/bin/sh", "-c", "sleep 60"].map(String::from).to_vec()
    };
    let engine = fx.engine.clone();
    let job = w.job;
    let started = Instant::now();
    let task = tokio::spawn(async move { engine.validate_workspace(job, slow).await });
    let deadline = Instant::now() + Duration::from_secs(30);
    let validation = loop {
        if let Some(op) = of_kind(&fx, ticket, kind::LEGACY_VALIDATE).into_iter().next() {
            break op;
        }
        assert!(Instant::now() < deadline, "validation never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(validation.state, "started");
    tokio::time::sleep(Duration::from_millis(300)).await;
    fx.engine.cancel(ticket).unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("取消"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(30), "the process tree was not stopped");
    let validation = state(&fx, ticket, validation.id);
    assert_eq!(validation.state, "cancelled", "{:?}", validation.detail);
    let kinds: Vec<String> = fx.all(&format!("SELECT kind FROM events WHERE kind LIKE 'operation.%' AND json_extract(payload,'$.operation')={} ORDER BY seq", validation.id))
        .iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["operation.started", "operation.cancel_requested", "operation.cancelled"]);
    // The writer's uncommitted file was checkpointed under its own receipt.
    let checkpoint = of_kind(&fx, ticket, kind::CHECKPOINT);
    assert_eq!(checkpoint.len(), 1);
    assert_eq!(checkpoint[0].state, "confirmed");
    assert_eq!(fx.engine.workspaces(ticket).unwrap()[0].state, "failed");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_rows_and_pending_tool_receipts_are_reconciled_on_restart() {
    let mut fx = Fx::new(noop(), t1_catalog());
    init(&fx.project);
    let ticket = fx.create(fx.params()).await;
    fx.engine.cancel(ticket).unwrap();
    let root = fx.int("SELECT id FROM jobs WHERE parent IS NULL");
    let source = fx.project.to_string_lossy().to_string();
    let contained = head(&fx.project);
    fx.engine.db().tx(|q| {
        let child = q.insert("INSERT INTO jobs(ticket,parent,agent,body,writing,depth,state) VALUES(?,?,'codex','x',1,1,'done')", &[&ticket, &root])?;
        let other = q.insert("INSERT INTO jobs(ticket,parent,agent,body,writing,depth,state) VALUES(?,?,'codex','y',1,1,'done')", &[&ticket, &root])?;
        for (job, state, head) in [(root, "validating", ""), (child, "integrating", contained.as_str()), (other, "integrating", "0000000000000000000000000000000000000000")] {
            q.run("INSERT INTO job_workspaces(job,ticket,source,path,branch,head,state) VALUES(?,?,?,?,?,?,?)",
                &[&job, &ticket, &source, &format!("missing-{job}"), &format!("codex/legacy-{job}"), &head, &state])?;
        }
        q.conn().execute_batch("CREATE TABLE IF NOT EXISTS studio_tool_receipts(job INTEGER NOT NULL REFERENCES jobs,logical INTEGER NOT NULL,key TEXT NOT NULL,
            request TEXT NOT NULL,result TEXT,state TEXT NOT NULL,PRIMARY KEY(job,logical,key));")?;
        q.run("INSERT INTO studio_tool_receipts VALUES(?,1,'call-1',?,NULL,'pending')",
            &[&root, &json!({"server":"files","tool":"write","arguments":{"path":"x"}}).to_string()])?;
        q.run("INSERT INTO studio_tool_receipts VALUES(?,1,'call-0',?,'{}','done')", &[&root, &"{}"])?;
        Ok(())
    }).unwrap();

    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    let rows = fx.all("SELECT job,state,detail FROM job_workspaces ORDER BY job");
    assert!(rows.iter().all(|r| r["state"] == "failed"), "{rows:?}");
    assert!(rows[0]["detail"].as_str().unwrap().contains("验证中断"));
    assert!(rows[1]["detail"].as_str().unwrap().contains("缺少合入回执"));
    assert!(rows[2]["detail"].as_str().unwrap().contains("未包含"));
    let integrate = of_kind(&fx, ticket, kind::LEGACY_INTEGRATE);
    assert_eq!(integrate.len(), 1, "only the contained head needs an operator check");
    assert_eq!(integrate[0].state, "unknown_effect");
    let receipts = fx.all("SELECT key,state,result FROM studio_tool_receipts ORDER BY key");
    assert_eq!(receipts[0]["state"], "done");
    assert_eq!(receipts[1]["state"], "uncertain");
    let mcp = of_kind(&fx, ticket, kind::MCP);
    assert_eq!(mcp.len(), 1);
    assert_eq!(mcp[0].state, "unknown_effect");
    assert_eq!(mcp[0].intent["tool"], "write");
    assert!(mcp[0].intent.get("arguments").is_none(), "arguments are not copied into the intent");
    let results = payloads(&fx, "SELECT payload FROM events WHERE kind='mcp.result'");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["state"], "uncertain");
    // A second start does not reclassify or duplicate anything.
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    assert_eq!(of_kind(&fx, ticket, kind::MCP).len(), 1);
    assert_eq!(of_kind(&fx, ticket, kind::LEGACY_INTEGRATE).len(), 1);
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    fx.engine.close().await;
}
