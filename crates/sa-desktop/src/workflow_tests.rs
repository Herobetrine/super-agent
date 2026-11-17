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
                    state.ui.checks(ui, &Palette::light(), Some(&state.host), ".", None, false)
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
    assert!(h.query_by_label("验证命令需要 JSON 参数数组，最多 64 项").is_some());
    assert!(!h.get_by_label("保存完成检查").accesskit_node().is_disabled());
    assert!(h.state().host.engine().project_checks(".").unwrap().is_empty());
}
