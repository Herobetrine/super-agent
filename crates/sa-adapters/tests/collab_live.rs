//! Billable acceptance driver for the collaboration loop
//! (`docs/dev/collaboration-loop.md`). Opt-in: `SA_LIVE_COLLAB=<scenario.json>`.
//!
//! Runs one real ticket in a temporary journal and leaves
//! the journal in place; the journal is the evidence and is analysed outside
//! this test. Only the selected adapter is mounted, and collaborator routing
//! is locked to the scenario's selected model for comparable runs.
//!
//! Scenario fields: `project` (a prepared git work tree), `ledger` (a new
//! directory outside the project), `model`, `goal`, and optionally `agent`
//! (`claude` by default, or `antigravity`), `provider`, `profile`
//! (default `full`), `checks` (argv lists for the project's completion
//! checks), `answers` (operator answers, used in order for questions as they
//! appear), `follow` (operator follow-ups, sent each time the root settles),
//! `timeout_s`, and `create_only` (create the ticket and stop, so a desktop
//! window opened on the journal runs it).
//! `required_events` optionally lists event kinds that must each appear at
//! least once in this ticket's journal before acceptance can pass.
//! `required_job_keys` and `required_continuation_keys` require actual completed
//! direct children and successful continuation receipts, respectively.
//! `isolated_writers` and `integration_queue` enable the corresponding ticket
//! policies. `confirm_integration` explicitly authorizes this fixture driver to
//! publish validated candidates after recording an unchanged source snapshot.

use sa_adapters::{antigravity::Antigravity, claude::Claude, sys::Sys};
use sa_engine::{
    CreateParams, Engine, EngineOptions,
    adapter::{Adapter, Route},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

fn texts(v: &Value) -> VecDeque<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect()
}

fn verify_completion(
    state: &str,
    remaining_follows: usize,
    jobs: &[(i64, String)],
) -> anyhow::Result<()> {
    anyhow::ensure!(state == "done", "root ended {state}, expected done");
    anyhow::ensure!(
        remaining_follows == 0,
        "{remaining_follows} follow-ups were not delivered"
    );
    for (id, state) in jobs {
        anyhow::ensure!(state == "done", "job {id} ended {state}, expected done");
    }
    Ok(())
}

fn verify_required_events(
    db: &rusqlite::Connection,
    ticket: i64,
    required: &[String],
) -> anyhow::Result<()> {
    let mut statement = db.prepare("SELECT count(*) FROM events WHERE ticket=? AND kind=?")?;
    for kind in required {
        let count: i64 = statement.query_row(rusqlite::params![ticket, kind], |row| row.get(0))?;
        anyhow::ensure!(
            count >= 1,
            "required event {kind:?} is missing from ticket {ticket}"
        );
    }
    Ok(())
}

fn verify_required_controls(
    db: &rusqlite::Connection,
    ticket: i64,
    scenario: &Value,
) -> anyhow::Result<()> {
    for key in texts(&scenario["required_job_keys"]) {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM jobs j JOIN jobs root ON root.id=j.parent
             WHERE j.ticket=? AND root.ticket=j.ticket AND root.parent IS NULL
               AND j.msgkey=? AND j.state='done'",
            rusqlite::params![ticket, key],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            count == 1,
            "expected one completed direct child for key {key:?}, found {count}"
        );
    }
    for key in texts(&scenario["required_continuation_keys"]) {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM events e JOIN jobs j ON j.id=json_extract(e.payload,'$.job')
             JOIN jobs root ON root.id=e.job
             WHERE e.ticket=? AND e.kind='continued' AND json_extract(e.payload,'$.key')=?
               AND root.parent IS NULL AND j.parent=root.id AND j.ticket=e.ticket
               AND j.state='done' AND j.turn>=2",
            rusqlite::params![ticket, key],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            count == 1,
            "expected one successful continuation for key {key:?}, found {count}"
        );
    }
    Ok(())
}

fn source_snapshot(project: &Path) -> anyhow::Result<Value> {
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "fixture source Git inspection failed"
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    Ok(json!({
        "head": git(&["rev-parse", "HEAD"])?.trim(),
        "status": git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_collaboration_scenario() {
    let Ok(path) = std::env::var("SA_LIVE_COLLAB") else {
        eprintln!("set SA_LIVE_COLLAB=<scenario.json> to run a billable acceptance ticket");
        return;
    };
    let s: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let required_events: Vec<String> = s.get("required_events").map_or_else(Vec::new, |value| {
        value
            .as_array()
            .expect("scenario.required_events must be an array")
            .iter()
            .map(|kind| {
                kind.as_str()
                    .filter(|text| !text.trim().is_empty())
                    .expect("scenario.required_events must contain nonempty strings")
                    .to_string()
            })
            .collect()
    });
    let text = |k: &str| {
        s[k].as_str()
            .unwrap_or_else(|| panic!("scenario.{k} is missing"))
            .to_string()
    };
    let cwd = PathBuf::from(text("project"));
    let ledger = PathBuf::from(text("ledger"));
    let isolated_writers = s["isolated_writers"].as_bool().unwrap_or(false);
    let integration_queue = s["integration_queue"].as_bool().unwrap_or(false);
    let confirm_integration = s["confirm_integration"].as_bool().unwrap_or(false);
    assert!(
        !integration_queue || isolated_writers,
        "integration_queue requires isolated_writers"
    );
    assert!(
        !confirm_integration || integration_queue,
        "confirm_integration requires integration_queue"
    );
    assert!(
        cwd.join(".git").exists(),
        "scenario.project must be a git work tree"
    );
    assert!(
        !ledger.starts_with(&cwd),
        "scenario.ledger must be outside the project"
    );
    // Reserve a fresh directory before discovery or any model request.
    std::fs::create_dir(&ledger).expect("scenario.ledger must not exist and its parent must exist");

    let agent = s["agent"].as_str().unwrap_or("claude");
    let adapter: Arc<dyn Adapter> = match agent {
        "claude" => Arc::new(Claude::new(Sys::from_process())),
        "antigravity" => Arc::new(Antigravity::new(Sys::from_process())),
        other => panic!("unsupported scenario.agent: {other}"),
    };
    let catalog = adapter.catalog().await.unwrap().value;
    let model = text("model");
    let provider = catalog["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["available"] != json!(false))
        .filter(|p| s["provider"].as_str().is_none_or(|id| p["id"] == json!(id)))
        .filter(|p| p["models"].as_array().is_some_and(|m| m.iter().any(|x| x["id"] == json!(model))))
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .min_by_key(|id| catalog["default"]["provider"] != json!(id))
        .unwrap_or_else(|| {
            let listed: Vec<String> = catalog["providers"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["available"] != json!(false))
                .map(|p| format!("{}: {}", p["id"], p["models"].as_array().map_or(json!([]), |m| m.iter().map(|x| x["id"].clone()).collect())))
                .collect();
            panic!("no available {agent} provider matches the scenario provider/model; default {} available {listed:?}", catalog["default"])
        });
    let route = Route {
        provider,
        model,
        mode: "cli".into(),
        reasoning_effort: None,
    };
    sa_adapters::common::select(&catalog, &route).unwrap();
    eprintln!("route {} / {}", route.provider, route.model);

    let engine = Engine::open(ledger.clone(), vec![adapter], EngineOptions::default()).unwrap();
    // NEW-API-BEGIN
    if let Some(checks) = s["checks"].as_array() {
        let commands = checks.iter().map(|argv| texts(argv).into()).collect();
        engine
            .set_project_checks(cwd.to_str().unwrap(), commands)
            .unwrap();
    }
    let mut answers = texts(&s["answers"]);
    // NEW-API-END
    let ticket = engine
        .create(CreateParams {
            goal: text("goal"),
            cwd: cwd.clone(),
            lead: Some(agent.into()),
            allow_lead_routing: Some(false),
            permission_profile: Some(s["profile"].as_str().unwrap_or("full").into()),
            selections: Some(
                json!({(agent): {"provider": route.provider, "model": route.model, "mode": "cli"}}),
            ),
            ..Default::default()
        })
        .await
        .unwrap();
    if isolated_writers {
        engine.set_isolated_writers(ticket, true).unwrap();
    }
    if integration_queue {
        engine.set_integration_queue(ticket, true).unwrap();
    }
    if s["create_only"] == json!(true) {
        // Leaves the queued ticket for a desktop window opened on this journal.
        engine.close().await;
        eprintln!("ticket {ticket} created in {}", ledger.display());
        return;
    }
    let mut follows = texts(&s["follow"]);
    let source_before = source_snapshot(&cwd).unwrap();
    if integration_queue {
        assert_eq!(
            source_before["status"], "",
            "queue fixture source must start clean"
        );
    }
    let mut queue_observations = Vec::new();
    let mut observed_awaiting_confirmation = false;
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(s["timeout_s"].as_u64().unwrap_or(1500));
    let result: anyhow::Result<String> = async {
        engine.start()?;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(f) = engine.fault() {
                anyhow::bail!("engine fault: {f}");
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("the ticket did not settle in time");
            }
            // NEW-API-BEGIN
            if let Some(q) = engine.questions(ticket)?.first() {
                let body = answers.pop_front().ok_or_else(|| {
                    anyhow::anyhow!(
                        "unexpected question without a configured answer: {}",
                        q["question"]
                    )
                })?;
                eprintln!(
                    "[{:>4}s] question from job {} (root is {}): {} {} -> {body}",
                    started.elapsed().as_secs(),
                    q["job"],
                    engine.root(ticket)?["state"],
                    q["question"],
                    q["options"]
                );
                let id = q["id"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("question has no numeric id"))?;
                engine.answer(id, &body)?;
                continue;
            }
            // NEW-API-END
            let state = engine.root(ticket)?["state"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if state == "done"
                && let Some(body) = follows.pop_front()
            {
                eprintln!(
                    "[{:>4}s] root is {state}; follow-up: {body}",
                    started.elapsed().as_secs()
                );
                engine.follow(ticket, &body, false)?;
                continue;
            }
            if matches!(
                state.as_str(),
                "done" | "partial" | "failed" | "cancelled" | "interrupted"
            ) {
                if state == "done" && integration_queue {
                    let items = engine.integration_queue(ticket)?;
                    // Root completion alone is insufficient. The scheduler may
                    // reconcile a just-finished child on its next pass.
                    if items.is_empty()
                        || items
                            .iter()
                            .any(|item| matches!(item.state.as_str(), "queued" | "validating"))
                    {
                        continue;
                    }
                    anyhow::ensure!(
                        items.iter().all(|item| matches!(
                            item.state.as_str(),
                            "awaiting_confirmation" | "merged"
                        )),
                        "queue did not produce a validated candidate: {items:?}"
                    );
                    let source = source_snapshot(&cwd)?;
                    if items
                        .iter()
                        .all(|item| item.state == "awaiting_confirmation")
                    {
                        anyhow::ensure!(
                            source == source_before,
                            "fixture source changed before operator confirmation"
                        );
                        observed_awaiting_confirmation = true;
                    }
                    queue_observations.push(json!({"source": source, "items": items}));
                    std::fs::write(
                        ledger.join("acceptance-queue.json"),
                        serde_json::to_vec_pretty(&json!({
                            "source_before": source_before,
                            "confirm_integration": confirm_integration,
                            "observations": queue_observations,
                        }))?,
                    )?;
                    anyhow::ensure!(
                        observed_awaiting_confirmation,
                        "no unconfirmed validated candidate was observed"
                    );
                    if confirm_integration && items.iter().any(|item| item.state != "merged") {
                        for item in items
                            .iter()
                            .filter(|item| item.state == "awaiting_confirmation")
                        {
                            let confirmation_token =
                                item.validation["confirmation_token"].as_str().ok_or_else(|| {
                                    anyhow::anyhow!("validated candidate has no confirmation_token")
                                })?;
                            engine.confirm_integration(item.job, confirmation_token).await?;
                        }
                        continue;
                    }
                    if confirm_integration {
                        anyhow::ensure!(
                            source["head"] != source_before["head"],
                            "confirmed fixture source HEAD did not advance"
                        );
                        anyhow::ensure!(
                            source["status"] == "",
                            "confirmed fixture source is dirty"
                        );
                    }
                }
                break Ok(state);
            }
        }
    }
    .await;
    if result.is_err() {
        let _ = engine.cancel(ticket);
    }
    engine.close().await;
    let state = result.expect("acceptance driver failed; engine closed");
    eprintln!(
        "[{:>4}s] ticket {ticket} ended {state}; journal {}",
        started.elapsed().as_secs(),
        ledger.join("hub.sqlite3").display()
    );
    let db = rusqlite::Connection::open_with_flags(
        ledger.join("hub.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = db
        .prepare("SELECT id,state FROM jobs WHERE ticket=? ORDER BY id")
        .unwrap();
    let jobs = statement
        .query_map([ticket], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<(i64, String)>>>()
        .unwrap();
    verify_completion(&state, follows.len(), &jobs).unwrap();
    verify_required_events(&db, ticket, &required_events).unwrap();
    verify_required_controls(&db, ticket, &s).unwrap();
}

#[test]
fn completion_requires_done_jobs_and_all_followups() {
    let done = vec![(1, "done".to_string()), (2, "done".to_string())];
    assert!(verify_completion("done", 0, &done).is_ok());
    assert!(verify_completion("done", 1, &done).is_err());
    for state in [
        "partial",
        "failed",
        "cancelled",
        "interrupted",
        "queued",
        "running",
        "waiting",
    ] {
        assert!(verify_completion(state, 0, &done).is_err());
        assert!(verify_completion("done", 0, &[(2, state.to_string())]).is_err());
    }
}

#[test]
fn required_events_must_exist_in_the_current_ticket() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE events(ticket INTEGER,kind TEXT);
        INSERT INTO events VALUES(1,'continued'),(2,'check.bounced');",
    )
    .unwrap();
    assert!(verify_required_events(&db, 1, &[]).is_ok());
    assert!(verify_required_events(&db, 1, &["continued".into()]).is_ok());
    let error =
        verify_required_events(&db, 1, &["continued".into(), "check.bounced".into()]).unwrap_err();
    assert!(error.to_string().contains("check.bounced"));
    assert!(verify_required_events(&db, 1, &["missing".into()]).is_err());
}

#[test]
fn required_controls_reject_plain_done_and_missing_continuation() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE jobs(id INTEGER,parent INTEGER,ticket INTEGER,msgkey TEXT,state TEXT,turn INTEGER);
         CREATE TABLE events(ticket INTEGER,job INTEGER,kind TEXT,payload TEXT);
         INSERT INTO jobs VALUES(1,NULL,1,NULL,'done',3),(2,1,1,'impl','done',1);",
    ).unwrap();
    let scenario = json!({"required_job_keys":["impl","review-followup"],"required_continuation_keys":["resume-impl"]});
    assert!(verify_required_controls(&db, 1, &scenario).is_err());
    db.execute_batch("INSERT INTO jobs VALUES(3,1,1,'review-followup','done',1);")
        .unwrap();
    assert!(verify_required_controls(&db, 1, &scenario).is_err());
    db.execute_batch(
        "INSERT INTO events VALUES(1,1,'continued','{\"job\":2,\"key\":\"resume-impl\"}');",
    )
    .unwrap();
    assert!(verify_required_controls(&db, 1, &scenario).is_err());
    db.execute_batch("UPDATE jobs SET turn=2 WHERE id=2;")
        .unwrap();
    assert!(verify_required_controls(&db, 1, &scenario).is_ok());
    assert!(verify_required_controls(&db, 2, &scenario).is_err());
}
