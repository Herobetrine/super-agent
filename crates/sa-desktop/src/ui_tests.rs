//! Drives the real [`App`] through its accessibility tree (egui_kittest):
//! typing, clicks and Enter go through the same widgets as a user, against
//! a temporary journal scheduled by [`Host`] with fake harnesses.

use crate::app::{App, Tab};
use crate::cli::Options;
use crate::host::{Host, tests::fakes};
use crate::worker;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn harness(data: &std::path::Path, profile: &str) -> Harness<'static, App> {
    let dir = data.to_path_buf();
    let profile = profile.to_string();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 860.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = Host::start_with(dir.clone(), fakes, move || ctx.request_repaint())
                .map_err(|e| format!("{e:#}"));
            let opener = crate::data::StoreSource::opener(dir.clone());
            let worker = worker::spawn(opener, cc.egui_ctx.clone());
            let opts = Options {
                data: Some(dir),
                ..Default::default()
            };
            let mut app = App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host);
            app.composer.profile = profile;
            app
        })
}

/// Step frames until `cond` holds (background threads advance in real time).
fn until(h: &mut Harness<'_, App>, what: &str, cond: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        h.step();
        if cond(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let app = h.state();
    let dump = app.host.as_ref().map(|host| {
        host.engine()
            .db()
            .with(|q| q.all("SELECT ticket,job,kind,substr(payload,1,160) p FROM events ORDER BY seq", &[]))
            .unwrap()
    });
    panic!(
        "timed out waiting for {what}; message={:?} pending={} events={dump:#?}",
        app.composer.message, app.composer.pending
    );
}

fn root_state(app: &App) -> String {
    app.view
        .as_ref()
        .and_then(|v| v.snap.as_ref())
        .map(|s| s.ticket.state.clone())
        .unwrap_or_default()
}

fn kinds(app: &App, ticket: i64) -> Vec<String> {
    app.host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.all("SELECT kind FROM events WHERE ticket=? ORDER BY seq", &[&ticket]))
        .unwrap()
        .into_iter()
        .map(|r| r["kind"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn composer_sends_follows_up_approves_and_edits_the_plan() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "ask");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.run_steps(4);
    assert!(h.state().writable(), "host error: {:?}", h.state().host_error);

    // Type the goal into the composer and press Enter.
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("核对项目文件");
    h.run_steps(2);
    assert_eq!(h.state().composer.draft, "核对项目文件");
    h.key_press(egui::Key::Enter);
    until(&mut h, "ticket selected", |a| a.view.is_some());
    let ticket = h.state().view.as_ref().unwrap().id;
    assert!(h.state().composer.draft.is_empty(), "draft kept after send");

    // The `ask` turn waits for the operator: approve it from the panel.
    until(&mut h, "pending approval", |a| {
        a.view.as_ref().and_then(|v| v.snap.as_ref()).is_some_and(|s| !s.approvals.is_empty())
    });
    h.state_mut().inspector_open = true;
    h.state_mut().tab = Tab::Approvals;
    h.run_steps(2);
    h.get_by_label("允许本次请求").click();
    until(&mut h, "reply after approval", |a| root_state(a) == "done");
    let reply = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.one("SELECT result FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket]))
        .unwrap()
        .unwrap();
    assert!(reply["result"].as_str().unwrap().ends_with("approved"), "{reply:?}");

    // Plan: add an item with Enter, then tick it.
    h.state_mut().tab = Tab::Plan;
    h.run_steps(2);
    let field = h.get_by_label("计划项输入");
    field.focus();
    field.type_text("核对附件");
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    until(&mut h, "plan item", |a| {
        a.view.as_ref().and_then(|v| v.snap.as_ref()).is_some_and(|s| s.plan.len() == 1)
    });
    h.get_by_label("标记为已完成 核对附件").click();
    until(&mut h, "plan done", |a| {
        a.view.as_ref().and_then(|v| v.snap.as_ref()).is_some_and(|s| s.plan.first().is_some_and(|p| p.done))
    });

    // Follow-up through the send button.
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("继续");
    h.run_steps(2);
    h.get_by_role_and_label(Role::Button, "发送").click();
    // Every `ask` turn requests approval again; deny this one.
    until(&mut h, "second approval", |a| {
        a.view.as_ref().and_then(|v| v.snap.as_ref()).is_some_and(|s| !s.approvals.is_empty())
    });
    h.state_mut().tab = Tab::Approvals;
    h.run_steps(2);
    h.get_by_label("拒绝").click();
    until(&mut h, "second reply", |a| {
        root_state(a) == "done" && kinds(a, ticket).iter().filter(|k| *k == "reply").count() == 2
    });

    let k = kinds(h.state(), ticket);
    for want in ["user", "approval.request", "approval.decision", "plan.add", "plan.done", "user.follow"] {
        assert!(k.iter().any(|x| x == want), "missing {want}: {k:?}");
    }
    let decisions: Vec<Value> = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.all("SELECT decision FROM approvals WHERE ticket=? ORDER BY id", &[&ticket]))
        .unwrap()
        .into_iter()
        .map(|mut r| r.remove("decision").unwrap())
        .collect();
    assert_eq!(decisions, vec![Value::from("allow"), Value::from("deny")]);
    let last = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.one("SELECT result FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket]))
        .unwrap()
        .unwrap();
    assert!(last["result"].as_str().unwrap().ends_with("denied"), "{last:?}");
}

fn ticket_routes(app: &App, ticket: i64) -> Value {
    let row = app
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.one("SELECT agent_routes FROM tickets WHERE id=?", &[&ticket]))
        .unwrap()
        .unwrap();
    serde_json::from_str(row["agent_routes"].as_str().unwrap()).unwrap()
}

#[test]
fn model_dialog_saves_defaults_and_ticket_routes() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.run_steps(4);

    // Defaults: pin DeepSeek's route explicitly.
    h.state_mut().open_models(crate::models::Scope::Defaults);
    until(&mut h, "deepseek catalog", |a| a.models.as_ref().is_some_and(|d| !d.needs("deepseek")));
    h.run_steps(2);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.run_steps(2);
    h.get_by_label("保存选择").click();
    until(&mut h, "defaults saved", |a| a.models.is_none() && a.composer.pending == 0);
    let route = serde_json::json!({"provider": "test-local", "model": "default", "mode": "cli"});
    assert_eq!(h.state().host.as_ref().unwrap().defaults(), serde_json::json!({"deepseek": route}));

    // A new conversation inherits the defaults.
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("核对");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "done", |a| root_state(a) == "done");
    let ticket = h.state().view.as_ref().unwrap().id;
    assert_eq!(ticket_routes(h.state(), ticket), serde_json::json!({"deepseek": route}));

    // The finished ticket's routes: also pin Codex.
    h.state_mut().open_models(crate::models::Scope::Ticket(ticket));
    until(&mut h, "dialog", |a| a.models.as_ref().is_some_and(|d| !d.needs("deepseek")));
    h.get_by_label("Codex").click();
    until(&mut h, "codex catalog", |a| a.models.as_ref().is_some_and(|d| !d.needs("codex")));
    h.run_steps(2);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.run_steps(2);
    h.get_by_label("保存选择").click();
    until(&mut h, "routes saved", |a| a.models.is_none() && a.composer.pending == 0);
    assert_eq!(
        ticket_routes(h.state(), ticket),
        serde_json::json!({"deepseek": route, "codex": route})
    );
    assert!(kinds(h.state(), ticket).iter().any(|k| k == "routes.changed"));
}

#[test]
fn second_window_on_an_owned_journal_is_a_read_only_observer() {
    let data = tempfile::tempdir().unwrap();
    let _owner = sa_engine::owner::claim(data.path()).unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    assert!(!h.state().writable());
    assert_eq!(h.state().host_error.as_deref(), Some(sa_engine::owner::BUSY));
    assert!(h.query_by_label("输入框").is_none(), "observer must not show the composer");
}

/// Billable: `SA_LIVE_UI=1`. One read-only DeepSeek turn created from the
/// composer of the real window with the real harness adapters.
#[test]
fn live_composer_turn_with_real_deepseek() {
    if std::env::var("SA_LIVE_UI").as_deref() != Ok("1") {
        eprintln!("set SA_LIVE_UI=1 to run a billable window turn");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let nonce = format!("{:x}", std::process::id() as u64 * 7919 + 17);
    std::fs::write(project.path().join("challenge.txt"), &nonce).unwrap();
    let dir = data.path().to_path_buf();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 860.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = Host::start(dir.clone(), move || ctx.request_repaint()).map_err(|e| format!("{e:#}"));
            let worker = worker::spawn(crate::data::StoreSource::opener(dir.clone()), cc.egui_ctx.clone());
            let opts = Options { data: Some(dir), ..Default::default() };
            App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host)
        });
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.state_mut().composer.lead = "deepseek".into();
    h.run_steps(4);
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("Read challenge.txt in this project. Return only the exact text inside it. Do not modify any files.");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    let end = Instant::now() + Duration::from_secs(240);
    loop {
        h.step();
        let state = root_state(h.state());
        if matches!(state.as_str(), "done" | "failed" | "partial" | "interrupted" | "cancelled") {
            break;
        }
        assert!(Instant::now() < end, "timed out; message={:?}", h.state().composer.message);
        std::thread::sleep(Duration::from_millis(100));
    }
    let ticket = h.state().view.as_ref().unwrap().id;
    let row = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.one("SELECT state,result FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket]))
        .unwrap()
        .unwrap();
    eprintln!("state={} matched={}", row["state"], row["result"].as_str().map(str::trim) == Some(nonce.as_str()));
    assert_eq!(row["state"], "done");
    assert_eq!(row["result"].as_str().map(str::trim), Some(nonce.as_str()));
    drop(h);
    if let Ok(keep) = std::env::var("SA_LIVE_UI_KEEP") {
        let keep = std::path::PathBuf::from(keep);
        std::fs::create_dir_all(&keep).unwrap();
        for f in ["hub.sqlite3", "hub.sqlite3-wal", "hub.sqlite3-shm"] {
            let _ = std::fs::copy(data.path().join(f), keep.join(f));
        }
        eprintln!("journal kept in {}", keep.display());
    }
}
