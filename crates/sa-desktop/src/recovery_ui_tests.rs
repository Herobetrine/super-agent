//! The recovery panel of an interrupted migration and a corrupt journal, driven
//! through the real App with fake adapters and disposable data directories.

use crate::{
    app::App,
    cli::Options,
    host::{Host, tests::fakes},
    recovery_ui::{Hooks, Phase},
    worker,
};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sa_store::journal::JournalError;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

const SNAPSHOT: &str = "hub-00000000000000012345.sqlite3";

/// A standalone SQLite file holding one table with one row.
fn sqlite_with(path: &Path, table: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE {table}(v TEXT); INSERT INTO {table} VALUES('kept');"
    ))
    .unwrap();
    conn.close().map_err(|(_, e)| e).unwrap();
}

fn has_table(path: &Path, table: &str) -> bool {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name=?",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// A data dir with a live `hub.sqlite3` (table `current`), a snapshot (table
/// `snap`) and the marker of an unfinished migration pointing at it.
fn interrupted(dir: &Path, with_snapshot: bool) -> PathBuf {
    std::fs::create_dir_all(dir.join("backups")).unwrap();
    sqlite_with(&dir.join(sa_store::DB_FILE), "current");
    let snapshot = dir.join("backups").join(SNAPSHOT);
    if with_snapshot {
        sqlite_with(&snapshot, "snap");
    }
    let marker = serde_json::json!({
        "from": 1,
        "to": 2,
        "snapshot": snapshot,
        "started_at": { "secs_since_epoch": 12, "nanos_since_epoch": 0 },
    });
    std::fs::write(dir.join("backups").join("migration.json"), marker.to_string()).unwrap();
    snapshot
}

/// File names with their lengths; enough to prove nothing was moved or rewritten.
fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push((
                    path.strip_prefix(dir).unwrap().to_string_lossy().into_owned(),
                    std::fs::metadata(&path).unwrap().len(),
                ));
            }
        }
    }
    files.retain(|(name, _)| name != "desktop.lock");
    files.sort();
    files
}

fn damaged(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("hub-damaged-"))
        })
        .collect()
}

/// The panel is built from whatever `Host::start_with` returns, as in `main`.
fn harness(dir: &Path, observe: bool) -> Harness<'static, App> {
    let dir = dir.to_path_buf();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let mut start_error = None;
            let host = Host::start_with(dir.clone(), fakes, move || ctx.request_repaint())
                .map_err(|e| {
                    let message = format!("{e:#}");
                    start_error = Some(e);
                    message
                });
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            let mut app = App::new(
                cc,
                Options {
                    data: Some(dir),
                    observe,
                    ..Default::default()
                },
                worker,
                Arc::new(Mutex::new(None)),
                host,
            );
            if let Some(error) = &start_error {
                app.set_start_error(error);
            }
            app
        })
}

/// A harness whose panel comes from a constructed journal error.
fn harness_with_error(dir: &Path, error: JournalError, observe: bool) -> Harness<'static, App> {
    let dir = dir.to_path_buf();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            let mut app = App::new(
                cc,
                Options {
                    data: Some(dir),
                    observe,
                    ..Default::default()
                },
                worker,
                Arc::new(Mutex::new(None)),
                Err(error.to_string()),
            );
            app.set_start_error(&anyhow::Error::new(error));
            app
        })
}

fn settle(h: &mut Harness<'_, App>) {
    h.run_steps(4);
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
    h.get_by_role_and_label(Role::Button, label).click();
    h.run_steps(2);
}

fn button_enabled(h: &Harness<'_, App>, label: &str) -> bool {
    !h.get_by_role_and_label(Role::Button, label)
        .accesskit_node()
        .is_disabled()
}

fn has_button(h: &Harness<'_, App>, label: &str) -> bool {
    h.query_by_role_and_label(Role::Button, label).is_some()
}

/// Some label contains `text` (the plain footer may repeat a path).
fn has_text(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

#[test]
fn interrupted_migration_with_snapshot_restores_and_offers_restart() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);

    // The title, the three paragraphs and the three buttons.
    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(&h, "升级前的快照在："));
    assert!(has_text(&h, &snapshot.display().to_string()));
    assert!(has_text(&h, "（保存于 "));
    assert!(has_text(
        &h,
        "从快照恢复后，工单数据库会回到那个时间点，之后的工单和对话不会保留。恢复前，当前文件会先另存为 hub-damaged-"
    ));
    assert!(has_text(
        &h,
        "暂不恢复时，Super Agent 以只读方式打开，不能运行工单；下次启动会再次提示。"
    ));
    for label in ["从快照恢复", "打开数据文件夹", "暂不恢复"] {
        assert!(has_button(&h, label), "missing button {label}");
        assert!(button_enabled(&h, label), "{label} should be enabled");
    }
    assert!(!has_button(&h, "立即重启"));

    let restarts = Arc::new(AtomicUsize::new(0));
    let counter = restarts.clone();
    h.state_mut().recovery.as_mut().unwrap().hooks = Hooks {
        restart: Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        ..Hooks::default()
    };

    click(&mut h, "从快照恢复");
    until(&mut h, "the restore", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Restored)
    });
    assert!(has_text(&h, "已从快照恢复。请重新启动 Super Agent。"));
    assert!(has_button(&h, "立即重启"));
    assert!(!has_button(&h, "从快照恢复"));

    // The snapshot is the live journal now; the old file is kept, not deleted.
    let live = data.path().join(sa_store::DB_FILE);
    assert!(has_table(&live, "snap") && !has_table(&live, "current"));
    let kept = damaged(data.path());
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(has_table(&kept[0], "current"));
    assert!(!data.path().join("backups").join("migration.json").exists());
    assert!(snapshot.exists());

    click(&mut h, "立即重启");
    until(&mut h, "the restart", |_| restarts.load(Ordering::SeqCst) == 1);
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
    assert!(
        matches!(h.state().recovery.as_ref().unwrap().phase, Phase::Closing),
        "the panel stays disabled until the window closes"
    );
    assert!(!button_enabled(&h, "立即重启"), "restart is one-shot while launching");
}

#[test]
fn interrupted_migration_without_snapshot_shows_the_missing_variant() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), false);
    let mut h = harness(data.path(), false);
    settle(&mut h);

    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(&h, "找不到升级前的快照（"));
    assert!(has_text(&h, &snapshot.display().to_string()));
    assert!(has_text(&h, "）。可以打开数据文件夹手动处理。"));
    assert!(!has_text(&h, "从快照恢复后"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(has_button(&h, "打开数据文件夹"));
    assert!(has_button(&h, "暂不恢复"));
}

#[test]
fn corrupt_journal_code_26_without_snapshot_offers_only_the_folder() {
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        data.path().join(sa_store::DB_FILE),
        b"this is definitely not a sqlite database file.".repeat(40),
    )
    .unwrap();
    let before = listing(data.path());
    let mut h = harness(data.path(), false);
    settle(&mut h);

    assert!(has_text(&h, "工单数据库已损坏"));
    assert!(has_text(&h, "无法读取 "));
    assert!(has_text(&h, &data.path().join(sa_store::DB_FILE).display().to_string()));
    assert!(has_text(
        &h,
        "（SQLite 错误 26：不是有效的数据库文件）。Super Agent 已停止写入，没有改动这个文件。"
    ));
    assert!(has_text(
        &h,
        "可以退出 Super Agent，先把这个文件复制到别处保存，再用 sqlite3 的 .recover 命令尝试抢救。"
    ));
    assert!(has_text(
        &h,
        "如果不需要旧数据，可以退出后把这个文件移走或改名，再次打开 Super Agent 会新建空的工单数据库。"
    ));
    assert!(has_button(&h, "打开数据文件夹"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "暂不恢复"));
    assert_eq!(listing(data.path()), before, "the damaged file stays untouched");
}

#[test]
fn corrupt_journal_code_11_with_snapshot_offers_restore() {
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(data.path().join("backups")).unwrap();
    let snapshot = data.path().join("backups").join(SNAPSHOT);
    sqlite_with(&snapshot, "snap");
    let mut h = harness_with_error(
        data.path(),
        JournalError::Corrupt {
            path: data.path().join(sa_store::DB_FILE),
            code: 11,
            latest_snapshot: Some((snapshot.clone(), SystemTime::now())),
        },
        false,
    );
    settle(&mut h);

    assert!(has_text(&h, "工单数据库已损坏"));
    assert!(has_text(
        &h,
        "（SQLite 错误 11：文件内容损坏）。Super Agent 已停止写入，没有改动这个文件。"
    ));
    assert!(has_text(&h, "可以从最近的快照恢复："));
    assert!(has_text(&h, &snapshot.display().to_string()));
    assert!(has_text(
        &h,
        "恢复后回到那个时间点，之后的工单和对话不会保留；当前文件会先另存为 hub-damaged-"
    ));
    assert!(!has_text(&h, ".recover"));
    assert!(has_button(&h, "从快照恢复") && button_enabled(&h, "从快照恢复"));
    assert!(has_button(&h, "打开数据文件夹"));
}

#[test]
fn failed_restore_shows_red_text_and_changes_nothing() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    assert!(has_button(&h, "从快照恢复"));

    // The snapshot vanishes between the panel and the click.
    std::fs::remove_file(&snapshot).unwrap();
    let before = listing(data.path());
    click(&mut h, "从快照恢复");
    until(&mut h, "the failed restore", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });

    assert!(has_text(&h, "恢复失败："));
    assert!(has_text(&h, "。当前文件没有改动。"));
    assert!(!has_text(&h, "已从快照恢复"));
    assert!(!has_text(&h, "正在从快照恢复…"));
    assert!(!has_button(&h, "立即重启"));
    // The buttons come back so the user can retry or leave.
    assert!(button_enabled(&h, "从快照恢复"));
    assert!(button_enabled(&h, "暂不恢复"));
    assert_eq!(listing(data.path()), before);
    assert!(damaged(data.path()).is_empty());

    // The error is drawn in the failure colour.
    let pal = h.state().pal.clone();
    let error = h.state().recovery.as_ref().unwrap().error.clone().unwrap();
    assert!(error.starts_with("恢复失败："), "{error}");
    assert_ne!(pal.fail.fg, pal.ink);
}

#[test]
fn restoring_state_disables_every_button() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    // Hold the restore open with a controlled result.
    let (pending, release) = crate::ops::Pending::<()>::controlled();
    h.state_mut().recovery.as_mut().unwrap().phase = Phase::Restoring(pending);
    h.run_steps(3);

    assert!(has_text(&h, "正在从快照恢复…"));
    for label in ["从快照恢复", "打开数据文件夹", "暂不恢复"] {
        assert!(!button_enabled(&h, label), "{label} should be disabled");
    }
    release.send(Ok(())).unwrap();
    until(&mut h, "the release", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Restored)
    });
    assert!(has_text(&h, "已从快照恢复。请重新启动 Super Agent。"));
}

#[test]
fn observer_window_cannot_restore() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let before = listing(data.path());
    let mut h = harness(data.path(), true);
    settle(&mut h);

    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_button(&h, "从快照恢复"));
    assert!(!button_enabled(&h, "从快照恢复"));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert!(button_enabled(&h, "打开数据文件夹"));
    assert!(button_enabled(&h, "暂不恢复"));
    click(&mut h, "从快照恢复");
    h.run_steps(5);
    assert!(matches!(h.state().recovery.as_ref().unwrap().phase, Phase::Ready));
    assert_eq!(listing(data.path()), before);
}

#[test]
fn open_folder_and_dismiss_run_without_blocking() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    let opened = Arc::new(Mutex::new(Vec::new()));
    let log = opened.clone();
    h.state_mut().recovery.as_mut().unwrap().hooks = Hooks {
        open_folder: Arc::new(move |dir| {
            log.lock().unwrap().push(dir.to_path_buf());
            Ok(())
        }),
        ..Hooks::default()
    };
    click(&mut h, "打开数据文件夹");
    until(&mut h, "the folder to open", |_| !opened.lock().unwrap().is_empty());
    assert_eq!(*opened.lock().unwrap(), vec![data.path().to_path_buf()]);

    // A failure to launch the file manager is reported in red text.
    h.state_mut().recovery.as_mut().unwrap().hooks = Hooks {
        open_folder: Arc::new(|_| anyhow::bail!("没有文件管理器")),
        ..Hooks::default()
    };
    click(&mut h, "打开数据文件夹");
    until(&mut h, "the failure", |app| app.recovery.as_ref().unwrap().error.is_some());
    assert!(has_text(&h, "无法打开数据文件夹：没有文件管理器"));

    // "暂不恢复" hides the panel; the window keeps its plain read-only text.
    click(&mut h, "暂不恢复");
    h.run_steps(3);
    assert!(!has_text(&h, "上次工单数据库升级没有完成"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(has_text(&h, "只读观察模式："));
}

#[test]
fn other_startup_errors_keep_the_plain_text() {
    let data = tempfile::tempdir().unwrap();
    // A second desktop cannot claim the journal: a plain error, no panel.
    let first = Host::start_with(data.path().to_path_buf(), fakes, || {}).unwrap();
    let mut h = harness(data.path(), false);
    settle(&mut h);
    assert!(h.state().recovery.is_none());
    assert!(h.state().host_error.is_some());
    assert!(has_text(&h, "只读观察模式："));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "打开数据文件夹"));
    drop(h);
    drop(first);
}
