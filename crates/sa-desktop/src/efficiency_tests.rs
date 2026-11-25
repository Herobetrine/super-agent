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
        .with_size(egui::vec2(1280.0, 1100.0))
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
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            if h.state().efficiency.is_some() {
                settle_dialog(h);
            }
            return;
        }
        if Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "settings action timed out, message={:?}",
        h.state().composer.message
    );
}

fn settle_dialog(h: &mut Harness<'_, App>) {
    // Modal centering follows the measured ScrollArea size over several frames.
    // The taller fixture keeps both the existing controls and usage controls
    // visible together; the smaller-window test below exercises scrolling.
    // Click only after its actual visible targets stop moving.
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 1100.0));
    let mut previous = Vec::new();
    let mut stable_frames = 0;
    for frame in 0..12 {
        let rects = [
            (Role::CheckBox, "增量上下文"),
            (Role::CheckBox, "按需读取 MCP 参数"),
            (Role::CheckBox, "精简协作提示"),
            (Role::RadioButton, "1 小时"),
            (Role::CheckBox, "长等待缓存保温"),
            (Role::SpinButton, "协作者并发上限"),
            (Role::CheckBox, "额度保护"),
            (Role::Button, "恢复默认"),
            (Role::Button, "保存"),
        ]
        .map(|(role, label)| h.get_by_role_and_label(role, label).rect())
        .to_vec();
        assert!(rects.iter().all(|rect| viewport.contains_rect(*rect)), "dialog targets outside viewport: {rects:?}");
        eprintln!("efficiency dialog geometry frame={frame} targets={rects:?}");
        if rects == previous {
            stable_frames += 1;
            if stable_frames == 2 {
                return;
            }
        } else {
            stable_frames = 0;
        }
        previous = rects;
        h.run_steps(1);
    }
    panic!("efficiency dialog targets did not settle within 12 frames");
}

fn set_cap(h: &mut Harness<'_, App>, cap: u32) {
    h.run_steps(3);
    let target = h.get_by_role_and_label(Role::SpinButton, "协作者并发上限")
        .accesskit_node().id();
    h.event(egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
        target,
        action: egui::accesskit::Action::SetValue,
        data: Some(egui::accesskit::ActionData::NumericValue(cap as f64)),
    }));
    h.run_steps(3);
    assert_eq!(h.state().efficiency.as_ref().unwrap().draft.max_collaborators, cap);
}

fn open(h: &mut Harness<'_, App>) {
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "Token 与缓存")
        .click();
    h.run_steps(3);
    assert!(h.state().efficiency.is_some());
    settle_dialog(h);
}

#[test]
fn efficiency_clicks_cancel_save_restart_and_restore_defaults() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    assert!(h.state().efficiency.as_ref().unwrap().draft.lazy_mcp_schemas);
    h.get_by_role_and_label(Role::CheckBox, "增量上下文")
        .click();
    h.get_by_role_and_label(Role::CheckBox, "精简协作提示")
        .click();
    h.get_by_role_and_label(Role::CheckBox, "按需读取 MCP 参数")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "1 小时").click();
    set_cap(&mut h, 0);
    assert_eq!(h.get_by_role_and_label(Role::SpinButton, "协作者并发上限").value().as_deref(), Some("不限制"));
    h.get_by_role_and_label(Role::CheckBox, "额度保护").click();
    h.run_steps(3);
    assert_eq!(h.state().efficiency.as_ref().unwrap().draft, Settings {
        incremental_context: false,
        lazy_mcp_schemas: false,
        concise_handoffs: false,
        claude_cache_ttl: Ttl::OneHour,
        claude_keepalive: true,
        max_collaborators: 0,
        quota_guard: false,
    });
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
    h.get_by_role_and_label(Role::CheckBox, "按需读取 MCP 参数")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "5 分钟").click();
    set_cap(&mut h, 2);
    h.get_by_role_and_label(Role::CheckBox, "额度保护").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "保存").click();
    until(&mut h, |app| app.efficiency.is_none());
    let saved = Settings {
        incremental_context: false,
        lazy_mcp_schemas: false,
        concise_handoffs: false,
        claude_cache_ttl: Ttl::FiveMinutes,
        claude_keepalive: true,
        max_collaborators: 2,
        quota_guard: false,
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
    set_cap(&mut h, 6);
    h.get_by_role_and_label(Role::CheckBox, "额度保护").click();
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
    assert_eq!(h.state().efficiency.as_ref().unwrap().draft.max_collaborators, 6);
    assert!(!h.state().efficiency.as_ref().unwrap().draft.quota_guard);
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
    let saved = h.state().host.as_ref().unwrap().efficiency_settings();
    assert_eq!(saved.max_collaborators, 6);
    assert!(!saved.quota_guard);
}

#[test]
fn efficiency_observer_reads_saved_values_and_demo_can_preview() {
    let data = tempfile::tempdir().unwrap();
    let saved = Settings {
        incremental_context: false,
        lazy_mcp_schemas: false,
        concise_handoffs: true,
        claude_cache_ttl: Ttl::OneHour,
        claude_keepalive: true,
        max_collaborators: 2,
        quota_guard: false,
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
    assert!(h.get_by_role_and_label(Role::SpinButton, "协作者并发上限").accesskit_node().is_disabled());
    assert!(h.get_by_role_and_label(Role::CheckBox, "额度保护").accesskit_node().is_disabled());
    let end = Instant::now() + Duration::from_secs(60);
    while h.state_mut().saved_efficiency().as_ref() != Some(&saved) {
        assert!(Instant::now() < end, "observer settings did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.state_mut().efficiency.as_mut().unwrap().draft.max_collaborators = 12;
    assert_eq!(h.state_mut().saved_efficiency(), Some(saved.clone()));
    drop(h);

    let mut h = harness(data.path(), false, true);
    open(&mut h);
    h.get_by_role_and_label(Role::CheckBox, "增量上下文")
        .click();
    h.get_by_role_and_label(Role::RadioButton, "1 小时").click();
    set_cap(&mut h, 0);
    h.get_by_role_and_label(Role::CheckBox, "额度保护").click();
    h.run_steps(3);
    assert!(!h.state().efficiency.as_ref().unwrap().draft.quota_guard);
    assert_eq!(h.state_mut().saved_efficiency(), Some(Settings::default()));
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

#[test]
fn efficiency_controls_remain_accessible_in_a_smaller_window() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, true);
    open(&mut h);
    h.set_size(egui::vec2(1280.0, 900.0));
    h.run_steps(3);
    h.get_by_role_and_label(Role::CheckBox, "额度保护").scroll_to_me();
    h.run_steps(3);
    h.get_by_role_and_label(Role::CheckBox, "额度保护").click();
    h.run_steps(3);
    assert!(!h.state().efficiency.as_ref().unwrap().draft.quota_guard);
    h.get_by_role_and_label(Role::SpinButton, "协作者并发上限").scroll_to_me();
    h.run_steps(3);
    set_cap(&mut h, 16);
    h.get_by_role_and_label(Role::Button, "取消").click();
    h.run_steps(3);
    assert!(h.state().efficiency.is_none());
}

#[test]
fn observing_a_saved_larger_cap_does_not_modify_the_draft() {
    let data = tempfile::tempdir().unwrap();
    let owner = Host::start_with(data.path().into(), fakes, || {}).unwrap();
    let saved = Settings { max_collaborators: 20, ..Default::default() };
    owner.engine().db().with(|q| sa_store::efficiency::save(q.conn(), &saved)).unwrap();
    let mut h = harness(data.path(), true, false);
    open(&mut h);
    until(&mut h, |app| app.efficiency.as_ref().is_some_and(|d| d.draft == saved));
    h.run_steps(3);
    assert_eq!(h.state().efficiency.as_ref().unwrap().draft, saved);
    h.get_by_role_and_label(Role::Button, "取消").click();
    h.run_steps(3);
    assert_eq!(owner.engine().db().with(|q| sa_store::efficiency::load(q.conn())).unwrap(), saved);
}
