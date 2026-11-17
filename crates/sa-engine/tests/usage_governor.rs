//! Admission slots, provider windows and quota recovery through real engine entry points.

mod common;

use common::*;
use sa_engine::adapter::AdapterEvent;
use sa_engine::resilience::classify;
use sa_engine::{CreateParams, EngineOptions};
use sa_store::efficiency::{Settings, shared};
use sa_store::quota::Window;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn options(cap: u32, guard: bool) -> EngineOptions {
    EngineOptions {
        efficiency: shared(Settings { max_collaborators: cap, quota_guard: guard, ..Default::default() }),
        ..quiet()
    }
}

fn children(count: usize) -> String {
    (0..count).map(|n| send(json!({"id":format!("child-{n}"),"body":format!("child-{n}")}))).collect::<Vec<_>>().join("\n")
}

fn observe(fx: &Fx, agent: &str, provider: &str, used: f64, exhausted: bool, resets_at: f64) {
    fx.engine.db().tx(|q| {
        sa_store::quota::upsert(q.0, agent, provider, &Window {
            kind: "five_hour".into(), used: Some(used), resets_at: Some(resets_at), exhausted, minutes: Some(300),
        }, sa_engine::db::now())?;
        Ok(())
    }).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collaborator_cap_queues_five_children_and_keeps_the_lead_available() {
    let release = Gate::new();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let fx = Fx::with(script({
        let (release, active, peak) = (release.clone(), active.clone(), peak.clone());
        move |c| {
            let (release, active, peak) = (release.clone(), active.clone(), peak.clone());
            async move {
                if c.depth() == 0 {
                    if c.turn() == 1 { return Ok(children(5)); }
                    if c.turn() == 2 {
                        assert_eq!(active.load(Ordering::SeqCst), 2);
                        assert!(c.packet["notes"].as_array().unwrap().iter()
                            .any(|note| note["body"] == "汇报当前进展"));
                    }
                    return Ok("reviewed".into());
                }
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                release.wait().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok("checked".into())
            }
        }
    }), t1_catalog(), options(2, true));
    let ticket = fx.begin(fx.params()).await;
    // Dispatch commands commit one at a time. Wait for the lead's reply so
    // the queued count includes its complete batch, even when children start early.
    fx.until(|| root_state(&fx.engine, ticket) == "waiting" && active.load(Ordering::SeqCst) == 2).await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"), 5);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='queued'"), 3);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    // A waiting non-auto lead resumes on mailbox delivery. follow() records
    // user updates but only directly requeues terminal or auto roots.
    fx.engine.note(ticket, root, "汇报当前进展").unwrap();
    fx.until(|| fx.shared.count(|c| c.packet["depth"] == 0 && c.packet["turn"] == 2) == 1).await;
    assert_eq!(active.load(Ordering::SeqCst), 2);
    release.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='done'"), 5);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_cap_admits_all_collaborators() {
    let release = Gate::new();
    let fx = Fx::with(script({
        let release = release.clone();
        move |c| {
            let release = release.clone();
            async move {
                if c.depth() == 0 { return Ok(if c.turn() == 1 { children(5) } else { "reviewed".into() }); }
                release.wait().await;
                Ok("checked".into())
            }
        }
    }), t1_catalog(), options(0, true));
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='running'") == 5).await;
    release.open();
    fx.finish(ticket, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_ticket_does_not_fill_the_reader_slice_ahead_of_another_ticket() {
    let release = Gate::new();
    let fx = Fx::with(script({
        let release = release.clone();
        move |c| {
            let release = release.clone();
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 {
                        children(if c.packet["goal"] == "second" { 1 } else { 20 })
                    } else { "reviewed".into() });
                }
                release.wait().await;
                Ok("checked".into())
            }
        }
    }), t1_catalog(), options(2, true));
    let first = fx.begin(fx.params()).await;
    // Two children can start before all twenty SA_SEND lines commit. The
    // waiting lead establishes the complete backlog needed to test fairness.
    fx.until(|| root_state(&fx.engine, first) == "waiting"
        && fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={first} AND parent IS NOT NULL AND state='running'")) == 2).await;
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={first} AND parent IS NOT NULL")), 20);
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={first} AND parent IS NOT NULL AND state='queued'")), 18);
    let second = fx.create(CreateParams::new("second", &fx.project).lead("codex")).await;
    fx.until(|| fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={second} AND parent IS NOT NULL AND state='running'")) == 1).await;
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={first} AND parent IS NOT NULL AND state='running'")), 2);
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={first} AND parent IS NOT NULL AND state='queued'")), 18);
    release.open();
    fx.finish(first, "done").await;
    fx.finish(second, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_child_waits_and_resumes_the_same_native_session_after_reset() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let fx = Fx::with(script({
        let attempts = attempts.clone();
        move |c| {
            let attempts = attempts.clone();
            async move {
                if c.depth() == 0 { return Ok(if c.turn() == 1 { children(1) } else { "reviewed".into() }); }
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = c.events.send(AdapterEvent::Session("quota-session".into()));
                    let mut error = classify::quota("five_hour", Some(Duration::from_millis(40)));
                    error.session_id = Some("quota-session".into());
                    return Err(error);
                }
                assert_eq!(c.req.session_id.as_deref(), Some("quota-session"));
                Ok("continued after reset".into())
            }
        }
    }), t1_catalog(), options(4, true));
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'") == 1).await;
    let child = fx.all("SELECT id,state,ready_after FROM jobs WHERE parent IS NOT NULL").remove(0);
    assert_eq!(child["state"], "queued");
    assert!(child["ready_after"].as_f64().unwrap() > sa_engine::db::now() + 59.0);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE state='failed'"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 0);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    assert!(fx.engine.notes(root).unwrap().iter().any(|note| note["sender"] == "platform"));
    tokio::time::sleep(Duration::from_millis(80)).await;
    // The reset has passed. Advance only the fixed grace period to keep this
    // integration test fast, after checking its real persisted deadline.
    fx.engine.db().tx(|q| {
        q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&sa_engine::db::now(), &child["id"].as_i64().unwrap()])?;
        Ok(())
    }).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(fx.int("SELECT count(*) FROM quota_windows WHERE exhausted=1"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_window_batch_is_stored_and_supplies_an_unreported_failure_reset() {
    let fail = Gate::new();
    let reset = sa_engine::db::now() + 180.0;
    let fx = Fx::with(script({
        let fail = fail.clone();
        move |c| {
            let fail = fail.clone();
            async move {
                c.emit("native", json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
                    "primary":{"usedPercent":97,"windowDurationMins":300,"resetsAt":reset},
                    "secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":reset + 604800.0}
                }}}));
                fail.wait().await;
                Err(classify::quota("codex", None))
            }
        }
    }), t1_catalog(), options(4, true));
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM quota_windows") == 2).await;
    let observed = payloads(&fx, "SELECT payload FROM events WHERE kind='quota.window'");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0]["agent"], "codex");
    assert_eq!(observed[0]["provider"], "test-local");
    assert_eq!(observed[0]["windows"].as_array().unwrap().len(), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='native' AND json_extract(payload,'$.method')='account/rateLimits/updated'"), 1);
    fail.open();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'") == 1).await;
    let stored = fx.all("SELECT * FROM quota_windows WHERE kind='primary'").remove(0);
    assert_eq!(stored["agent"], "codex");
    assert_eq!(stored["provider"], "test-local");
    assert_eq!(stored["used"], json!(0.97));
    assert_eq!(stored["resets_at"], json!(reset));
    assert_eq!(stored["exhausted"], 1);
    assert_eq!(fx.int("SELECT count(*) FROM quota_windows"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM quota_windows WHERE kind='codex'"), 0);
    let waits = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.quota_wait'");
    assert_eq!(waits[0]["code"], "quota:codex");
    assert_eq!(waits[0]["window"], "primary");
    assert!((waits[0]["until"].as_f64().unwrap() - reset - 60.0).abs() < 0.01);
    assert_eq!(root_state(&fx.engine, ticket), "queued");
    let batches = payloads(&fx, "SELECT payload FROM events WHERE kind='quota.window'");
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[1]["windows"].as_array().unwrap().len(), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twelve_quota_waits_end_the_current_recovery_cycle() {
    let fx = Fx::with(script(|_| async { Err(classify::quota("five_hour", Some(Duration::from_secs(30)))) }), t1_catalog(), options(4, true));
    let ticket = fx.create(fx.params()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.db().tx(|q| {
        for _ in 0..12 {
            q.log(sa_engine::db::Who { ticket, id: root, turn: 1 }, "turn.quota_wait", &json!({
                "until":sa_engine::db::now() - 1.0,"window":"five_hour","reason":"quota_window",
                "class":"billing","code":"quota:five_hour",
                "route":{"provider":"test-local","model":"default","mode":"cli"}
            }))?;
        }
        Ok(())
    }).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "failed").await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'"), 12);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.gave_up'"), 1);
    assert_eq!(fx.shared.count(|_| true), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_window_and_disabled_guard_give_up() {
    for (guard, reset) in [(true, Duration::from_secs(7 * 3600)), (false, Duration::from_millis(20))] {
        let fx = Fx::with(script(move |_| async move { Err(classify::quota("five_hour", Some(reset))) }), t1_catalog(), options(4, guard));
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "failed").await;
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'"), 0);
        let gave_up = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.gave_up'");
        assert_eq!(gave_up[0]["class"], "billing");
        assert_eq!(gave_up[0]["code"], "quota:five_hour");
        if guard {
            let result = fx.engine.root(ticket).unwrap()["result"].as_str().unwrap().to_string();
            assert!(result.contains("UTC"));
            assert!(result.contains("超过 6 小时"));
            assert!(gave_up[0]["quota_until"].is_number());
        }
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_exhausted_route_is_held_before_adapter_execution() {
    let fx = Fx::with(script(|_| async { panic!("exhausted route must wait") }), t1_catalog(), options(4, true));
    let ticket = fx.create(fx.params()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.db().tx(|q| {
        q.run("INSERT INTO native_routes VALUES(?,?)", &[&root, &json!({"provider":"test-local","model":"default","mode":"cli"}).to_string()])?;
        Ok(())
    }).unwrap();
    let reset = sa_engine::db::now() + 300.0;
    observe(&fx, "codex", "test-local", 1.0, true, reset);
    fx.engine.start().unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.deferred'") == 1).await;
    let deferred = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.deferred'");
    assert_eq!(deferred[0]["reason"], "quota_window");
    assert_eq!(deferred[0]["window"], "five_hour");
    assert_eq!(fx.shared.count(|_| true), 0);
    assert_eq!(root_state(&fx.engine, ticket), "queued");
    assert!(fx.engine.root(ticket).unwrap()["ready_after"].as_f64().unwrap() >= reset + 60.0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_exhausted_route_waits_without_recording_a_route_failure() {
    let fx = Fx::with(script(|_| async { panic!("fresh exhausted route must wait") }), t1_catalog(), options(4, true));
    observe(&fx, "codex", "test-local", 1.0, true, sa_engine::db::now() + 300.0);
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'") == 1).await;
    let waits = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.quota_wait'");
    assert_eq!(waits[0]["reason"], "quota_window");
    assert_eq!(waits[0]["window"], "five_hour");
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind IN ('turn.route_wait','turn.retry_scheduled','turn.gave_up','route.rotated')"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='harness.health' AND json_extract(payload,'$.state') IN ('open','degraded')"), 0);
    assert_eq!(fx.shared.count(|_| true), 0);
    assert_eq!(root_state(&fx.engine, ticket), "queued");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_window_without_a_reset_allows_its_thirty_minute_probe() {
    let fx = Fx::with(script(|_| async { Ok("provider is serving again".into()) }), t1_catalog(), options(4, true));
    let ticket = fx.create(fx.params()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.db().tx(|q| {
        q.run("INSERT INTO native_routes VALUES(?,?)", &[&root, &json!({"provider":"test-local","model":"default","mode":"cli"}).to_string()])?;
        sa_store::quota::upsert(q.0, "codex", "test-local", &Window {
            kind: "unknown".into(), used: None, resets_at: None, exhausted: true, minutes: None,
        }, sa_engine::db::now() - 1801.0)?;
        Ok(())
    }).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|_| true), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind IN ('turn.deferred','turn.quota_wait')"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM quota_windows WHERE exhausted=1"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserve_holds_only_lead_route_collaborators_and_observes_live_guard_changes() {
    let efficiency = shared(Settings::default());
    let fx = Fx::with(script(|c| async move {
        if c.depth() == 0 {
            return Ok(if c.turn() == 1 {
                [send(json!({"id":"same","to":"codex"})), send(json!({"id":"other","to":"deepseek"}))].join("\n")
            } else { "reviewed".into() });
        }
        Ok("checked".into())
    }), t1_catalog(), EngineOptions { efficiency: efficiency.clone(), ..quiet() });
    let reset = sa_engine::db::now() + 300.0;
    observe(&fx, "codex", "test-local", 0.92, false, reset);
    observe(&fx, "deepseek", "other", 0.92, false, reset);
    let mut params = fx.params();
    params.selections = Some(json!({"deepseek":{"provider":"other","model":"foreign","mode":"cli"}}));
    let ticket = fx.begin(params).await;
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'") == 1
        && fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND agent='deepseek' AND state='done'") == 1).await;
    assert!(fx.shared.count(|c| c.packet["depth"] == 0) >= 1);
    assert_eq!(fx.shared.count(|c| c.packet["depth"].as_i64().unwrap() > 0 && c.name == "codex"), 0);
    let waits = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.quota_wait'");
    assert_eq!(waits[0]["reason"], "quota_reserve");
    fx.engine.db().tx(|q| { q.run("UPDATE jobs SET ready_after=0 WHERE parent IS NOT NULL AND agent='codex'", &[])?; Ok(()) }).unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.deferred' AND json_extract(payload,'$.reason')='quota_reserve'") == 1).await;
    // A live policy change is read on the next pass once the deferred deadline
    // becomes eligible, without reopening the engine or changing its route.
    efficiency.write().unwrap().quota_guard = false;
    fx.engine.db().tx(|q| { q.run("UPDATE jobs SET ready_after=0 WHERE parent IS NOT NULL AND agent='codex'", &[])?; Ok(()) }).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"].as_i64().unwrap() > 0 && c.name == "codex"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_budget_rounds_windows_and_omits_empty_policy() {
    let fx = Fx::with(script(|_| async { Ok("seen".into()) }), t1_catalog(), options(4, true));
    observe(&fx, "codex", "test-local", 0.83, false, sa_engine::db::now() + 96.0 * 60.0);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    fx.engine.follow(ticket, "继续核对", false).unwrap();
    fx.finish(ticket, "done").await;
    let budgets = fx.shared.calls(|c| Some(c.packet["usage_budget"].clone()));
    assert_eq!(budgets.len(), 2);
    assert_eq!(budgets[0]["max_parallel_collaborators"], 4);
    assert_eq!(budgets[0]["windows"], json!([{"agent":"codex","provider":"test-local","window_minutes":300,"used_percent":85,"resets_in_minutes":95,"state":"ok"}]));
    assert_eq!(budgets[1], budgets[0]);
    assert_eq!(fx.shared.calls(|c| (c.packet["turn"] == 2).then(|| c.packet["context_delivery"]["mode"].clone())), vec![json!("delta")]);
    fx.engine.close().await;
    for guard in [true, false] {
        let fx = Fx::with(script(|_| async { Ok("seen".into()) }), t1_catalog(), options(0, guard));
        if !guard { observe(&fx, "codex", "test-local", 0.83, false, sa_engine::db::now() + 300.0); }
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "done").await;
        assert_eq!(fx.shared.calls(|c| Some(c.packet.get("usage_budget").cloned())), vec![None]);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distant_exhaustion_runs_and_fails_while_distant_reserve_allows_a_collaborator() {
    for (exhausted, seconds) in [(true, 7.0 * 3600.0), (false, 3.0 * 86400.0)] {
        let fx = Fx::with(script(move |c| async move {
            if exhausted { return Err(classify::quota("five_hour", None)); }
            if c.depth() == 0 && c.turn() == 1 {
                return Ok(send(json!({"id":"same-provider","to":"codex"})));
            }
            Ok("checked".into())
        }), t1_catalog(), options(4, true));
        let ticket = fx.create(fx.params()).await;
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        fx.engine.db().tx(|q| {
            q.run("INSERT INTO native_routes VALUES(?,?)", &[&root,
                &json!({"provider":"test-local","model":"default","mode":"cli"}).to_string()])?;
            Ok(())
        }).unwrap();
        let reset = sa_engine::db::now() + seconds;
        observe(&fx, "codex", "test-local", if exhausted { 1.0 } else { 0.92 }, exhausted, reset);
        fx.engine.start().unwrap();
        fx.finish(ticket, if exhausted { "failed" } else { "done" }).await;
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind IN ('turn.deferred','turn.quota_wait')"), 0);
        if exhausted {
            assert_eq!(fx.shared.count(|_| true), 1);
            let gave_up = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.gave_up'");
            assert_eq!(gave_up.len(), 1);
            assert!((gave_up[0]["quota_until"].as_f64().unwrap() - reset - 60.0).abs() < 0.01);
            let result = fx.engine.root(ticket).unwrap()["result"].as_str().unwrap().to_owned();
            assert!(result.contains(&classify::utc_minute(reset as i64)));
            assert!(result.contains("超过 6 小时"));
        } else {
            assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='done'"), 1);
            let child = fx.shared.calls(|c| (c.packet["depth"] == 1).then(|| (c.name.clone(), c.req.route.provider.clone())));
            assert_eq!(child, vec![("codex".into(), "test-local".into())]);
        }
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn near_window_holds_a_bound_collaborator_even_with_a_distant_weekly_window() {
    for exhausted in [false, true] {
        let fx = Fx::with(script(|_| async { panic!("near window must hold the bound collaborator") }),
            t1_catalog(), options(4, true));
        let ticket = fx.create(fx.params()).await;
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        fx.engine.db().tx(|q| {
            q.run("UPDATE jobs SET state='waiting' WHERE id=?", &[&root])?;
            let child = q.insert("INSERT INTO jobs(ticket,parent,agent,body,writing,depth,turn,state)
                VALUES(?,?,'codex','check',0,1,1,'queued')", &[&ticket, &root])?;
            let route = json!({"provider":"test-local","model":"default","mode":"cli"}).to_string();
            q.run("INSERT INTO native_routes VALUES(?,?)", &[&root, &route])?;
            q.run("INSERT INTO native_routes VALUES(?,?)", &[&child, &route])?;
            let at = sa_engine::db::now();
            for (kind, seconds, minutes) in [("five_hour", 2.0 * 3600.0, 300), ("seven_day", 3.0 * 86400.0, 10080)] {
                sa_store::quota::upsert(q.0, "codex", "test-local", &Window {
                    kind: kind.into(), used: Some(if exhausted { 1.0 } else { 0.92 }),
                    resets_at: Some(at + seconds), exhausted, minutes: Some(minutes),
                }, at)?;
            }
            Ok(())
        }).unwrap();
        fx.engine.start().unwrap();
        fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.deferred'") == 1).await;
        let deferred = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.deferred'");
        assert_eq!(deferred[0]["window"], "five_hour");
        assert_eq!(deferred[0]["reason"], if exhausted { "quota_window" } else { "quota_reserve" });
        assert_eq!(fx.shared.count(|_| true), 0);
        assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='queued'"), 1);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_budget_reserves_only_windows_resetting_within_six_hours() {
    let fx = Fx::with(script(|_| async { Ok("checked".into()) }), t1_catalog(), options(4, true));
    let at = sa_engine::db::now();
    observe(&fx, "codex", "test-local", 0.92, false, at + 2.0 * 3600.0);
    fx.engine.db().tx(|q| {
        sa_store::quota::upsert(q.0, "codex", "test-local", &Window {
            kind: "seven_day".into(), used: Some(0.92), resets_at: Some(at + 3.0 * 86400.0),
            exhausted: false, minutes: Some(10080),
        }, at)?;
        Ok(())
    }).unwrap();
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    let budgets = fx.shared.calls(|c| Some(c.packet["usage_budget"].clone()));
    assert_eq!(budgets.len(), 1);
    assert_eq!(budgets[0]["windows"], json!([
        {"agent":"codex","provider":"test-local","window_minutes":300,"used_percent":90,"resets_in_minutes":120,"state":"reserve"},
        {"agent":"codex","provider":"test-local","window_minutes":10080,"used_percent":90,"resets_in_minutes":4320,"state":"ok"}
    ]));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consecutive_quota_waits_notify_the_parent_only_once_per_cycle() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let fx = Fx::with(script({
        let attempts = attempts.clone();
        move |c| {
            let attempts = attempts.clone();
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { children(1) } else { "reviewed".into() });
                }
                if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    let mut error = classify::quota("five_hour", Some(Duration::from_millis(40)));
                    error.session_id = Some("waiting-session".into());
                    return Err(error);
                }
                assert_eq!(c.req.session_id.as_deref(), Some("waiting-session"));
                Ok("checked".into())
            }
        }
    }), t1_catalog(), options(4, true));
    let ticket = fx.begin(fx.params()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    for count in 1..=2 {
        fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'") == count).await;
        let notes = payloads(&fx, &format!("SELECT e.payload FROM mailbox m JOIN events e ON e.seq=m.seq
            WHERE m.target={root} AND e.kind='note' AND json_extract(e.payload,'$.sender')='platform'"));
        assert_eq!(notes.len(), 1);
        let body: serde_json::Value = serde_json::from_str(notes[0]["body"].as_str().unwrap()).unwrap();
        let child = fx.all("SELECT id,state,ready_after FROM jobs WHERE parent IS NOT NULL").remove(0);
        assert_eq!(body["child"], child["id"]);
        assert_eq!(body["class"], "quota_window");
        assert_eq!(body["attempt"], 1);
        assert_eq!(child["state"], "queued");
        assert!(child["ready_after"].as_f64().unwrap() > sa_engine::db::now() + 59.0);
        if count == 1 {
            fx.until(|| root_state(&fx.engine, ticket) == "waiting"
                && fx.shared.count(|c| c.packet["depth"] == 0) == 2).await;
        }
        assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 2);
        // Each reset really passes. Skip only the already-asserted grace period.
        tokio::time::sleep(Duration::from_millis(80)).await;
        fx.engine.db().tx(|q| {
            q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&sa_engine::db::now(), &child["id"].as_i64().unwrap()])?;
            Ok(())
        }).unwrap();
    }
    fx.finish(ticket, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.quota_wait'"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox m JOIN events e ON e.seq=m.seq WHERE e.kind='note' AND json_extract(e.payload,'$.sender')='platform'"), 1);
    fx.engine.close().await;
}
