//! Real App clicks with fake adapters and disposable settings storage.

use crate::{
    app::App,
    cli::Options,
    host::{Host, tests::fakes},
    worker,
};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sa_store::efficiency::{Settings, Ttl};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn harness(data: &Path, observe: bool, demo: bool) -> Harness<'static, App> {
    let dir = data.to_path_buf();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = if observe || demo {
                Err("只读预览".into())
            } else {
                Host::start_with(dir.clone(), fakes, move || ctx.request_repaint())
                    .map_err(|e| format!("{e:#}"))
            };
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            App::new(
                cc,
                Options {
                    data: Some(dir),
                    observe,
                    demo,
                    ..Default::default()
                },
                worker,
                Arc::new(Mutex::new(None)),
                host,
            )
        })
}

fn until(h: &mut Harness<'_, App>, cond: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "settings action timed out, message={:?}",
        h.state().composer.message
    );
}

fn open(h: &mut Harness<'_, App>) {
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "Token 与缓存")
        .click();
    h.run_steps(3);
    assert!(h.state().efficiency.is_some());
}

#[test]
fn efficiency_clicks_cancel_save_restart_and_restore_defaults() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    h.get_by_role_and_label(Role::CheckBox, "增量上下文")
        .click();
    h.get_by_role_and_label(Role::CheckBox, "精简协作提示")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "1 小时").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "取消").click();
    h.run_steps(3);
    assert!(h.state().efficiency.is_none());
    assert_eq!(
        h.state().host.as_ref().unwrap().efficiency_settings(),
        Settings::default()
    );

    open(&mut h);
    assert_eq!(
        h.state().efficiency.as_ref().unwrap().draft,
        Settings::default()
    );
    h.get_by_role_and_label(Role::CheckBox, "增量上下文")
        .click();
    h.get_by_role_and_label(Role::CheckBox, "精简协作提示")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "5 分钟").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "保存").click();
    until(&mut h, |app| app.efficiency.is_none());
    let saved = Settings {
        incremental_context: false,
        concise_handoffs: false,
        claude_cache_ttl: Ttl::FiveMinutes,
    };
    assert_eq!(
        h.state().host.as_ref().unwrap().efficiency_settings(),
        saved
    );
    drop(h);

    let mut h = harness(data.path(), false, false);
    open(&mut h);
    assert_eq!(h.state().efficiency.as_ref().unwrap().draft, saved);
    h.get_by_role_and_label(Role::Button, "恢复默认").click();
    h.run_steps(3);
    assert_eq!(
        h.state().efficiency.as_ref().unwrap().draft,
        Settings::default()
    );
    assert_eq!(
        h.state().host.as_ref().unwrap().efficiency_settings(),
        saved
    );
    h.get_by_role_and_label(Role::Button, "保存").click();
    until(&mut h, |app| app.efficiency.is_none());
    assert_eq!(
        h.state().host.as_ref().unwrap().efficiency_settings(),
        Settings::default()
    );
}

#[test]
fn efficiency_failed_save_keeps_draft_and_can_retry() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    h.state().host.as_ref().unwrap().engine().db().with(|q| {
        q.run("CREATE TRIGGER reject_efficiency BEFORE INSERT ON studio_efficiency BEGIN SELECT RAISE(ABORT,'fixture settings write failure'); END", &[])
    }).unwrap();
    open(&mut h);
    h.get_by_role_and_label(Role::RadioButton, "1 小时").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "保存").click();
    until(&mut h, |app| {
        app.efficiency
            .as_ref()
            .is_some_and(|d| !d.saving && d.error.is_some())
    });
    assert_eq!(
        h.state()
            .efficiency
            .as_ref()
            .unwrap()
            .draft
            .claude_cache_ttl,
        Ttl::OneHour
    );
    assert_eq!(
        h.state().host.as_ref().unwrap().efficiency_settings(),
        Settings::default()
    );
    h.state()
        .host
        .as_ref()
        .unwrap()
        .engine()
        .db()
        .with(|q| q.run("DROP TRIGGER reject_efficiency", &[]))
        .unwrap();
    h.get_by_role_and_label(Role::Button, "保存").click();
    until(&mut h, |app| app.efficiency.is_none());
    assert_eq!(
        h.state()
            .host
            .as_ref()
            .unwrap()
            .efficiency_settings()
            .claude_cache_ttl,
        Ttl::OneHour
    );
}

#[test]
fn efficiency_observer_reads_saved_values_and_demo_can_preview() {
    let data = tempfile::tempdir().unwrap();
    let saved = Settings {
        incremental_context: false,
        concise_handoffs: true,
        claude_cache_ttl: Ttl::OneHour,
    };
    let owner = Host::start_with(data.path().into(), fakes, || {}).unwrap();
    owner
        .engine()
        .db()
        .with(|q| sa_store::efficiency::save(q.conn(), &saved))
        .unwrap();
    let mut h = harness(data.path(), true, false);
    open(&mut h);
    until(&mut h, |app| {
        app.efficiency.as_ref().is_some_and(|d| d.draft == saved)
    });
    assert!(
        h.get_by_role_and_label(Role::Button, "保存")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.get_by_role_and_label(Role::CheckBox, "增量上下文")
            .accesskit_node()
            .is_disabled()
    );
    drop(h);

    let mut h = harness(data.path(), false, true);
    open(&mut h);
    h.get_by_role_and_label(Role::CheckBox, "增量上下文")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "1 小时").click();
    h.run_steps(3);
    assert!(
        !h.state()
            .efficiency
            .as_ref()
            .unwrap()
            .draft
            .incremental_context
    );
    assert_eq!(
        h.state()
            .efficiency
            .as_ref()
            .unwrap()
            .draft
            .claude_cache_ttl,
        Ttl::OneHour
    );
    assert!(
        h.get_by_role_and_label(Role::Button, "保存")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn efficiency_dialog_flag_is_supported() {
    let opts = crate::cli::parse(["--demo", "--dialog", "efficiency"].map(str::to_string)).unwrap();
    assert_eq!(opts.dialog.as_deref(), Some("efficiency"));
}
