//! Real egui clicks against a temporary journal. Action assertions stop at the
//! UI boundary; engine integration tests own Git mutations and native turns.
use crate::{
    app::{App, Tab},
    cli::Options,
    host::{Action, Host, tests::fakes},
    theme::Palette,
    worker,
    workflow::WorkflowState,
};
use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::{NodeT, Queryable}};
use sa_engine::db::{Db, Who};
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn seed(data: &Path) -> i64 {
    let db = Db::open(data).unwrap();
    let seq=db.tx(|q| {
        for ticket in 1..=2 {
            q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created,permission_profile) VALUES(?,'workflow fixture','.',1,'codex',1,'ask')", &[&ticket])?;
            q.run("INSERT INTO jobs(id,ticket,agent,body,writing,depth,state,result) VALUES(?,?,'codex','fixture',1,0,'done','complete')", &[&ticket,&ticket])?;
        }
        q.run("INSERT INTO job_workspaces(job,ticket,source,path,branch,head,state,validation) VALUES(1,1,'.','fixture/worktree','codex/test','abc','validated','{\"passed\":true}')", &[])?;
        q.log(Who{ticket:1,id:1,turn:1},"reply",&json!("completed fixture"))
    }).unwrap();
    db.close();
    seq
}

struct State {
    host: Host,
    ui: WorkflowState,
    ticket: i64,
    actions: Vec<Action>,
    /// App fonts are installed on the first frame.
    fonts: bool,
    checks: bool,
    checks_cwd: String,
}

fn harness(data: &Path) -> Harness<'static, State> {
    let host = Host::start_with(data.to_path_buf(), fakes, || {}).unwrap();
    Harness::builder()
        .with_size(egui::vec2(1200.0, 1000.0))
        .build_ui_state(
            |ui, state: &mut State| {
                // The panel draws with the app's fonts (semibold headings).
                if !state.fonts {
                    state.fonts = true;
                    crate::theme::install_fonts(ui.ctx());
                    return;
                }
                let action = if state.checks {
                    state.ui.checks(ui, &Palette::light(), Some(&state.host), &state.checks_cwd, None, false)
                } else {
                    state.ui.show(ui, &Palette::light(), &state.host, state.ticket)
                };
                if let Some(action) = action
                {
                    state.actions.push(action);
                }
            },
            State {
                host,
                ui: WorkflowState::default(),
                ticket: 1,
                actions: vec![],
                fonts: false,
                checks: false,
                checks_cwd: ".".into(),
            },
        )
}

#[test]
fn workspace_actions_require_valid_input_and_explicit_confirmation() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.get_by_label("隔离工作区与合入").click();
    h.run_steps(2);
    h.get_by_label("将后续写入子任务放入独立 Git 工作区")
        .click();
    h.run_steps(2);
    assert!(matches!(
        h.state_mut().actions.pop(),
        Some(Action::SetIsolatedWriters {
            ticket: 1,
            enabled: true
        })
    ));
    h.get_by_label("验证 #1").click();
    h.run_steps(2);
    assert!(h.state().actions.is_empty());
    assert!(
        h.query_by_label("请输入命令参数数组，例如 [\"git\",\"diff\",\"--check\"]")
            .is_some()
    );
    let input = h.get_by_role(Role::TextInput);
    input.focus();
    input.type_text(r#"["git","diff","--check"]"#);
    h.run_steps(2);
    h.get_by_label("验证 #1").click();
    h.run_steps(2);
    match h.state_mut().actions.pop() {
        Some(Action::ValidateWorkspace { job, command }) => {
            assert_eq!(job, 1);
            assert_eq!(command, vec!["git", "diff", "--check"]);
        }
        other => panic!("unexpected action: {other:?}"),
    }
    h.get_by_label("合入 #1…").click();
    h.run_steps(2);
    assert!(h.state().actions.is_empty());
    h.get_by_label("暂不合入").click();
    h.run_steps(2);
    assert!(h.state().actions.is_empty());
    assert!(h.query_by_label("确认验证并合入").is_none());
    h.get_by_label("合入 #1…").click();
    h.run_steps(2);
    // Switching tickets discards a pending confirmation instead of carrying it
    // to another project. Returning requires another explicit confirmation.
    h.state_mut().ticket = 2;
    h.run_steps(2);
    h.state_mut().ticket = 1;
    h.run_steps(2);
    assert!(h.query_by_label("确认验证并合入").is_none());
    h.get_by_label("合入 #1…").click();
    h.run_steps(2);
    h.get_by_label("确认验证并合入").click();
    h.run_steps(2);
    assert!(matches!(
        h.state_mut().actions.pop(),
        Some(Action::IntegrateWorkspace { job: 1 })
    ));
    assert!(h.state().actions.is_empty());
}

#[test]
fn checkpoint_fork_can_be_cancelled_and_emits_one_confirmed_action() {
    let data = tempfile::tempdir().unwrap();
    let seq = seed(data.path());
    let mut h = harness(data.path());
    h.get_by_label("从历史检查点分支").click();
    h.run_steps(2);
    h.get_by_label("建立分支…").click();
    h.run_steps(2);
    assert!(h.query_by_label("确认沿用权限并启动").is_none());
    h.get_by_role(Role::ComboBox).click();
    h.run_steps(2);
    h.get_by_label(&format!("第 1 轮 · 检查点 #{seq}")).click();
    h.run_steps(2);
    let input = h.get_by_role(Role::MultilineTextInput);
    input.focus();
    input.type_text("核对另一条实现路径");
    h.run_steps(2);
    h.get_by_label("建立分支…").click();
    h.run_steps(2);
    assert!(h.state().actions.is_empty());
    h.get_by_label("取消分支").click();
    h.run_steps(2);
    assert!(h.state().actions.is_empty());
    assert!(h.query_by_label("确认沿用权限并启动").is_none());
    h.get_by_label("建立分支…").click();
    h.run_steps(2);
    h.get_by_label("确认沿用权限并启动").click();
    h.run_steps(2);
    match h.state_mut().actions.pop() {
        Some(Action::ForkCheckpoint {
            ticket,
            seq: actual,
            goal,
        }) => {
            assert_eq!(ticket, 1);
            assert_eq!(actual, seq);
            assert_eq!(goal, "核对另一条实现路径");
        }
        other => panic!("unexpected action: {other:?}"),
    }
    assert!(h.state().actions.is_empty());
    assert!(h.query_by_label("确认沿用权限并启动").is_none());
}

#[test]
fn observer_collaboration_panel_never_exposes_workflow_writes() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let owner = Host::start_with(data.path().to_path_buf(), fakes, || {}).unwrap();
    owner.engine().set_project_checks(".", vec![vec!["git".into(), "diff".into(), "--check".into()]]).unwrap();
    let before = owner
        .engine()
        .db()
        .with(|q| q.int("SELECT count(*) FROM events", &[]))
        .unwrap();
    let dir = data.path().to_path_buf();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 1000.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let host = Host::start_with(dir.clone(), fakes, || {}).map_err(|e| format!("{e:#}"));
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            let opts = Options {
                data: Some(dir),
                ..Default::default()
            };
            let mut app = App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host);
            app.inspector_open = true;
            app.tab = Tab::Collab;
            app.select(Some(1));
            app
        });
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        h.step();
        if h.state().view.as_ref().is_some_and(|v| v.snap.is_some()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(2);
    assert!(
        h.state().view.as_ref().is_some_and(|v| v.snap.is_some()),
        "fixture ticket must be visible"
    );
    assert!(!h.state().writable());
    assert_eq!(
        h.state().host_error.as_deref(),
        Some(sa_engine::owner::BUSY)
    );
    h.get_by_label("完成检查").click();
    h.run_steps(3);
    let end = Instant::now() + Duration::from_secs(5);
    while h.query_by_label(r#"["git","diff","--check"]"#).is_none() {
        assert!(Instant::now() < end, "observer checks did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    for label in [
        "完成检查命令",
        "推荐检查",
        "采用推荐检查",
        "添加检查",
        "保存完成检查",
        "移除检查 1",
        "隔离工作区与合入",
        "从历史检查点分支",
        "验证 #1",
        "合入 #1…",
        "确认验证并合入",
        "确认沿用权限并启动",
        "PR 与 CI 反馈",
        "绑定 PR",
        "读取反馈 #1",
    ] {
        assert!(
            h.query_by_label(label).is_none(),
            "observer exposed {label}"
        );
    }
    assert_eq!(
        owner
            .engine()
            .db()
            .with(|q| q.int("SELECT count(*) FROM events", &[]))
            .unwrap(),
        before
    );
}

#[test]
fn completion_checks_add_remove_save_and_limit() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.state_mut().checks = true;
    h.run_steps(3);
    h.get_by_label("完成检查").click();
    h.run_steps(3);
    let end = Instant::now() + Duration::from_secs(5);
    while h.get_by_label("添加检查").accesskit_node().is_disabled() {
        assert!(Instant::now() < end, "completion checks did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    for index in 1..=4 {
        let input = h.get_by_role_and_label(Role::TextInput, "完成检查命令");
        input.focus();
        input.type_text(&format!(r#"["git","check-{index}"]"#));
        h.run_steps(2);
        h.get_by_label("添加检查").click();
        h.run_steps(2);
        assert!(h.query_by_label(&format!("移除检查 {index}")).is_some());
    }
    assert!(h.get_by_label("添加检查").accesskit_node().is_disabled());
    assert!(h.query_by_label("移除检查 5").is_none());
    h.get_by_label("移除检查 2").click();
    h.run_steps(2);
    assert!(!h.get_by_label("添加检查").accesskit_node().is_disabled());
    assert!(h.query_by_label("移除检查 4").is_none());
    h.get_by_label("保存完成检查").click();
    h.run_steps(2);
    let action = h.state_mut().actions.pop().expect("save action");
    let (request, expected) = match &action {
        Action::SaveProjectChecks { cwd, checks, request } => {
            assert_eq!(cwd, ".");
            assert_eq!(checks, &vec![vec!["git".to_owned(), "check-1".to_owned()], vec!["git".to_owned(), "check-3".to_owned()], vec!["git".to_owned(), "check-4".to_owned()]]);
            (*request, checks.clone())
        }
        other => panic!("unexpected action: {other:?}"),
    };
    assert!(h.state().host.submit(action));
    let result = h.state().host.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(result.result.is_ok(), "{:?}", result.result);
    h.state_mut().ui.checks_result(request, result.result.map(|_| ()));
    assert_eq!(h.state().host.engine().project_checks(".").unwrap(), expected);
    // A fresh editor must load the persisted project commands.
    h.state_mut().ui = WorkflowState::default();
    h.run_steps(3);
    let end = Instant::now() + Duration::from_secs(5);
    while h.query_by_label("移除检查 3").is_none() {
        assert!(Instant::now() < end, "saved checks did not reload");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.query_by_label(r#"["git","check-3"]"#).is_some());
}

#[test]
fn completion_check_engine_validation_returns_inline_and_unlocks_editor() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.state_mut().checks = true;
    h.run_steps(3);
    h.get_by_label("完成检查").click();
    h.run_steps(3);
    let end = Instant::now() + Duration::from_secs(5);
    while h.get_by_label("添加检查").accesskit_node().is_disabled() {
        assert!(Instant::now() < end, "completion checks did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let input = h.get_by_role_and_label(Role::TextInput, "完成检查命令");
    input.focus();
    input.type_text(r#"[""]"#);
    h.run_steps(2);
    h.get_by_label("添加检查").click();
    h.run_steps(2);
    h.get_by_label("保存完成检查").click();
    h.run_steps(2);
    let action = h.state_mut().actions.pop().unwrap();
    let request = match &action { Action::SaveProjectChecks { request, .. } => *request, other => panic!("unexpected action: {other:?}") };
    h.state().host.submit(action);
    let outcome = h.state().host.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(outcome.result.is_err());
    h.state_mut().ui.checks_result(request, outcome.result.map(|_| ()));
    h.run_steps(3);
    assert!(h.query_by_label("保存 . 的完成检查失败：验证命令需要 JSON 参数数组，最多 64 项").is_some());
    assert!(!h.get_by_label("保存完成检查").accesskit_node().is_disabled());
    assert!(h.state().host.engine().project_checks(".").unwrap().is_empty());
}

#[test]
fn recommended_checks_require_one_explicit_adoption_and_preserve_existing() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("package.json"), r#"{"scripts":{"test":"node test.js","lint":"eslint ."},"packageManager":"npm@11"}"#).unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    let cwd = project.path().to_string_lossy().into_owned();
    let original = vec![vec!["git".into(), "diff".into(), "--check".into()]];
    h.state().host.engine().set_project_checks(&cwd, original.clone()).unwrap();
    h.state_mut().checks_cwd = cwd.clone();
    h.state_mut().checks = true;
    h.run_steps(3);
    h.get_by_label("完成检查").click();
    let end = Instant::now() + Duration::from_secs(5);
    while h.query_by_label("推荐检查").is_none() || h.query_by_label("移除检查 1").is_none() {
        assert!(Instant::now() < end, "recommendations did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(3);
    if h.query_by_label("采用推荐检查").is_none() {
        h.get_by_label("推荐检查").click();
        h.run_steps(3);
    }
    assert!(h.state().actions.is_empty());
    assert_eq!(h.state().host.engine().project_checks(&cwd).unwrap(), original);
    h.get_by_label("采用推荐检查").click();
    h.run_steps(3);
    let action = h.state_mut().actions.pop().unwrap();
    let (request, expected) = match &action {
        Action::SaveProjectChecks { cwd: actual, checks, request } => {
            assert_eq!(actual, &cwd);
            assert_eq!(checks.len(), 3);
            assert_eq!(checks[0], original[0]);
            assert_eq!(checks[1], ["npm", "run", "test"]);
            (*request, checks.clone())
        }
        other => panic!("unexpected action {other:?}"),
    };
    // Visit a different project and return before the write runs. The old
    // project read must not overwrite the eventual save acknowledgement.
    let other_project = tempfile::tempdir().unwrap();
    h.state_mut().checks_cwd = other_project.path().to_string_lossy().into_owned();
    h.run_steps(3);
    h.state_mut().checks_cwd = cwd.clone();
    h.run_steps(3);
    assert!(h.get_by_label("保存完成检查").accesskit_node().is_disabled());
    assert!(h.state().host.submit(action));
    let outcome = h.state().host.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    h.state_mut().ui.checks_result(request, outcome.result.map(|_| ()));
    h.run_steps(3);
    assert_eq!(h.state().host.engine().project_checks(&cwd).unwrap(), expected);
    assert!(h.get_by_label("采用推荐检查").accesskit_node().is_disabled());
    assert!(h.state().actions.is_empty());
}

#[test]
fn queue_confirmation_binds_the_displayed_candidate_and_resets_on_change() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.state().host.engine().db().tx(|q| {
        q.run("INSERT INTO integration_queue(job,ticket,turn,state,version,commands,validation) VALUES(1,1,1,'awaiting_confirmation','integration-queue/1','[]','{\"candidate_head\":\"same-head\",\"confirmation_token\":\"old-receipt\"}')", &[])?;
        Ok(())
    }).unwrap();
    h.get_by_label("隔离工作区与合入").click();
    h.run_steps(3);
    assert!(h.query_by_label("合入 #1…").is_none(), "queue cannot use manual integration bypass");
    h.get_by_label("自动验证合入队列，合入前由我确认").click();
    h.run_steps(3);
    assert!(matches!(h.state_mut().actions.pop(), Some(Action::SetIntegrationQueue {ticket:1,enabled:true})));
    h.get_by_label("确认候选 #1…").click();
    h.run_steps(3);
    assert!(h.state().actions.is_empty());
    h.state().host.engine().db().tx(|q| {
        q.run("UPDATE integration_queue SET validation='{\"candidate_head\":\"same-head\",\"confirmation_token\":\"new-receipt\"}' WHERE job=1", &[])?;
        Ok(())
    }).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("确认合入候选 #1").is_none());
    h.get_by_label("确认候选 #1…").click();
    h.run_steps(3);
    h.get_by_label("暂不合入候选 #1").click();
    h.run_steps(3);
    assert!(h.state().actions.is_empty());
    h.get_by_label("确认候选 #1…").click();
    h.run_steps(3);
    h.get_by_label("确认合入候选 #1").click();
    h.run_steps(3);
    assert!(matches!(h.state_mut().actions.pop(), Some(Action::ConfirmIntegration {job:1,confirmation_token}) if confirmation_token=="new-receipt"));
}

fn delivery_app(data: &Path) -> Harness<'static, App> {
    let dir = data.to_owned();
    Harness::builder().with_size(egui::vec2(1600.0, 1900.0)).build_eframe(move |cc| {
        crate::theme::install_fonts(&cc.egui_ctx);
        let host = Host::start_with(dir.clone(), fakes, || {}).map_err(|e| format!("{e:#}"));
        let worker = worker::spawn(crate::data::StoreSource::opener(dir.clone()), cc.egui_ctx.clone());
        let opts = Options { data: Some(dir), ..Default::default() };
        let mut app = App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host);
        app.inspector_open = true;
        app.tab = Tab::Collab;
        app.select(Some(1));
        app
    })
}

fn wait_delivery_ui(h: &mut Harness<'static, App>, ready: impl Fn(&Harness<'static, App>) -> bool) {
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        h.run_steps(3);
        if ready(h) { return; }
        assert!(Instant::now() < end, "delivery UI did not reach expected state");
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn delivery_ui_records_captures_and_revises_without_changing_done() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed(data.path());
    let db = Db::open(data.path()).unwrap();
    db.with(|q| q.run("UPDATE tickets SET cwd=? WHERE id=1", &[&project.path().to_string_lossy().as_ref()])).unwrap();
    db.close();
    std::fs::write(project.path().join("answer.txt"), b"expected").unwrap();
    let mut h = delivery_app(data.path());
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("记录需求").is_some());
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").type_text("交付预先约定的文件内容");
    h.get_by_label("文件验收标准（可选）").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::TextInput, "验收文件（项目相对路径）").focus();
    h.get_by_role_and_label(Role::TextInput, "验收文件（项目相对路径）").type_text("answer.txt");
    h.get_by_role_and_label(Role::TextInput, "预期内容指纹（SHA-256）").focus();
    h.get_by_role_and_label(Role::TextInput, "预期内容指纹（SHA-256）").type_text(&sa_store::delivery::sha256(b"expected").to_uppercase());
    h.run_steps(3);
    h.get_by_label("记录需求").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 1 · 1 项需求").is_some());
    h.get_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）").type_text("answer.txt");
    h.run_steps(3);
    h.get_by_label("捕获交付快照").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("最近交付快照 #1 · 需求版本 1 · 1 个文件").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.requirements[0].status, "pass");
    assert_eq!(summary.requirements[0].source, "operator");
    assert!(h.query_by_label("#1 · 文件指纹符合标准 · 交付预先约定的文件内容").is_some());
    h.get_by_label("修订需求 #1").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").type_text("，补充要求");
    h.run_steps(3);
    h.get_by_label("保存需求修订").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 2 · 1 项需求").is_some());
    assert_eq!(sa_store::delivery::read_summary(data.path(), 1).unwrap().requirements[0].status, "stale");
    let state = h.state().host.as_ref().unwrap().engine().db().with(|q| q.one("SELECT state FROM jobs WHERE id=1", &[])).unwrap().unwrap();
    assert_eq!(state["state"], "done");
}

#[test]
fn delivery_observer_can_read_but_cannot_mutate_requirements() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let owner = Host::start_with(data.path().to_owned(), fakes, || {}).unwrap();
    owner.engine().save_requirement(1, 0, None, "尚待人工核查的要求".into(), None, None, Some(1)).unwrap();
    let before = owner.engine().db().with(|q| q.int("SELECT count(*) FROM events", &[])).unwrap();
    let mut h = delivery_app(data.path());
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    assert!(!h.state().writable());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 1 · 1 项需求").is_some());
    assert!(h.query_by_label("#1 · 尚未验证 · 尚待人工核查的要求").is_some());
    for label in ["记录需求", "捕获交付快照", "修订需求 #1", "需求说明", "启用快照交付核验（由我控制）", "核验快照交付", "调整必需状态 #1"] {
        assert!(h.query_by_label(label).is_none(), "observer exposed {label}");
    }
    assert_eq!(owner.engine().db().with(|q| q.int("SELECT count(*) FROM events", &[])).unwrap(), before);
}

#[test]
fn delivery_gate_ui_blocks_unknown_then_verifies_exact_snapshot() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed(data.path());
    let db = Db::open(data.path()).unwrap();
    db.with(|q| q.run("UPDATE tickets SET cwd=? WHERE id=1", &[&project.path().to_string_lossy().as_ref()])).unwrap();
    db.close();
    std::fs::write(project.path().join("answer.txt"), b"expected").unwrap();
    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.save_requirement(1, 0, None, "交付约定内容".into(), None, None, Some(1)).unwrap();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("快照交付核验已关闭").is_some());
    assert!(!sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.enabled);
    h.get_by_label("启用快照交付核验（由我控制）").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("核验快照交付").is_some() && sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.enabled);
    h.get_by_label("核验快照交付").click();
    h.run_steps(3);
    assert!(sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.latest_assessment.is_none());
    h.get_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）").type_text("answer.txt");
    h.run_steps(3);
    h.get_by_label("捕获交付快照").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("最近交付快照 #1 · 需求版本 1 · 1 个文件").is_some());
    h.get_by_label("核验快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("最近快照核验 #1 · 快照 #1 · 需求版本 1 · 策略版本 1").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.gate.status, "blocked");
    assert_eq!(summary.gate.latest_assessment.unwrap().unmet[0].requirement_id, 1);
    wait_delivery_ui(&mut h, |h| h.query_by_label("修订需求 #1").is_some());
    h.get_by_label("修订需求 #1").click();
    h.run_steps(3);
    h.get_by_label("文件验收标准（可选）").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::TextInput, "验收文件（项目相对路径）").focus();
    h.get_by_role_and_label(Role::TextInput, "验收文件（项目相对路径）").type_text("answer.txt");
    h.get_by_role_and_label(Role::TextInput, "预期内容指纹（SHA-256）").focus();
    h.get_by_role_and_label(Role::TextInput, "预期内容指纹（SHA-256）").type_text(&sa_store::delivery::sha256(b"expected"));
    h.run_steps(3);
    h.get_by_label("保存需求修订").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 2 · 1 项需求").is_some());
    h.get_by_label("捕获交付快照").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("最近交付快照 #2 · 需求版本 2 · 1 个文件").is_some());
    h.get_by_label("核验快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("已满足快照必需标准").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.gate.latest_assessment.unwrap().snapshot_id, 2);
    h.get_by_label("接受此快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("确认接受快照 #2 · 核验回执 #2 · 快照交付").is_some());
    assert!(sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.acceptance.is_none());
    h.get_by_label("确认接受快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("已接受快照交付 · 快照 #2 · 核验回执 #2 · 接受记录 #1").is_some());
    std::fs::write(project.path().join("answer.txt"), b"later change").unwrap();
    h.get_by_label("刷新交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("当前需求证据 · 证据已过期").is_some());
    assert!(h.query_by_label("已满足快照必需标准").is_some());
    assert!(h.query_by_label("已接受快照交付 · 快照 #2 · 核验回执 #2 · 接受记录 #1").is_some());
    let state = h.state().host.as_ref().unwrap().engine().db().with(|q| q.one("SELECT state FROM jobs WHERE id=1", &[])).unwrap().unwrap();
    assert_eq!(state["state"], "done");
}

#[test]
fn delivery_disposition_ui_requires_reason_and_stales_snapshot() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed(data.path());
    let db = Db::open(data.path()).unwrap();
    db.with(|q| q.run("UPDATE tickets SET cwd=? WHERE id=1", &[&project.path().to_string_lossy().as_ref()])).unwrap();
    db.close();
    std::fs::write(project.path().join("answer.txt"), b"expected").unwrap();
    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.save_requirement(1, 0, None, "约定文件".into(), Some("answer.txt".into()), Some(sa_store::delivery::sha256(b"expected")), None).unwrap();
    engine.save_requirement(1, 1, None, "后续人工核查".into(), None, None, None).unwrap();
    engine.capture_delivery(1, 2, vec!["answer.txt".into()]).unwrap();
    engine.set_delivery_gate(1, 0, true).unwrap();
    let assessment = engine.prepare_delivery(1, 2, 1, 1).unwrap();
    assert_eq!(assessment.status, "partial");
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("调整必需状态 #2").is_some());
    h.get_by_label("调整必需状态 #2").click();
    h.run_steps(3);
    h.get_by_label("可选").click();
    h.run_steps(3);
    h.get_by_label("保存必需状态 #2").click();
    h.run_steps(3);
    assert_eq!(sa_store::delivery::read_summary(data.path(), 1).unwrap().contract_revision, 2);
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #2").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #2").type_text("操作员决定留待下一阶段核查");
    h.run_steps(3);
    h.get_by_label("保存必需状态 #2").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 3 · 2 项需求").is_some());
    assert!(h.query_by_label("需求 #2 · 可选").is_some());
    assert!(h.query_by_label("操作员调整理由 · 操作员决定留待下一阶段核查").is_some());
    assert!(h.query_by_label("快照核验已过期，请重新核验").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert!(!summary.requirements[1].required);
    assert_eq!(summary.latest_snapshot.unwrap().contract_revision, 2);
    assert_eq!(summary.gate.latest_assessment.unwrap().contract_revision, 2);
    h.get_by_label("调整必需状态 #2").click();
    h.run_steps(3);
    h.get_by_label("撤销").click();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #2").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #2").type_text("操作员撤销本阶段要求");
    h.run_steps(3);
    h.get_by_label("保存必需状态 #2").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 4 · 2 项需求").is_some());
    assert!(h.query_by_label("需求 #2 · 已撤销").is_some());
    assert!(h.query_by_label("操作员调整理由 · 操作员撤销本阶段要求").is_some());
    assert!(sa_store::delivery::read_summary(data.path(), 1).unwrap().requirements[1].withdrawn);
}

#[test]
fn delivery_editors_keep_opening_revision_after_external_change_and_refresh() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = delivery_app(data.path());
    h.state().host.as_ref().unwrap().engine().save_requirement(1, 0, None, "初始要求".into(), None, None, None).unwrap();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("修订需求 #1").is_some());
    h.get_by_label("修订需求 #1").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").type_text("，未提交草稿");
    h.get_by_label("调整必需状态 #1").click();
    h.run_steps(3);
    h.get_by_label("可选").click();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #1").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #1").type_text("尚未提交的调整理由");
    h.run_steps(3);
    assert!(!h.get_by_label("保存需求修订").accesskit_node().is_disabled());
    assert!(!h.get_by_label("保存必需状态 #1").accesskit_node().is_disabled());

    h.state().host.as_ref().unwrap().engine().save_requirement(1, 1, Some(1), "另一操作员已修订的要求".into(), None, None, None).unwrap();
    h.get_by_label("刷新交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 2 · 1 项需求").is_some());
    assert!(h.query_by_label("需求修订草稿基于需求版本 1，当前为版本 2。请比较当前需求并重新打开需求编辑器。").is_some());
    assert!(h.query_by_label("状态调整草稿基于需求版本 1，当前为版本 2。请比较当前需求并重新打开状态编辑器。").is_some());
    assert_eq!(h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").value().as_deref(), Some("初始要求，未提交草稿"));
    assert_eq!(h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #1").value().as_deref(), Some("尚未提交的调整理由"));
    assert!(h.get_by_label("保存需求修订").accesskit_node().is_disabled());
    assert!(h.get_by_label("保存必需状态 #1").accesskit_node().is_disabled());
    h.get_by_label("保存需求修订").click();
    h.get_by_label("保存必需状态 #1").click();
    h.run_steps(3);
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.contract_revision, 2);
    assert_eq!(summary.requirements[0].text, "另一操作员已修订的要求");
    assert!(summary.requirements[0].required);

    h.get_by_label("修订需求 #1").click();
    h.run_steps(3);
    assert_eq!(h.get_by_role_and_label(Role::MultilineTextInput, "需求说明").value().as_deref(), Some("另一操作员已修订的要求"));
    assert!(!h.get_by_label("保存需求修订").accesskit_node().is_disabled());
    h.get_by_label("保存需求修订").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 3 · 1 项需求").is_some());
    assert!(h.get_by_label("保存必需状态 #1").accesskit_node().is_disabled());
    h.get_by_label("调整必需状态 #1").click();
    h.run_steps(3);
    h.get_by_label("可选").click();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #1").focus();
    h.get_by_role_and_label(Role::MultilineTextInput, "调整理由 #1").type_text("比较新需求后决定可选");
    h.run_steps(3);
    h.get_by_label("保存必需状态 #1").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 4 · 1 项需求").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.requirements[0].text, "另一操作员已修订的要求");
    assert!(!summary.requirements[0].required);
}

fn seed_delivery_project(data: &Path, project: &Path) {
    seed(data);
    let db = Db::open(data).unwrap();
    db.with(|q| q.run("UPDATE tickets SET cwd=? WHERE id=1", &[&project.to_string_lossy().as_ref()])).unwrap();
    db.close();
}

#[test]
fn delivery_behavior_ui_adopts_exact_argv_and_checks_frozen_inputs() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed_delivery_project(data.path(), project.path());
    let answer = "answer with space.txt";
    let (verifier, body, argv) = if cfg!(windows) {
        (
            "verify behavior.ps1",
            "$actual = Get-Content -LiteralPath 'answer with space.txt' -Raw; if ($actual -cne 'expected') { exit 23 }",
            vec!["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "verify behavior.ps1"],
        )
    } else {
        (
            "verify behavior.sh",
            "test \"$(cat 'answer with space.txt')\" = expected",
            vec!["sh", "verify behavior.sh"],
        )
    };
    std::fs::write(project.path().join(answer), b"expected").unwrap();
    std::fs::write(project.path().join(verifier), body).unwrap();
    let argv: Vec<String> = argv.into_iter().map(str::to_owned).collect();

    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.set_project_checks(project.path().to_str().unwrap(), vec![vec!["git".into(), "--version".into()], argv.clone()]).unwrap();
    engine.save_requirement(1, 0, None, "检查交付文件的约定行为".into(), None, None, Some(1)).unwrap();
    engine.db().with(|q| q.run(
        "INSERT INTO integration_queue(job,ticket,turn,state,version,commands,validation) SELECT id,ticket,turn,'merged','integration-queue/1','[]','{}' FROM jobs WHERE id=1",
        &[],
    )).unwrap();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("为需求 #1 采用检查").is_some());
    assert!(h.query_by_label("任务执行 · 已完成").is_some());
    h.get_by_label("为需求 #1 采用检查").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("选用完成检查 2").is_some());
    assert!(h.get_by_label("保存采用的检查 #1").accesskit_node().is_disabled());
    h.get_by_label("选用完成检查 2").click();
    let input = h.get_by_role_and_label(Role::MultilineTextInput, "验证器文件（每行一个项目相对路径）");
    input.focus();
    input.type_text(verifier);
    h.run_steps(3);
    h.get_by_label("保存采用的检查 #1").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 2 · 1 项需求").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    let adopted = summary.requirements[0].behavior.as_ref().unwrap();
    assert_eq!(adopted.argv, argv);
    assert_eq!(adopted.verifier_files[0].path, verifier);
    assert!(adopted.receipt.is_none());
    assert!(h.query_by_label("请为当前需求版本捕获快照，再运行检查。").is_some());
    h.get_by_label("启用快照交付核验（由我控制）").click();
    wait_delivery_ui(&mut h, |h| {
        h.query_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）")
            .is_some_and(|node| !node.accesskit_node().is_disabled())
            && sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.enabled
    });

    let input = h.get_by_role_and_label(Role::MultilineTextInput, "交付文件（每行一个项目相对路径）");
    input.focus();
    input.type_text(&format!("{answer}\n{verifier}"));
    h.run_steps(3);
    h.get_by_label("捕获交付快照").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("运行需求 #1 的快照检查").is_some());
    h.get_by_label("核验快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("最近快照核验 #1 · 快照 #1 · 需求版本 2 · 策略版本 1").is_some());
    assert_eq!(sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.status, "blocked");
    std::fs::write(project.path().join(answer), b"changed after capture").unwrap();
    h.get_by_label("运行需求 #1 的快照检查").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("检查回执 #1 · 快照 #1 · 通过").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    let receipt = summary.requirements[0].behavior.as_ref().unwrap().receipt.as_ref().unwrap();
    assert_eq!(receipt.exit_code, Some(0));
    assert_eq!(summary.requirements[0].status, "stale");
    assert_eq!(std::fs::read(project.path().join(answer)).unwrap(), b"changed after capture");
    assert!(h.query_by_label("当前需求证据 · 证据已过期").is_some());
    assert!(h.query_by_label("当前核验尚无人工接受记录。").is_some());
    h.get_by_label("核验快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("已满足快照必需标准").is_some());
    let state = h.state().host.as_ref().unwrap().engine().db().with(|q| q.one("SELECT state,turn FROM jobs WHERE id=1", &[])).unwrap().unwrap();
    assert_eq!(state["state"], "done");
    let merge_label = format!("关联任务 #1 · 第 {} 轮合入队列 · 已本地合入", state["turn"]);
    assert!(h.query_by_label(&merge_label).is_some());

    let mut observer = delivery_app(data.path());
    wait_delivery_ui(&mut observer, |h| h.query_by_label("需求与交付证据").is_some());
    assert!(!observer.state().writable());
    observer.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut observer, |h| h.query_by_label("检查回执 #1 · 快照 #1 · 通过").is_some());
    assert!(observer.query_by_label(&merge_label).is_some());
    assert!(observer.query_by_label("为需求 #1 采用检查").is_none());
    assert!(observer.query_by_label("重新运行需求 #1 的快照检查").is_none());

    // Recreate the fixture state left by an exit between reserving the
    // attempt and recording its final result.
    h.state().host.as_ref().unwrap().engine().db().with(|q| {
        q.run("DELETE FROM delivery_behavior_results WHERE attempt_id=1", &[])
    }).unwrap();
    h.get_by_label("刷新交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("检查回执 #1 · 快照 #1 · 尚无最终结果").is_some());
    assert!(!h.get_by_label("重新运行需求 #1 的快照检查").accesskit_node().is_disabled());
    h.get_by_label("重新运行需求 #1 的快照检查").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("检查回执 #2 · 快照 #1 · 通过").is_some());
}

#[test]
fn delivery_adoption_draft_keeps_opening_contract_version_after_refresh() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed_delivery_project(data.path(), project.path());
    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.set_project_checks(project.path().to_str().unwrap(), vec![vec!["git".into(), "--version".into()]]).unwrap();
    engine.save_requirement(1, 0, None, "第一版".into(), None, None, None).unwrap();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("为需求 #1 采用检查").is_some());
    h.get_by_label("为需求 #1 采用检查").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("选用完成检查 1").is_some());
    h.get_by_label("选用完成检查 1").click();
    h.run_steps(3);
    assert!(!h.get_by_label("保存采用的检查 #1").accesskit_node().is_disabled());

    h.state().host.as_ref().unwrap().engine().save_requirement(1, 1, Some(1), "第二版".into(), None, None, None).unwrap();
    h.get_by_label("刷新交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("需求版本 2 · 1 项需求").is_some());
    assert!(h.get_by_label("保存采用的检查 #1").accesskit_node().is_disabled());
    h.get_by_label("保存采用的检查 #1").click();
    h.run_steps(3);
    assert!(sa_store::delivery::read_summary(data.path(), 1).unwrap().requirements[0].behavior.is_none());
}

#[test]
fn delivery_ui_can_cancel_its_running_check() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let probe = tempfile::tempdir().unwrap();
    seed_delivery_project(data.path(), project.path());
    std::fs::write(project.path().join("answer.txt"), b"snapshot input").unwrap();
    let marker = probe.path().join("started");
    let argv = if cfg!(windows) {
        let path = marker.to_str().unwrap().replace('\'', "''");
        vec!["powershell.exe".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
            format!("[IO.File]::WriteAllText('{path}', 'started'); Start-Sleep -Seconds 30")]
    } else {
        vec!["sh".into(), "-c".into(), "printf started > \"$1\"; sleep 30".into(), "fixture".into(), marker.to_str().unwrap().into()]
    };

    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.set_project_checks(project.path().to_str().unwrap(), vec![argv.clone()]).unwrap();
    engine.save_requirement(1, 0, None, "验证前允许取消".into(), None, None, None).unwrap();
    engine.adopt_delivery_check(1, 1, 1, argv).unwrap();
    engine.capture_delivery(1, 2, vec!["answer.txt".into()]).unwrap();

    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("运行需求 #1 的快照检查").is_some());
    h.get_by_label("运行需求 #1 的快照检查").click();
    wait_delivery_ui(&mut h, |h| marker.is_file() && h.query_by_label("取消本次检查").is_some());
    h.get_by_label("取消本次检查").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("检查回执 #1 · 快照 #1 · 无效").is_some());
    let summary = sa_store::delivery::read_summary(data.path(), 1).unwrap();
    let receipt = summary.requirements[0].behavior.as_ref().unwrap().receipt.as_ref().unwrap();
    assert!(receipt.detail.contains("取消"));
    assert_eq!(receipt.exit_code, None);
    assert!(h.query_by_label("任务执行 · 已完成").is_some());
}

#[test]
fn delivery_ui_accepts_only_the_confirmed_partial_assessment() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    seed_delivery_project(data.path(), project.path());
    std::fs::write(project.path().join("answer.txt"), b"expected").unwrap();
    let mut h = delivery_app(data.path());
    let engine = h.state().host.as_ref().unwrap().engine();
    engine.save_requirement(1, 0, None, "校验交付文件".into(), Some("answer.txt".into()), Some(sa_store::delivery::sha256(b"expected")), None).unwrap();
    engine.save_requirement(1, 1, None, "仍须人工核查".into(), None, None, None).unwrap();
    engine.set_delivery_gate(1, 0, true).unwrap();
    engine.capture_delivery(1, 2, vec!["answer.txt".into()]).unwrap();
    let first = engine.prepare_delivery(1, 2, 1, 1).unwrap();
    assert_eq!(first.status, "partial");

    wait_delivery_ui(&mut h, |h| h.query_by_label("需求与交付证据").is_some());
    h.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("接受此部分交付").is_some());
    assert!(h.query_by_label("接受此快照交付").is_none());
    h.get_by_label("接受此部分交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("确认接受快照 #1 · 核验回执 #1 · 部分交付").is_some());
    assert!(h.query_by_label(&format!("此次仍未满足的必需项 #2 · {}", first.unmet[0].reason)).is_some());
    assert!(sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.acceptance.is_none());
    h.get_by_label("确认接受部分交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("已接受部分交付 · 快照 #1 · 核验回执 #1 · 接受记录 #1").is_some());
    let record = sa_store::delivery::read_summary(data.path(), 1).unwrap().gate.acceptance.unwrap();
    assert_eq!(record.assessment_id, first.id);
    assert_eq!(record.status, "partial");

    h.get_by_label("核验快照交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("待人工接受 · 部分交付 · 快照 #1 · 核验回执 #2").is_some());
    h.get_by_label("接受此部分交付").click();
    wait_delivery_ui(&mut h, |h| h.query_by_label("确认接受快照 #1 · 核验回执 #2 · 部分交付").is_some());
    let third = h.state().host.as_ref().unwrap().engine().prepare_delivery(1, 2, 1, 1).unwrap();
    h.get_by_label("确认接受部分交付").click();
    let pending = format!("待人工接受 · 部分交付 · 快照 #1 · 核验回执 #{}", third.id);
    wait_delivery_ui(&mut h, |h| h.query_by_label(&pending).is_some());
    assert!(h.query_by_label("确认接受部分交付").is_none());
    let history = "历史接受记录 #1 · 部分交付 · 快照 #1 · 核验回执 #1";
    assert!(h.query_by_label(history).is_some());
    let gate = sa_store::delivery::read_summary(data.path(), 1).unwrap().gate;
    assert!(gate.acceptance.is_none());
    assert_eq!(gate.latest_acceptance.unwrap().assessment_id, first.id);

    let mut observer = delivery_app(data.path());
    wait_delivery_ui(&mut observer, |h| h.query_by_label("需求与交付证据").is_some());
    assert!(!observer.state().writable());
    observer.get_by_label("需求与交付证据").click();
    wait_delivery_ui(&mut observer, |h| h.query_by_label(&pending).is_some());
    assert!(observer.query_by_label(history).is_some());
    assert!(observer.query_by_label("接受此部分交付").is_none());
    assert!(observer.query_by_label("确认接受部分交付").is_none());
}

#[test]
fn execution_policy_toggle_and_ledger_report_are_reachable() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.get_by_label("执行策略").click();
    h.run_steps(3);
    h.get_by_label("按规则自动调整（本项目）").click();
    h.run_steps(2);
    assert!(matches!(
        h.state_mut().actions.pop(),
        Some(Action::SetExecutionPolicy { ticket: 1, apply: true })
    ));
    assert!(h.query_by_label("本工单还没有策略记录。").is_some());
    h.get_by_label("用量账目").click();
    h.run_steps(3);
    h.get_by_label("生成用量账目").click();
    let deadline = Instant::now() + Duration::from_secs(20);
    while h.query_by_label_contains("次报告了用量").is_none() {
        assert!(Instant::now() < deadline, "ledger report did not arrive");
        std::thread::sleep(Duration::from_millis(50));
        h.run_steps(1);
    }
    assert!(h.query_by_label_contains("按 API 牌价折算").is_some());
    assert!(h.query_by_label_contains("不是实际账单").is_some());
}

#[test]
fn unknown_platform_operations_ask_for_a_check() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let db = Db::open(data.path()).unwrap();
    db.tx(|q| q.run("INSERT INTO operations(ticket,job,kind,intent,intent_sha256,state,attempts,detail,created,updated)
        VALUES(1,1,'integration.publish','{}','x','unknown_effect',1,'{\"reason\":\"原项目 HEAD 既不是基线也不是候选\"}',1,1)", &[])).unwrap();
    db.close();
    let mut h = harness(data.path());
    h.get_by_label("平台操作 · 1 项待核对").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("合入原项目 · 任务 #1 · 结果未知").is_some());
    assert!(h.query_by_label("原项目 HEAD 既不是基线也不是候选").is_some());
    h.get_by_label("已核对现场 #1").click();
    h.run_steps(2);
    assert!(matches!(h.state_mut().actions.pop(), Some(Action::ResolveOperation { id: 1, .. })));
}

#[test]
fn hydration_paths_are_saved_one_per_line() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let mut h = harness(data.path());
    h.get_by_label("隔离工作区与合入").click();
    h.run_steps(2);
    let input = h.get_by_role(Role::MultilineTextInput);
    input.focus();
    input.type_text("gen\n\n  assets/x  ");
    h.run_steps(2);
    h.get_by_label("保存补齐路径").click();
    h.run_steps(2);
    match h.state_mut().actions.pop() {
        Some(Action::SetWorkspaceHydration { ticket, paths }) => {
            assert_eq!(ticket, 1);
            assert_eq!(paths, ["gen", "assets/x"]);
        }
        other => panic!("unexpected action {other:?}"),
    }
}
