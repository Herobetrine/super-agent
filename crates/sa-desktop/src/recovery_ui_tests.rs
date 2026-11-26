//! Journal recovery through the desktop startup branches.

use crate::{
    app::App,
    cli::Options,
    host::{Host, tests::fakes},
    recovery_ui::State,
};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sa_store::{
    DB_FILE,
    journal::{RestoreError, restore_from_snapshot_at},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const SNAPSHOT: &str = "hub-00000000000000012345.sqlite3";
const OFFER: &str = "用升级前的快照恢复";
const MANUAL: &str = "可以这样手动恢复：1. 关闭所有 Super Agent 窗口。2. 把数据文件夹里的 hub.sqlite3 以及存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起移到别处保存。3. 把快照复制到数据文件夹并改名为 hub.sqlite3。4. 删除 backups 文件夹里的 migration.json，然后重新打开 Super Agent。";
const SALVAGE: &str = "也可以退出所有 Super Agent 窗口，把 hub.sqlite3 和同一目录下存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起复制到别处保存，再对副本用 sqlite3 的 .recover 命令尝试抢救。";
const EMPTY: &str = "如果不需要旧数据，可以退出后把这些文件一起移走或改名，再次打开 Super Agent 会新建空的工单数据库。";

fn sqlite_fixture(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE fixture (id INTEGER); INSERT INTO fixture VALUES (42)")
        .unwrap();
}

fn interrupted(dir: &Path, with_snapshot: bool) -> PathBuf {
    let backups = dir.join("backups");
    fs::create_dir(&backups).unwrap();
    sqlite_fixture(&dir.join(DB_FILE));
    let snapshot = backups.join(SNAPSHOT);
    if with_snapshot {
        sqlite_fixture(&snapshot);
    }
    let marker = serde_json::json!({
        "from": 1, "to": 2, "snapshot": SNAPSHOT,
        "started_at": { "secs_since_epoch": 12, "nanos_since_epoch": 0 },
    });
    fs::write(backups.join("migration.json"), marker.to_string()).unwrap();
    snapshot
}

fn harness(dir: &Path, observe: bool) -> Harness<'static, App> {
    let dir = dir.to_path_buf();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_eframe(move |cc| {
            let opener = crate::data::StoreSource::opener(dir.clone());
            crate::create_app(
                cc,
                Options {
                    data: Some(dir.clone()),
                    observe,
                    ..Default::default()
                },
                dir,
                opener,
                Arc::new(Mutex::new(None)),
                |dir, ctx| {
                    assert!(!observe, "observer must not attempt to start a host");
                    Host::start_with(dir, fakes, move || ctx.request_repaint())
                },
            )
        })
}

fn until(h: &mut Harness<'_, App>, what: &str, cond: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn click(h: &mut Harness<'_, App>, label: &str) {
    queue_button_click(h, label);
    h.run_steps(2);
}

fn queue_button_click(h: &mut Harness<'_, App>, label: &str) {
    // The centered modal remeasures when its contents change. The harness
    // queues its pointer events at the rectangle measured before the click.
    let mut previous = None;
    let mut stable = 0;
    for _ in 0..12 {
        let button = h.get_by_role_and_label(Role::Button, label);
        let rect = button.rect();
        if previous == Some(rect) {
            stable += 1;
            if stable == 2 {
                let viewport =
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 900.0));
                assert!(
                    viewport.contains_rect(rect),
                    "{label} is outside the viewport: {rect:?}"
                );
                button.click();
                return;
            }
        } else {
            stable = 0;
        }
        previous = Some(rect);
        h.step();
    }
    panic!("button {label} did not settle within 12 frames");
}

fn has_text(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

fn assert_buttons(h: &Harness<'_, App>, acknowledge: bool) {
    assert!(
        !h.get_by_role_and_label(Role::Button, "打开数据文件夹")
            .accesskit_node()
            .is_disabled()
    );
    let known = h.query_by_role_and_label(Role::Button, "知道了");
    assert_eq!(known.is_some(), acknowledge);
    if let Some(button) = known {
        assert!(!button.accesskit_node().is_disabled());
    }
    for removed in ["从快照恢复", "重试清理", "立即重启", "暂不恢复"] {
        assert!(
            h.query_by_role_and_label(Role::Button, removed).is_none(),
            "unexpected {removed}"
        );
    }
}

#[test]
fn interrupted_migration_offers_restore_and_folded_manual_steps() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    let live = data.path().join(DB_FILE);
    let marker = data.path().join("backups").join("migration.json");
    let before = [&live, &snapshot, &marker].map(|path| fs::read(path).unwrap());
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    assert!(h.state().host.is_none());
    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(
        &h,
        &format!("升级前的快照在：{}（保存于 ", snapshot.display())
    ));
    assert!(has_text(&h, "）。Super Agent 已停止调度，没有继续升级。"));
    assert!(has_text(&h, OFFER));
    assert!(!has_text(&h, MANUAL));
    h.get_by_label("手动恢复步骤").click();
    h.run_steps(4);
    assert!(has_text(&h, MANUAL));
    assert_buttons(&h, true);
    click(&mut h, "知道了");
    assert!(!has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(&h, "只读观察模式："));
    assert_eq!(
        [&live, &snapshot, &marker].map(|path| fs::read(path).unwrap()),
        before
    );
}

#[test]
fn interrupted_migration_without_snapshot_shows_missing_copy() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), false);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(
        &h,
        &format!(
            "找不到升级前的快照（{}）。Super Agent 已停止调度，没有继续升级。可以打开数据文件夹手动处理。",
            snapshot.display()
        )
    ));
    assert!(!has_text(&h, MANUAL));
    assert!(!has_text(&h, OFFER));
    assert!(!has_text(&h, "手动恢复步骤"));
    assert_buttons(&h, true);
}

#[test]
fn corrupt_code_26_without_snapshot_shows_salvage_and_empty_journal_copy() {
    let data = tempfile::tempdir().unwrap();
    let live = data.path().join(DB_FILE);
    let bytes = [0xcd; 4096];
    fs::write(&live, bytes).unwrap();
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    assert!(has_text(&h, "工单数据库已损坏"));
    assert!(has_text(
        &h,
        &format!(
            "无法读取 {}（SQLite 错误 26：不是有效的数据库文件）。Super Agent 已停止调度，尚未执行任何恢复。",
            live.display()
        )
    ));
    assert!(!has_text(&h, "有可用的快照："));
    assert!(has_text(&h, SALVAGE));
    assert!(has_text(&h, EMPTY));
    assert_buttons(&h, true);
    assert_eq!(fs::read(&live).unwrap(), bytes);
}

#[test]
fn corrupt_code_11_with_snapshot_offers_restore_with_short_local_time() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    fs::remove_file(data.path().join("backups").join("migration.json")).unwrap();
    let live = data.path().join(DB_FILE);
    let mut bytes = fs::read(&live).unwrap();
    bytes[28..32].copy_from_slice(&200_u32.to_be_bytes());
    fs::write(&live, &bytes).unwrap();
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    assert!(has_text(&h, "工单数据库已损坏"));
    assert!(has_text(
        &h,
        "（SQLite 错误 11：文件内容损坏）。Super Agent 已停止调度，尚未执行任何恢复。"
    ));
    assert!(has_text(
        &h,
        &format!("有可用的快照：{}（保存于 ", snapshot.display())
    ));
    let time = chrono::DateTime::<chrono::Local>::from(
        fs::metadata(&snapshot).unwrap().modified().unwrap(),
    );
    let label = format!("用最近快照恢复（{}）", time.format("%-m/%-d %H:%M"));
    assert!(h.query_by_role_and_label(Role::Button, &label).is_some());
    assert!(!has_text(&h, SALVAGE));
    h.get_by_label("手动恢复步骤").click();
    h.run_steps(4);
    assert!(has_text(
        &h,
        "可以按上面的步骤把它恢复为 hub.sqlite3（先关闭所有窗口并移走当前文件及其 -wal、-shm、-journal 文件）。"
    ));
    assert!(has_text(&h, SALVAGE));
    assert!(has_text(&h, EMPTY));
    assert_buttons(&h, true);
    assert_eq!(fs::read(&live).unwrap(), bytes);
}

#[test]
fn malformed_migration_marker_offers_only_the_folder() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let marker = data.path().join("backups").join("migration.json");
    fs::write(&marker, b"invalid JSON").unwrap();
    let Some(sa_store::journal::JournalError::InvalidMigrationMarker { reason, .. }) =
        sa_store::journal::inspect(data.path())
    else {
        panic!("fixture should be invalid");
    };
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    assert!(has_text(&h, "升级标记无法识别"));
    assert!(has_text(
        &h,
        &format!("{}：{reason}。为安全起见，没有继续升级。", marker.display())
    ));
    assert_buttons(&h, false);
    assert!(!has_text(&h, OFFER));
    assert!(!has_text(&h, "手动恢复步骤"));
}

#[test]
fn observer_inspects_without_starting_host_and_shows_same_manual_copy() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), true);
    until(&mut h, "observer inspection", |app| app.recovery.is_some());
    assert!(h.state().host.is_none());
    assert!(!data.path().join(sa_engine::owner::LOCK_FILE).exists());
    assert!(!data.path().join(".journal-migration.lock").exists());
    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(&h, MANUAL));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert!(!has_text(&h, OFFER));
    assert!(!has_text(&h, "手动恢复步骤"));
    assert_buttons(&h, true);
}

fn recovery_state(app: &App) -> &State {
    &app.recovery.as_ref().unwrap().state
}

#[test]
fn confirm_button_is_centered_on_the_first_rendered_frame() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);

    h.get_by_role_and_label(Role::Button, OFFER).click();
    h.step();
    assert!(matches!(recovery_state(h.state()), State::Confirm { .. }));

    // The click can finish on the offer's last frame. Take the very next
    // frame in that case, without using the coordinate-settling helper.
    if h.query_by_role_and_label(Role::Button, "确认恢复")
        .is_none()
    {
        h.step();
    }
    let first = h.get_by_role_and_label(Role::Button, "确认恢复").rect();
    h.run_steps(2);
    let later = h.get_by_role_and_label(Role::Button, "确认恢复").rect();
    assert_eq!(first, later, "confirmation moved after its first render");
}

#[test]
fn confirming_names_the_backup_and_cancel_keeps_the_originals() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    h.state_mut().recovery.as_mut().unwrap().restore =
        Arc::new(|_, _, _| panic!("restore ran before confirmation"));
    click(&mut h, OFFER);
    let State::Confirm { folder_stamp, .. } = recovery_state(h.state()) else {
        panic!("expected confirmation");
    };
    let time = chrono::DateTime::<chrono::Local>::from(
        fs::metadata(snapshot).unwrap().modified().unwrap(),
    );
    assert!(has_text(&h, "确认从快照恢复"));
    assert!(has_text(
        &h,
        &format!(r"backups\broken-{folder_stamp}\ 保存，不会删除；")
    ));
    assert!(has_text(
        &h,
        &format!(
            "把 {} 的快照复制为新的 hub.sqlite3。",
            time.format("%Y/%-m/%-d %H:%M:%S")
        )
    ));
    assert!(has_text(
        &h,
        "快照之后产生的工单记录不会出现在恢复后的数据库里"
    ));
    assert!(
        h.query_by_role_and_label(Role::Button, "确认恢复")
            .is_some()
    );
    click(&mut h, "取消");
    assert!(matches!(recovery_state(h.state()), State::Ready));
    assert!(has_text(&h, OFFER));
    assert!(data.path().join("backups/migration.json").is_file());
}

fn assert_protected_from_close(h: &mut Harness<'_, App>) {
    assert!(
        h.query_by_role_and_label(Role::Button, "关闭对话框")
            .is_none()
    );
    h.key_press(egui::Key::Escape);
    h.step();
    let pos = egui::pos2(2.0, 2.0);
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    assert!(
        h.output().viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose)
    );
    h.run_steps(2);
}

#[test]
fn restore_runs_off_thread_preserves_files_and_success_relaunches() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    let (release, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    let ui_thread = std::thread::current().id();
    let panel = h.state_mut().recovery.as_mut().unwrap();
    panel.restore = Arc::new(move |dir, plan, time| {
        assert_ne!(std::thread::current().id(), ui_thread);
        wait.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        restore_from_snapshot_at(dir, plan, time)
    });
    let (launched, launch) = mpsc::channel();
    panel.relaunch = Arc::new(move || {
        launched.send(()).unwrap();
        Ok(())
    });
    click(&mut h, OFFER);
    let State::Confirm { folder_stamp, .. } = recovery_state(h.state()) else {
        panic!("expected confirmation");
    };
    let backup = data
        .path()
        .join("backups")
        .join(format!("broken-{folder_stamp}"));
    click(&mut h, "确认恢复");
    assert!(has_text(&h, "正在恢复…"));
    assert!(
        h.query_all_by_role(Role::ProgressIndicator)
            .next()
            .is_some()
    );
    assert_protected_from_close(&mut h);
    assert!(matches!(recovery_state(h.state()), State::Running(_)));
    release.send(()).unwrap();
    until(&mut h, "restore succeeds", |app| {
        matches!(recovery_state(app), State::Succeeded(_))
    });
    assert!(has_text(&h, "已恢复到 "));
    assert!(has_text(&h, &backup.display().to_string()));
    assert!(
        h.query_by_role_and_label(Role::Button, "打开备份文件夹")
            .is_some()
    );
    assert!(backup.join(DB_FILE).is_file());
    assert!(backup.join("migration.json").is_file());
    assert!(backup.join("restore.json").is_file());
    assert_protected_from_close(&mut h);
    assert!(matches!(recovery_state(h.state()), State::Succeeded(_)));
    queue_button_click(&mut h, "重新打开");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        h.step();
        if h.output().viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "relaunch never requested window close"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    launch.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn failed_relaunch_offers_manual_window_close() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    h.state_mut().recovery.as_mut().unwrap().relaunch = Arc::new(|| anyhow::bail!("无法启动进程"));
    click(&mut h, OFFER);
    click(&mut h, "确认恢复");
    until(&mut h, "restore succeeds", |app| {
        matches!(recovery_state(app), State::Succeeded(_))
    });
    click(&mut h, "重新打开");
    until(&mut h, "launch failure", |app| {
        matches!(recovery_state(app), State::RelaunchFailed(_))
    });
    assert!(has_text(
        &h,
        "无法自动重新打开：无法启动进程。请手动关闭并重新打开 Super Agent。"
    ));
    assert_buttons(&h, false);
    assert!(
        h.query_by_role_and_label(Role::Button, "关闭窗口")
            .is_some()
    );
}

#[test]
fn an_untouched_failure_can_retry_through_confirmation() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    fs::write(snapshot, b"not a database").unwrap();
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    click(&mut h, OFFER);
    click(&mut h, "确认恢复");
    until(&mut h, "untouched failure", |app| {
        matches!(recovery_state(app), State::Untouched(_))
    });
    assert!(has_text(&h, "恢复没有完成："));
    assert!(has_text(&h, "。没有改动任何文件"));
    assert_buttons(&h, true);
    click(&mut h, "重试");
    assert!(has_text(&h, "确认从快照恢复"));
}

#[test]
fn a_moved_failure_displays_the_saved_path_and_cannot_retry() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let backup = data.path().join("backups/broken-20261126-015200");
    let saved = backup.clone();
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    h.state_mut().recovery.as_mut().unwrap().restore = Arc::new(move |_, _, _| {
        Err(RestoreError::Moved {
            reason: "无法移回".into(),
            broken_dir: saved.clone(),
        })
    });
    click(&mut h, OFFER);
    click(&mut h, "确认恢复");
    until(&mut h, "moved failure", |app| {
        matches!(recovery_state(app), State::Moved { .. })
    });
    assert!(has_text(
        &h,
        &format!(
            "恢复没有完成：无法移回。原来的文件保存在：{}",
            backup.display()
        )
    ));
    assert_buttons(&h, true);
    assert!(h.query_by_role_and_label(Role::Button, "重试").is_none());
}

#[test]
fn busy_restore_waits_for_explicit_recheck() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    let owner = sa_engine::owner::claim(data.path()).unwrap();
    click(&mut h, OFFER);
    click(&mut h, "确认恢复");
    until(&mut h, "busy", |app| {
        matches!(recovery_state(app), State::Busy)
    });
    assert!(has_text(
        &h,
        "检测到其他 Super Agent 窗口正在运行，请先全部关闭。"
    ));
    assert!(
        h.get_by_role_and_label(Role::Button, OFFER)
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, "重新检查");
    assert!(matches!(recovery_state(h.state()), State::Busy));
    drop(owner);
    h.run_steps(4);
    assert!(matches!(recovery_state(h.state()), State::Busy));
    click(&mut h, "重新检查");
    assert!(matches!(recovery_state(h.state()), State::Ready));
    assert!(
        !h.get_by_role_and_label(Role::Button, OFFER)
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn losing_the_owner_lock_also_inspects_the_journal() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let _owner = sa_engine::owner::claim(data.path()).unwrap();
    let mut h = harness(data.path(), false);
    until(&mut h, "owner-lock observer inspection", |app| {
        app.recovery.is_some()
    });
    assert!(h.state().host.is_none());
    assert!(has_text(&h, MANUAL));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert_buttons(&h, true);
}

#[test]
fn losing_the_owner_lock_for_a_healthy_journal_has_no_panel() {
    let data = tempfile::tempdir().unwrap();
    let _owner = Host::start_with(data.path().to_path_buf(), fakes, || {}).unwrap();
    let mut h = harness(data.path(), false);
    until(&mut h, "healthy observer inspection", |app| {
        app.recovery_probe.is_none()
    });
    assert!(h.state().host_error.is_some());
    assert!(h.state().recovery.is_none());
    assert!(has_text(&h, "只读观察模式："));
}

#[test]
fn open_folder_runs_off_thread_and_displays_launch_errors() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    h.run_steps(4);
    let opened = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&opened);
    let ui_thread = std::thread::current().id();
    h.state_mut().recovery.as_mut().unwrap().open_folder = Arc::new(move |dir| {
        let background = std::thread::current().id() != ui_thread;
        *observed.lock().unwrap() = Some((dir.to_path_buf(), background));
        anyhow::bail!("没有文件管理器");
    });
    click(&mut h, "打开数据文件夹");
    until(&mut h, "open folder failure", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });
    assert_eq!(
        *opened.lock().unwrap(),
        Some((data.path().to_path_buf(), true))
    );
    assert!(has_text(&h, "无法打开数据文件夹：没有文件管理器"));
    assert_buttons(&h, true);
}
