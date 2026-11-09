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
            .with(|q| {
                q.all(
                    "SELECT ticket,job,kind,substr(payload,1,160) p FROM events ORDER BY seq",
                    &[],
                )
            })
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
        .with(|q| {
            q.all(
                "SELECT kind FROM events WHERE ticket=? ORDER BY seq",
                &[&ticket],
            )
        })
        .unwrap()
        .into_iter()
        .map(|r| r["kind"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn fork_source(h: &mut Harness<'_, App>, project: &std::path::Path) -> (i64, i64) {
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project).unwrap());
    h.state_mut().composer.lead = "codex".into();
    h.state_mut().composer.routes = Some(serde_json::json!({"codex":{
        "provider":"test-local", "model":"default", "mode":"cli", "reasoning_effort":"high"
    }}));
    h.run_steps(4);
    h.get_by_label("输入框").focus();
    h.get_by_label("输入框").type_text("生成供分支测试的终稿");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(h, "fork source completed", |a| root_state(a) == "done");
    let ticket = h.state().view.as_ref().unwrap().id;
    let seq = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .checkpoints(ticket)
        .unwrap()[0]["seq"]
        .as_i64()
        .unwrap();
    (ticket, seq)
}

fn ticket_count(app: &App) -> usize {
    app.host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.all("SELECT id FROM tickets", &[]))
        .unwrap()
        .len()
}

#[test]
fn workflow_fork_runs_through_host_selects_result_and_keeps_source_policy() {
    use egui_kittest::kittest::NodeT;
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "partial");
    let (source, seq) = fork_source(&mut h, project.path());
    let policy = |app: &App, ticket| {
        app.host
            .as_ref()
            .unwrap()
            .engine()
            .db()
            .with(|q| {
                q.one(
                    "SELECT lead,permission_profile,agent_routes FROM tickets WHERE id=?",
                    &[&ticket],
                )
            })
            .unwrap()
            .unwrap()
    };
    let original = policy(h.state(), source);
    assert_eq!(original["permission_profile"], "partial");
    assert_eq!(original["lead"], "codex");
    h.state_mut().composer.draft = "尚未提交的独立草稿".into();
    h.state_mut().composer.attachments = vec!["unsent.txt".into()];
    h.state_mut().inspector_open = true;
    h.state_mut().tab = Tab::Collab;
    h.run_steps(3);
    h.get_by_label("从历史检查点分支").click();
    h.run_steps(2);
    h.get_by_role(Role::ComboBox).click();
    h.run_steps(2);
    h.get_by_label(&format!("第 1 轮 · 检查点 #{seq}")).click();
    h.run_steps(2);
    let input = h
        .get_all_by_role(Role::MultilineTextInput)
        .find(|n| n.accesskit_node().label().as_deref() != Some("输入框"))
        .unwrap();
    input.focus();
    input.type_text("核对另一种实现");
    h.run_steps(2);
    h.get_by_label("建立分支…").click();
    h.run_steps(2);
    h.get_by_label("取消分支").click();
    h.run_steps(2);
    assert_eq!(ticket_count(h.state()), 1);
    assert_eq!(h.state().view.as_ref().unwrap().id, source);
    h.get_by_label("建立分支…").click();
    h.run_steps(2);
    h.get_by_label("确认沿用权限并启动").click();
    until(&mut h, "fork selected", |a| {
        a.view.as_ref().is_some_and(|v| v.id != source)
    });
    let fork = h.state().view.as_ref().unwrap().id;
    assert_eq!(ticket_count(h.state()), 2);
    assert_eq!(
        h.state().composer.message,
        Some((format!("已建立分支 #{fork}。"), false))
    );
    assert_eq!(h.state().composer.draft, "尚未提交的独立草稿");
    assert_eq!(h.state().composer.attachments, ["unsent.txt"]);
    assert_eq!(policy(h.state(), fork), original);
    assert_eq!(policy(h.state(), source), original);
    until(&mut h, "fork completed", |a| root_state(a) == "done");
    let route = h.state().host.as_ref().unwrap().engine().db().with(|q| q.one(
        "SELECT route FROM native_routes WHERE job=(SELECT id FROM jobs WHERE ticket=? AND parent IS NULL)", &[&fork]
    )).unwrap().unwrap();
    let route: Value = serde_json::from_str(route["route"].as_str().unwrap()).unwrap();
    assert_eq!(route["model"], "default");
    assert_eq!(route["reasoning_effort"], "high");
}

#[test]
fn fork_outcome_after_navigation_preserves_focus_and_prevents_duplicate_submission() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "partial");
    let (source, seq) = fork_source(&mut h, project.path());
    let action = || crate::host::Action::ForkCheckpoint {
        ticket: source,
        seq,
        goal: "另一个分支".into(),
    };
    h.state_mut().submit(action());
    h.state_mut().submit(action());
    assert_eq!(h.state().composer.pending, 1);
    // Navigate before polling the receipt, even if the host has already finished.
    h.state_mut().new_conversation();
    h.state_mut().composer.draft = "新会话草稿".into();
    until(&mut h, "late fork receipt", |a| a.composer.pending == 0);
    assert!(h.state().view.is_none());
    assert_eq!(ticket_count(h.state()), 2);
    assert_eq!(h.state().composer.draft, "新会话草稿");
    assert!(
        h.state()
            .composer
            .message
            .as_ref()
            .is_some_and(|(text, error)| !error && text.contains("可在会话列表打开"))
    );
}

#[test]
fn composer_sends_follows_up_approves_and_edits_the_plan() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "ask");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.run_steps(4);
    assert!(
        h.state().writable(),
        "host error: {:?}",
        h.state().host_error
    );

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
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| !s.approvals.is_empty())
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
        .with(|q| {
            q.one(
                "SELECT result FROM jobs WHERE ticket=? AND parent IS NULL",
                &[&ticket],
            )
        })
        .unwrap()
        .unwrap();
    assert!(
        reply["result"].as_str().unwrap().ends_with("approved"),
        "{reply:?}"
    );

    // Plan: add an item with Enter, then tick it.
    h.state_mut().tab = Tab::Plan;
    h.run_steps(2);
    let field = h.get_by_label("计划项输入");
    field.focus();
    field.type_text("核对附件");
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    until(&mut h, "plan item", |a| {
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| s.plan.len() == 1)
    });
    h.get_by_label("标记为已完成 核对附件").click();
    until(&mut h, "plan done", |a| {
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| s.plan.first().is_some_and(|p| p.done))
    });

    // Follow-up through the send button.
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("继续");
    h.run_steps(2);
    h.get_by_role_and_label(Role::Button, "发送").click();
    // Every `ask` turn requests approval again; deny this one.
    until(&mut h, "second approval", |a| {
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| !s.approvals.is_empty())
    });
    h.state_mut().tab = Tab::Approvals;
    h.run_steps(2);
    h.get_by_label("拒绝").click();
    until(&mut h, "second reply", |a| {
        root_state(a) == "done" && kinds(a, ticket).iter().filter(|k| *k == "reply").count() == 2
    });

    let k = kinds(h.state(), ticket);
    for want in [
        "user",
        "approval.request",
        "approval.decision",
        "plan.add",
        "plan.done",
        "user.follow",
    ] {
        assert!(k.iter().any(|x| x == want), "missing {want}: {k:?}");
    }
    let decisions: Vec<Value> = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| {
            q.all(
                "SELECT decision FROM approvals WHERE ticket=? ORDER BY id",
                &[&ticket],
            )
        })
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
        .with(|q| {
            q.one(
                "SELECT result FROM jobs WHERE ticket=? AND parent IS NULL",
                &[&ticket],
            )
        })
        .unwrap()
        .unwrap();
    assert!(
        last["result"].as_str().unwrap().ends_with("denied"),
        "{last:?}"
    );
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
    until(&mut h, "deepseek catalog", |a| {
        a.models.as_ref().is_some_and(|d| !d.needs("deepseek"))
    });
    h.run_steps(2);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.run_steps(2);
    h.get_by_label("保存选择").click();
    until(&mut h, "defaults saved", |a| {
        a.models.is_none() && a.composer.pending == 0
    });
    let route = serde_json::json!({"provider": "test-local", "model": "default", "mode": "cli"});
    assert_eq!(
        h.state().host.as_ref().unwrap().defaults(),
        serde_json::json!({"deepseek": route})
    );

    // A new conversation inherits the defaults.
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("核对");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "done", |a| root_state(a) == "done");
    let ticket = h.state().view.as_ref().unwrap().id;
    assert_eq!(
        ticket_routes(h.state(), ticket),
        serde_json::json!({"deepseek": route})
    );

    // The finished ticket's routes: also pin Codex.
    h.state_mut()
        .open_models(crate::models::Scope::Ticket(ticket));
    until(&mut h, "dialog", |a| {
        a.models.as_ref().is_some_and(|d| !d.needs("deepseek"))
    });
    h.get_by_label("Codex").click();
    until(&mut h, "codex catalog", |a| {
        a.models.as_ref().is_some_and(|d| !d.needs("codex"))
    });
    h.run_steps(2);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.run_steps(2);
    h.get_by_label("保存选择").click();
    until(&mut h, "routes saved", |a| {
        a.models.is_none() && a.composer.pending == 0
    });
    assert_eq!(
        ticket_routes(h.state(), ticket),
        serde_json::json!({"deepseek": route, "codex": route})
    );
    assert!(
        kinds(h.state(), ticket)
            .iter()
            .any(|k| k == "routes.changed")
    );
}

#[test]
fn knowledge_tab_lists_adds_and_archives_project_entries() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.run_steps(4);
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("整理项目");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "done", |a| root_state(a) == "done");

    // The finished root recorded its progress automatically.
    h.state_mut().inspector_open = true;
    h.state_mut().tab = Tab::Knowledge;
    let entries = |a: &App| {
        a.knowledge
            .entries
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .cloned()
            .unwrap_or_default()
    };
    until(&mut h, "progress entry", |a| {
        entries(a).iter().any(|e| e.kind == "progress")
    });

    let field = h.get_by_label("知识条目输入");
    field.focus();
    field.type_text("构建只用 scripts/cargo.cjs");
    h.run_steps(2);
    h.get_by_label("添加条目").click();
    until(&mut h, "operator entry", |a| {
        entries(a).iter().any(|e| {
            e.agent.as_deref() == Some("operator") && e.body == "构建只用 scripts/cargo.cjs"
        })
    });
    assert!(h.state().knowledge.draft.is_empty());
    let id = entries(h.state())
        .iter()
        .find(|e| e.agent.as_deref() == Some("operator"))
        .unwrap()
        .id;
    h.run_steps(2);
    h.get_by_label(&format!("归档条目 #{id}")).click();
    until(&mut h, "archived", |a| {
        entries(a).iter().any(|e| e.id == id && e.archived)
    });
    let ticket = h.state().view.as_ref().unwrap().id;
    let k = kinds(h.state(), ticket);
    assert!(k.iter().any(|x| x == "knowledge.add"), "{k:?}");
}

#[test]
fn providers_dialog_saves_custom_providers_and_rotation() {
    use sa_engine::runner::RotationSource;
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    h.get_by_label("供应商与轮换").click();
    h.run_steps(2);
    h.get_by_label("添加供应商").click();
    h.run_steps(2);
    for (role, label, text) in [
        (Role::TextInput, "名称", "公司网关"),
        (
            Role::TextInput,
            "接口地址",
            "https://gateway.example.com/v1",
        ),
        (Role::TextInput, "密钥环境变量", "SA_TEST_GATEWAY_KEY"),
        (Role::TextInput, "模型", "model-a"),
    ] {
        let f = h.get_by_role_and_label(role, label);
        f.focus();
        f.type_text(text);
        h.run_steps(1);
    }
    h.get_by_label("保存供应商").click();
    until(&mut h, "provider saved", |a| {
        a.host.as_ref().unwrap().providers().len() == 1
            && a.providers.as_ref().is_some_and(|d| d.form.is_none())
    });
    let p = h.state().host.as_ref().unwrap().providers().remove(0);
    assert_eq!(
        (p.agent.as_str(), p.name.as_str(), p.key_env.as_str()),
        ("claude", "公司网关", "SA_TEST_GATEWAY_KEY")
    );
    assert_eq!(p.models, vec!["model-a".to_string()]);
    // Nothing secret is stored: only the variable name.
    let stored: String = rusqlite::Connection::open(data.path().join(sa_store::DB_FILE))
        .unwrap()
        .query_row(
            "SELECT group_concat(base_url||key_env||models) FROM studio_providers",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored.contains("SA_TEST_GATEWAY_KEY"));

    // Rotation: enable and order two routes for Claude, then save.
    h.run_steps(3);
    assert!(h.state().providers.is_some(), "dialog closed");
    h.get_by_label("轮换").click();
    h.run_steps(2);
    h.get_by_label("启用供应商轮换").click();
    h.run_steps(1);
    let first = sa_store::providers::RouteRef {
        provider: p.route_id(),
        model: "model-a".into(),
        mode: "cli".into(),
    };
    let second = sa_store::providers::RouteRef {
        provider: "test-local".into(),
        model: "default".into(),
        mode: "cli".into(),
    };
    h.state_mut()
        .providers
        .as_mut()
        .unwrap()
        .rotation
        .order
        .insert("claude".into(), vec![first.clone(), second]);
    h.run_steps(2);
    h.get_by_label("保存轮换设置").click();
    until(&mut h, "rotation saved", |a| {
        a.host.as_ref().unwrap().rotation().enabled
    });
    let rot = h.state().host.as_ref().unwrap().rotation();
    assert_eq!(rot.order["claude"].len(), 2);
    let source = crate::host::DbRotation(std::sync::Arc::new(std::sync::RwLock::new(rot)));
    assert_eq!(source.candidates("claude")[0].provider, first.provider);
    assert!(source.candidates("codex").is_empty());

    // Removing the provider also drops it from the rotation order.
    h.run_steps(3);
    h.get_by_label("自定义供应商").click();
    h.run_steps(3);
    assert!(
        h.state()
            .providers
            .as_ref()
            .is_some_and(|d| d.page == crate::providers::Page::Providers),
        "dialog closed or page unchanged"
    );
    h.get_by_label("删除供应商 公司网关").click();
    until(&mut h, "provider removed", |a| {
        a.host.as_ref().unwrap().providers().is_empty()
    });
    assert_eq!(
        h.state().host.as_ref().unwrap().rotation().order["claude"].len(),
        1
    );
}

/// The `sa-ext` fixture MCP server next to this test binary (built by
/// `cargo build --workspace`), if present.
fn fixture_mcp() -> Option<std::path::PathBuf> {
    let deps = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let exe = deps.parent()?.join(format!(
        "sa-ext-fixture-mcp{}",
        std::env::consts::EXE_SUFFIX
    ));
    exe.is_file().then_some(exe)
}

#[test]
fn extensions_dialog_grants_skills_and_probed_mcp_servers() {
    let data = tempfile::tempdir().unwrap();
    let skill = tempfile::tempdir().unwrap();
    std::fs::write(
        skill.path().join("SKILL.md"),
        "---\nname: sa-ui-test-skill\ndescription: 界面测试技能\n---\n# 说明\n",
    )
    .unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    let skill_id = h
        .state()
        .host
        .as_ref()
        .unwrap()
        .ext()
        .import_skill(skill.path())
        .unwrap();

    h.get_by_label("MCP 与技能").click();
    let loaded = |a: &App| {
        a.extensions
            .as_ref()
            .is_some_and(|d| matches!(d.data, Some(Ok(_))))
    };
    until(&mut h, "extension list", loaded);
    h.run_steps(3);
    // The machine's own skills are listed too: filter down to the test skill.
    let search = h.get_by_role_and_label(Role::TextInput, "搜索技能或来源");
    search.focus();
    search.type_text("sa-ui-test");
    h.run_steps(3);
    h.get_by_label("选择 sa-ui-test-skill").click();
    h.run_steps(2);

    let fixture = fixture_mcp();
    if let Some(exe) = &fixture {
        let secret = data.path().join("mcp-secret");
        std::fs::write(&secret, "s3cret").unwrap();
        h.get_by_label("MCP 服务 0").click();
        h.run_steps(3);
        h.get_by_label("添加服务").click();
        h.run_steps(3);
        let args = serde_json::to_string(&[secret.display().to_string()]).unwrap();
        for (label, text) in [
            ("服务名称", "proof"),
            ("可执行程序", &*exe.display().to_string()),
            ("启动参数", &*args),
        ] {
            if label == "启动参数"
                && let Some(crate::extensions::View::Edit(f)) =
                    h.state_mut().extensions.as_mut().map(|d| &mut d.view)
            {
                // Replace the initial "[]".
                f.args.clear();
            }
            h.run_steps(1);
            let f = h.get_by_role_and_label(Role::TextInput, label);
            f.focus();
            f.type_text(text);
            h.run_steps(1);
        }
        h.get_by_label("启用服务，允许将它授予会话").click();
        h.run_steps(1);
        h.get_by_label("我信任上述程序或服务来源").click();
        h.run_steps(1);
        h.get_by_label("保存服务").click();
        until(&mut h, "server saved", |a| {
            a.extensions.as_ref().is_some_and(|d| {
                matches!(d.view, crate::extensions::View::List)
                    && d.data
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .is_some_and(|v| v["servers"].as_array().is_some_and(|s| s.len() == 1))
            })
        });
        h.run_steps(3);
        h.get_by_label("探测 proof").click();
        h.run_steps(3);
        h.get_by_label("连接并读取工具").click();
        until(&mut h, "probe", |a| {
            a.extensions.as_ref().is_some_and(|d| {
                matches!(
                    &d.view,
                    crate::extensions::View::Probe {
                        result: Some(_),
                        ..
                    }
                )
            })
        });
        if let Some(crate::extensions::View::Probe {
            result: Some(r), ..
        }) = h.state().extensions.as_ref().map(|d| &d.view)
        {
            let tools = r.as_ref().expect("probe ok");
            assert!(
                tools
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == "challenge"),
                "{tools}"
            );
        }
        h.run_steps(2);
        h.get_by_label("返回列表").click();
        until(&mut h, "list after probe", |a| {
            loaded(a)
                && a.extensions
                    .as_ref()
                    .is_some_and(|d| matches!(d.view, crate::extensions::View::List))
        });
        h.run_steps(3);
        h.get_by_label("选择 proof").click();
        h.run_steps(2);
    } else {
        eprintln!(
            "sa-ext-fixture-mcp not built; MCP part skipped (run `cargo build --workspace` first)"
        );
    }

    h.get_by_label("保存授权").click();
    until(&mut h, "defaults saved", |a| a.extensions.is_none());
    let defaults = h.state().host.as_ref().unwrap().ext().defaults().unwrap();
    assert_eq!(
        defaults["skills"],
        serde_json::json!([skill_id]),
        "{defaults}"
    );
    assert_eq!(
        defaults["servers"].as_array().unwrap().len(),
        usize::from(fixture.is_some()),
        "{defaults}"
    );
}

#[test]
fn command_palette_and_shortcuts() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    h.run_steps(3);
    assert!(h.state().palette.is_some(), "Ctrl+K opens the palette");
    let q = h.get_by_role_and_label(Role::TextInput, "命令搜索");
    q.type_text("供应商");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert!(h.state().palette.is_none());
    assert!(
        h.state().providers.is_some(),
        "Enter runs the filtered command"
    );
    h.state_mut().providers = None;
    h.run_steps(2);
    let open = h.state().inspector_open;
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::J);
    h.run_steps(2);
    assert_ne!(
        h.state().inspector_open,
        open,
        "Ctrl+J toggles the inspector"
    );
    let dark = h.state().dark;
    h.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::L,
    );
    h.run_steps(2);
    assert_ne!(h.state().dark, dark, "Ctrl+Shift+L toggles the theme");
}

#[test]
fn second_window_on_an_owned_journal_is_a_read_only_observer() {
    let data = tempfile::tempdir().unwrap();
    let _owner = sa_engine::owner::claim(data.path()).unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    assert!(!h.state().writable());
    assert_eq!(
        h.state().host_error.as_deref(),
        Some(sa_engine::owner::BUSY)
    );
    assert!(
        h.query_by_label("输入框").is_none(),
        "observer must not show the composer"
    );
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
            let host = Host::start(dir.clone(), move || ctx.request_repaint())
                .map_err(|e| format!("{e:#}"));
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            let opts = Options {
                data: Some(dir),
                ..Default::default()
            };
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
        if matches!(
            state.as_str(),
            "done" | "failed" | "partial" | "interrupted" | "cancelled"
        ) {
            break;
        }
        assert!(
            Instant::now() < end,
            "timed out; message={:?}",
            h.state().composer.message
        );
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
        .with(|q| {
            q.one(
                "SELECT state,result FROM jobs WHERE ticket=? AND parent IS NULL",
                &[&ticket],
            )
        })
        .unwrap()
        .unwrap();
    eprintln!(
        "state={} matched={}",
        row["state"],
        row["result"].as_str().map(str::trim) == Some(nonce.as_str())
    );
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

#[test]
fn sidebar_rename_pin_and_archive() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(4);
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("整理笔记");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "done", |a| root_state(a) == "done");
    let ticket = h.state().view.as_ref().unwrap().id;
    let row = |a: &App| a.history.iter().find(|r| r.id == ticket).cloned();
    h.state_mut().rename = Some((ticket, "周报整理".into()));
    h.run_steps(3);
    h.get_by_label("保存").click();
    until(&mut h, "renamed", |a| {
        row(a).and_then(|r| r.title).as_deref() == Some("周报整理")
    });
    h.state_mut().submit(crate::host::Action::TicketMeta {
        ticket,
        title: None,
        pinned: Some(true),
        archived: None,
    });
    until(&mut h, "pinned", |a| row(a).is_some_and(|r| r.pinned));
    h.state_mut().submit(crate::host::Action::TicketMeta {
        ticket,
        title: None,
        pinned: None,
        archived: Some(true),
    });
    until(&mut h, "archived", |a| row(a).is_some_and(|r| r.archived));
    h.run_steps(3);
    // Archived rows are hidden until the toggle is used.
    assert!(h.query_by_label("显示已归档（1）").is_some());
    // An empty title restores the goal.
    h.state_mut().submit(crate::host::Action::TicketMeta {
        ticket,
        title: Some(String::new()),
        pinned: None,
        archived: None,
    });
    until(&mut h, "title cleared", |a| {
        row(a).is_some_and(|r| r.title.is_none())
    });
}

#[test]
fn live_badge_and_failover_toggle() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "ask");
    h.run_steps(4);
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("等待审批");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    // The `ask` fake waits on an approval: the lead's live badge says so.
    until(&mut h, "approval badge", |a| {
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| !s.approvals.is_empty())
    });
    h.state_mut().inspector_open = true;
    h.state_mut().tab = Tab::Collab;
    let root = h
        .state()
        .view
        .as_ref()
        .and_then(|v| v.snap.as_ref())
        .unwrap()
        .jobs[0]
        .id;
    let mut badge = None;
    for _ in 0..80 {
        h.step();
        badge = h.state_mut().live_badges().remove(&root).map(|b| b.0);
        if badge.as_deref() == Some("等待审批") {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(badge.as_deref(), Some("等待审批"));
    let ticket = h.state().view.as_ref().unwrap().id;
    h.run_steps(2);
    h.get_by_label("只读子任务的执行端不可用时自动改派").click();
    until(&mut h, "failover on", |a| {
        a.host.as_ref().unwrap().failover(ticket)
    });
}

#[test]
fn at_mention_attaches_project_files() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(project.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(project.path().join("notes.md"), "x").unwrap();
    let mut h = harness(data.path(), "readonly");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.run_steps(4);
    let editor = h.get_by_label("输入框");
    editor.focus();
    editor.type_text("检查 @mai");
    until(&mut h, "suggestions", |a| {
        a.composer
            .index
            .as_ref()
            .and_then(|i| i.files.as_ref())
            .is_some_and(|f| f.is_ok())
    });
    h.run_steps(3);
    assert!(
        h.query_by_label("src/main.rs").is_some(),
        "suggestion row shown"
    );
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(
        h.state().composer.attachments,
        vec!["src/main.rs".to_string()]
    );
    assert_eq!(h.state().composer.draft, "检查 ");
    assert!(
        h.state().view.is_none(),
        "Enter picked the file instead of sending"
    );
}

#[test]
fn existing_conversation_accepts_a_dropped_image_and_shows_usage() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let picture = project.path().join("截图.PNG");
    image::RgbImage::new(3, 2)
        .save_with_format(&picture, image::ImageFormat::Png)
        .unwrap();
    let expected = std::fs::read(&picture).unwrap();
    let mut h = harness(data.path(), "readonly");
    h.state_mut().composer.project = Some(sa_engine::services::realpath(project.path()).unwrap());
    h.state_mut().composer.lead = "claude".into();
    h.run_steps(4);
    h.get_by_label("输入框").focus();
    h.get_by_label("输入框").type_text("先建立会话");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "initial reply", |app| root_state(app) == "done");
    let ticket = h.state().view.as_ref().unwrap().id;

    h.input_mut().dropped_files.push(egui::DroppedFile {
        path: Some(picture.clone()),
        ..Default::default()
    });
    h.run_steps(3);
    assert_eq!(h.state().composer.attachments, ["截图.PNG"]);
    h.get_by_label("输入框").focus();
    h.get_by_label("输入框").type_text("看看新图片");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "image follow-up", |app| {
        root_state(app) == "done"
            && kinds(app, ticket)
                .iter()
                .filter(|kind| *kind == "reply")
                .count()
                == 2
    });
    assert!(h.state().composer.attachments.is_empty());
    let row = h.state().host.as_ref().unwrap().engine().db().with(|q| {
        q.one("SELECT manifest FROM job_files WHERE job=(SELECT id FROM jobs WHERE ticket=? AND parent IS NULL)", &[&ticket])
    }).unwrap().unwrap();
    let manifest: Value = serde_json::from_str(row["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(manifest[0]["name"], "截图.PNG");
    std::fs::write(picture, "changed after delivery").unwrap();
    assert_eq!(
        std::fs::read(manifest[0]["object_path"].as_str().unwrap()).unwrap(),
        expected
    );

    h.run_steps(3);
    assert!(h.get_by_label("令牌用量").rect().top() >= h.get_by_label("输入框").rect().bottom());
    h.get_by_label("令牌用量").hover();
    h.run_steps(40);
    assert!(h.query_by_label("执行端已报告用量").is_some());
}

#[test]
fn create_receipt_preserves_attachments_added_after_send() {
    for change_project in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("first.txt"), "first").unwrap();
        let mut h = harness(data.path(), "readonly");
        h.state_mut().composer.project = Some(project.path().to_path_buf());
        h.run_steps(3);
        h.state_mut().submit(crate::host::Action::Create {
            goal: "initial request".into(),
            cwd: Some(project.path().to_path_buf()),
            lead: "claude".into(),
            profile: "readonly".into(),
            files: vec!["first.txt".into()],
            selections: None,
            extensions: None,
            allow_lead_routing: None,
        });
        if change_project {
            h.state_mut().new_conversation();
            h.state_mut().composer.project = Some(other.path().to_path_buf());
        }
        h.state_mut().composer.draft = "next request".into();
        h.state_mut().composer.attachments = vec!["next.png".into()];
        until(&mut h, "create receipt", |app| app.composer.pending == 0);
        assert_eq!(h.state().composer.attachments, ["next.png"]);
        assert_eq!(h.state().composer.draft, "next request");
        assert_eq!(h.state().view.is_none(), change_project);
    }
}

#[test]
fn model_picker_searches_complete_catalog_and_saves_declared_effort() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(3);
    h.state_mut().open_models(crate::models::Scope::Draft);
    let models: Vec<Value> = (0..2500).map(|n| serde_json::json!({
        "id":format!("model-{n:04}"), "reasoningEfforts":["low","high"], "defaultReasoningEffort":"low"
    })).collect();
    h.state_mut().models.as_mut().unwrap().answer(
        "deepseek",
        Ok(serde_json::json!({
            "default":{"mode":"cli","provider":"fixture","model":"model-0000"},
            "modes":[{"id":"cli"}], "providers":[{"id":"fixture","models":models}]
        })),
    );
    h.run_steps(3);
    assert!(h.query_by_label("共 2500 个模型，可在列表中搜索").is_some());
    h.get_by_role_and_label(Role::ComboBox, "模型").click();
    h.run_steps(3);
    h.get_by_label("搜索模型").focus();
    h.get_by_label("搜索模型").type_text("model-2499");
    h.run_steps(3);
    assert!(h.query_by_label("匹配 1 / 共 2500 个模型").is_some());
    h.get_by_label("model-2499").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::ComboBox, "思考强度").click();
    h.run_steps(3);
    h.get_by_label("high").click();
    h.run_steps(3);
    h.get_by_label("保存选择").click();
    h.run_steps(3);
    let route = &h.state().composer.routes.as_ref().unwrap()["deepseek"];
    assert_eq!(route["model"], "model-2499");
    assert_eq!(route["reasoning_effort"], "high");
    assert_eq!(h.state().composer.allow_lead_routing, Some(true));
}

#[test]
fn model_picker_opens_custom_providers_and_preserves_unsaved_choices() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(3);
    h.state_mut().open_models(crate::models::Scope::Draft);
    until(&mut h, "model catalog", |a| {
        a.models.as_ref().is_some_and(|d| !d.needs("deepseek"))
    });
    h.run_steps(3);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.run_steps(3);
    h.get_by_label("Codex").click();
    until(&mut h, "codex catalog", |a| {
        a.models.as_ref().is_some_and(|d| !d.needs("codex"))
    });
    h.run_steps(3);
    h.get_by_label("跟随本机适配器默认配置").click();
    h.get_by_label("交给主控选择协作者模型和思考强度").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "自定义供应商")
        .click();
    h.run_steps(3);
    assert!(
        h.state()
            .models
            .as_ref()
            .is_some_and(|d| d.scope == crate::models::Scope::Draft && !d.allow_lead_routing)
    );
    assert!(h.state().providers.is_some());
    h.get_by_label("添加供应商").click();
    h.run_steps(3);
    assert_eq!(
        h.state()
            .providers
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .agent,
        "codex"
    );
    for (label, text) in [
        ("名称", "完整目录测试"),
        ("接口地址", "http://127.0.0.1:34999/v1"),
    ] {
        h.get_by_role_and_label(Role::TextInput, label).focus();
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(text);
        h.run_steps(2);
    }
    // Exercise the form's complete probe-result entry point without a live endpoint.
    h.state_mut()
        .providers
        .as_mut()
        .unwrap()
        .probed(Ok((0..2500).map(|n| format!("remote-{n:04}")).collect()));
    h.run_steps(3);
    assert!(h.query_by_label("2500 个模型 · 每行一个模型 ID").is_some());
    h.get_by_label("保存供应商").click();
    until(&mut h, "custom provider saved", |a| {
        a.providers.as_ref().is_some_and(|d| d.form.is_none())
            && a.host.as_ref().unwrap().providers().len() == 1
    });
    let saved = h.state().host.as_ref().unwrap().providers().remove(0);
    assert_eq!(saved.agent, "codex");
    assert_eq!(saved.models.len(), 2500);
    assert_eq!(saved.models.last().unwrap(), "remote-2499");
    h.run_steps(3);
    h.get_by_label("返回模型选择").click();
    until(&mut h, "returned model catalog", |a| {
        a.providers.is_none() && a.models.as_ref().is_some_and(|d| !d.needs("codex"))
    });
    h.run_steps(3);
    h.get_by_label("保存选择").click();
    h.run_steps(3);
    let routes = h.state().composer.routes.as_ref().unwrap();
    assert_eq!(routes["deepseek"]["model"], "default");
    assert_eq!(routes["codex"]["model"], "default");
    assert_eq!(h.state().composer.allow_lead_routing, Some(false));
}

#[test]
fn custom_provider_entry_survives_catalog_failure_and_palette_search() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "readonly");
    h.run_steps(3);
    h.state_mut().open_models(crate::models::Scope::Draft);
    h.state_mut()
        .models
        .as_mut()
        .unwrap()
        .answer("deepseek", Err("目录暂时不可用".into()));
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "自定义供应商")
        .click();
    h.run_steps(3);
    assert!(h.state().providers.is_some());
    h.get_by_label("返回模型选择").click();
    h.run_steps(3);
    h.get_by_label("取消").click();
    h.run_steps(3);
    h.state_mut().palette = Some(crate::palette::PaletteState::default());
    h.state_mut().palette.as_mut().unwrap().query = "自定义".into();
    h.run_steps(3);
    h.get_by_label("自定义供应商与轮换").click();
    h.run_steps(3);
    assert!(h.state().providers.is_some());
}

#[test]
fn lead_routing_policy_roundtrips_defaults_draft_and_active_ticket() {
    const POLICY: &str = "交给主控选择协作者模型和思考强度";
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), "ask");
    h.run_steps(3);
    h.state_mut().open_models(crate::models::Scope::Defaults);
    h.run_steps(3);
    h.get_by_label(POLICY).click();
    h.run_steps(3);
    h.get_by_label("保存选择").click();
    until(&mut h, "default policy saved", |a| {
        a.models.is_none() && !a.host.as_ref().unwrap().allow_lead_routing_default()
    });
    drop(h);
    let mut h = harness(data.path(), "ask");
    h.state_mut().composer.project = Some(project.path().to_path_buf());
    h.run_steps(3);
    h.state_mut().open_models(crate::models::Scope::Draft);
    h.run_steps(3);
    assert!(!h.state().models.as_ref().unwrap().allow_lead_routing);
    h.get_by_label(POLICY).click();
    h.run_steps(3);
    h.get_by_label("保存选择").click();
    h.run_steps(3);
    assert_eq!(h.state().composer.allow_lead_routing, Some(true));
    assert!(
        !h.state()
            .host
            .as_ref()
            .unwrap()
            .allow_lead_routing_default()
    );
    h.get_by_label("输入框").focus();
    h.get_by_label("输入框")
        .type_text("保持审批等待以测试实时策略");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "active approval", |a| {
        a.view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| !s.approvals.is_empty())
    });
    let ticket = h.state().view.as_ref().unwrap().id;
    assert!(
        h.state()
            .view
            .as_ref()
            .unwrap()
            .snap
            .as_ref()
            .unwrap()
            .ticket
            .allow_lead_routing
    );
    let routes_before = ticket_routes(h.state(), ticket);
    h.state_mut()
        .open_models(crate::models::Scope::Ticket(ticket));
    h.run_steps(3);
    assert!(h.state().models.as_ref().unwrap().allow_lead_routing);
    h.get_by_label(POLICY).click();
    h.run_steps(3);
    h.get_by_label("保存选择").click();
    until(&mut h, "live policy saved", |a| {
        a.models.is_none()
            && a.view
                .as_ref()
                .and_then(|v| v.snap.as_ref())
                .is_some_and(|s| !s.ticket.allow_lead_routing)
    });
    assert_eq!(ticket_routes(h.state(), ticket), routes_before);
    assert!(
        kinds(h.state(), ticket)
            .iter()
            .any(|kind| kind == "lead_routing.changed")
    );
    h.state_mut()
        .open_models(crate::models::Scope::Ticket(ticket));
    h.run_steps(3);
    assert!(!h.state().models.as_ref().unwrap().allow_lead_routing);
}
