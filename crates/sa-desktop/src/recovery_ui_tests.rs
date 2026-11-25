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
use sa_store::journal::{JournalError, RestoreOutcome};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
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
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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
    std::fs::write(
        dir.join("backups").join("migration.json"),
        marker.to_string(),
    )
    .unwrap();
    snapshot
}

/// Compare paths and exact bytes, including markers, snapshots and sidecars.
fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push((
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
    }
    files.retain(|(name, _)| !matches!(name.as_str(), "desktop.lock" | ".journal-restore.lock"));
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

fn harness(dir: &Path, observe: bool) -> Harness<'static, App> {
    harness_with_start(dir, observe, move |dir, ctx| {
        assert!(!observe, "observer must not attempt to start a host");
        Host::start_with(dir, fakes, move || ctx.request_repaint())
    })
}

fn harness_with_start(
    dir: &Path,
    observe: bool,
    start_host: impl FnOnce(PathBuf, egui::Context) -> anyhow::Result<Host> + 'static,
) -> Harness<'static, App> {
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
                start_host,
            )
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
    until(&mut h, "the restart", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Closing)
    });
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
    assert!(
        matches!(h.state().recovery.as_ref().unwrap().phase, Phase::Closing),
        "the panel stays disabled until the window closes"
    );
    assert!(
        !button_enabled(&h, "立即重启"),
        "restart is one-shot while launching"
    );
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
    assert!(has_text(
        &h,
        &data.path().join(sa_store::DB_FILE).display().to_string()
    ));
    assert!(has_text(
        &h,
        "（SQLite 错误 26：不是有效的数据库文件）。Super Agent 已停止写入，没有改动这个文件。"
    ));
    assert!(has_text(
        &h,
        "可以退出所有 Super Agent 窗口，把 hub.sqlite3 和同一目录下存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起复制到别处保存，再对副本用 sqlite3 的 .recover 命令尝试抢救。"
    ));
    assert!(has_text(
        &h,
        "如果不需要旧数据，可以退出后把这些文件一起移走或改名，再次打开 Super Agent 会新建空的工单数据库。"
    ));
    assert!(has_button(&h, "打开数据文件夹"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "暂不恢复"));
    assert_eq!(
        listing(data.path()),
        before,
        "the damaged file stays untouched"
    );
}

#[test]
fn corrupt_journal_code_11_with_snapshot_offers_restore() {
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(data.path().join("backups")).unwrap();
    let snapshot = data.path().join("backups").join(SNAPSHOT);
    sqlite_with(&snapshot, "snap");
    let live = data.path().join(sa_store::DB_FILE);
    sqlite_with(&live, "current");
    let mut bytes = std::fs::read(&live).unwrap();
    // The header declares more pages than the file contains (SQLITE_CORRUPT).
    bytes[28..32].copy_from_slice(&200_u32.to_be_bytes());
    std::fs::write(&live, bytes).unwrap();
    let mut h = harness(data.path(), false);
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
fn vanished_snapshot_shows_red_text_and_changes_nothing() {
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

    assert!(has_text(&h, "快照不完整，没有恢复："));
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
    assert!(error.starts_with("快照不完整，没有恢复："), "{error}");
    assert_ne!(pal.fail.fg, pal.ink);
}

#[test]
fn restoring_state_disables_every_button() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    // Hold the restore open with a controlled result.
    let (pending, release) = crate::ops::Pending::<anyhow::Result<RestoreOutcome>>::controlled();
    h.state_mut().recovery.as_mut().unwrap().phase = Phase::Restoring(pending);
    h.run_steps(3);

    assert!(has_text(&h, "正在从快照恢复…"));
    for label in ["从快照恢复", "打开数据文件夹", "暂不恢复"] {
        assert!(!button_enabled(&h, label), "{label} should be disabled");
    }
    release.send(Ok(Ok(RestoreOutcome::Installed))).unwrap();
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
    until(&mut h, "observer inspection", |app| app.recovery.is_some());

    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(h.state().host.is_none());
    assert!(!data.path().join(sa_engine::owner::LOCK_FILE).exists());
    assert!(!data.path().join(".journal-restore.lock").exists());
    assert!(has_button(&h, "从快照恢复"));
    assert!(!button_enabled(&h, "从快照恢复"));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert!(button_enabled(&h, "打开数据文件夹"));
    assert!(button_enabled(&h, "暂不恢复"));
    click(&mut h, "从快照恢复");
    h.run_steps(5);
    assert!(matches!(
        h.state().recovery.as_ref().unwrap().phase,
        Phase::Ready
    ));
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
    until(&mut h, "the folder to open", |_| {
        !opened.lock().unwrap().is_empty()
    });
    assert_eq!(*opened.lock().unwrap(), vec![data.path().to_path_buf()]);

    // A failure to launch the file manager is reported in red text.
    h.state_mut().recovery.as_mut().unwrap().hooks = Hooks {
        open_folder: Arc::new(|_| anyhow::bail!("没有文件管理器")),
        ..Hooks::default()
    };
    click(&mut h, "打开数据文件夹");
    until(&mut h, "the failure", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });
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
    until(&mut h, "healthy observer inspection", |app| {
        app.recovery_probe.is_none()
    });
    assert!(h.state().recovery.is_none());
    assert!(h.state().host_error.is_some());
    assert!(has_text(&h, "只读观察模式："));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "打开数据文件夹"));
    drop(h);
    drop(first);
}

#[test]
fn installed_restore_offers_cleanup_only_then_restart() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    let restores = Arc::new(AtomicUsize::new(0));
    let cleanups = Arc::new(AtomicUsize::new(0));
    let restore_calls = Arc::clone(&restores);
    let cleanup_calls = Arc::clone(&cleanups);
    h.state_mut().recovery.as_mut().unwrap().hooks = Hooks {
        restore: Arc::new(move |_, _, _| {
            restore_calls.fetch_add(1, Ordering::SeqCst);
            Ok(RestoreOutcome::InstalledMarkerNotRemoved(anyhow::anyhow!(
                "清理失败测试"
            )))
        }),
        cleanup: Arc::new(move |_| {
            cleanup_calls.fetch_add(1, Ordering::SeqCst);
            Ok(RestoreOutcome::Installed)
        }),
        ..Hooks::default()
    };
    click(&mut h, "从快照恢复");
    until(&mut h, "cleanup offer", |app| {
        matches!(
            app.recovery.as_ref().unwrap().phase,
            Phase::CleanupRequired(_)
        )
    });
    assert!(has_text(
        &h,
        "已从快照恢复，但没能清理升级标记：清理失败测试。"
    ));
    assert!(button_enabled(&h, "重试清理"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "立即重启"));
    click(&mut h, "重试清理");
    until(&mut h, "cleanup success", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Restored)
    });
    assert!(has_text(&h, "已从快照恢复。请重新启动 Super Agent。"));
    assert!(button_enabled(&h, "立即重启"));
    assert_eq!(restores.load(Ordering::SeqCst), 1);
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_snapshot_keeps_current_files_byte_for_byte() {
    let data = tempfile::tempdir().unwrap();
    let snapshot = interrupted(data.path(), false);
    std::fs::write(snapshot, [0xcd; 4096]).unwrap();
    let mut h = harness(data.path(), false);
    settle(&mut h);
    let before = listing(data.path());
    click(&mut h, "从快照恢复");
    until(&mut h, "snapshot validation failure", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });
    assert!(has_text(&h, "快照不完整，没有恢复："));
    assert!(has_text(&h, "。当前文件没有改动。"));
    assert!(!has_button(&h, "立即重启"));
    assert_eq!(listing(data.path()), before);
}

#[test]
fn changed_failure_identity_cancels_restore_and_disables_stale_action() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    std::fs::remove_file(data.path().join("backups/migration.json")).unwrap();
    let before = listing(data.path());
    click(&mut h, "从快照恢复");
    until(&mut h, "restore cancellation", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });
    assert!(has_text(
        &h,
        "工单数据库的状态已经变化，已取消这次恢复。请重新启动 Super Agent 后再看。"
    ));
    assert!(!button_enabled(&h, "从快照恢复"));
    assert_eq!(listing(data.path()), before);
}

#[test]
fn in_use_error_in_chain_is_explained_without_claiming_success() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    h.state_mut().recovery.as_mut().unwrap().hooks.restore = Arc::new(|_, _, _| {
        let cause = if cfg!(windows) {
            std::io::Error::from_raw_os_error(32) // ERROR_SHARING_VIOLATION
        } else {
            std::io::Error::from(std::io::ErrorKind::PermissionDenied)
        };
        Err(anyhow::Error::new(cause).context("归档当前工单数据库"))
    });
    let before = listing(data.path());
    click(&mut h, "从快照恢复");
    until(&mut h, "in-use failure", |app| {
        app.recovery.as_ref().unwrap().error.is_some()
    });
    assert!(has_text(
        &h,
        "恢复失败：工单数据库正被其他程序占用。请关闭其他 Super Agent 窗口后重试。当前文件没有改动。"
    ));
    assert!(button_enabled(&h, "从快照恢复"));
    assert!(!has_button(&h, "立即重启"));
    assert_eq!(listing(data.path()), before);
}

#[test]
fn unresolved_restore_record_shows_path_and_does_not_create_database() {
    let data = tempfile::tempdir().unwrap();
    let record = restore_record(data.path(), "installing");
    let mut h = harness(data.path(), false);
    settle(&mut h);
    assert!(has_text(&h, "上次恢复没有完成"));
    assert!(has_text(
        &h,
        "上次从快照恢复时中断，Super Agent 无法自动完成："
    ));
    assert!(has_text(&h, &record.display().to_string()));
    assert!(has_text(&h, "为避免数据丢失，没有创建新的工单数据库。"));
    assert!(button_enabled(&h, "打开数据文件夹"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "重试清理"));
    assert!(!data.path().join(sa_store::DB_FILE).exists());
}

fn restore_record(dir: &Path, phase: &str) -> PathBuf {
    let backups = dir.join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    let record = backups.join("restore.json");
    let json = serde_json::json!({
        "snapshot_name": SNAPSHOT,
        "staged_name": "hub-restore-00000000000000012345.sqlite3.pending",
        "damaged_name": "hub-damaged-00000000000000012345.sqlite3",
        "had_journal": true,
        "migration_digest": null,
        "phase": phase,
    });
    std::fs::write(&record, json.to_string()).unwrap();
    record
}

#[test]
fn startup_cleanup_panel_uses_the_real_cleanup_entry_point() {
    let data = tempfile::tempdir().unwrap();
    let record = restore_record(data.path(), "installed");
    sqlite_with(&data.path().join(sa_store::DB_FILE), "snap");
    // Inject the typed startup failure. An unrestricted Host would have
    // cleaned this fixture before returning an error to the panel.
    let error = JournalError::RestoreInterrupted {
        record: record.clone(),
        snapshot_name: Some(SNAPSHOT.into()),
        installed: true,
        reason: "无法移除标记".into(),
    };
    let mut h = harness_with_start(data.path(), false, move |_, _| Err(error.into()));
    settle(&mut h);
    assert!(has_text(&h, "上次恢复没有完成清理"));
    assert!(has_text(
        &h,
        "快照已经装好，但升级标记还没清理：无法移除标记。"
    ));
    assert!(button_enabled(&h, "重试清理"));
    assert!(button_enabled(&h, "打开数据文件夹"));
    assert!(!has_button(&h, "从快照恢复"));
    click(&mut h, "重试清理");
    until(&mut h, "startup cleanup", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Restored)
    });
    assert!(has_text(&h, "已从快照恢复。请重新启动 Super Agent。"));
    assert!(!record.exists());
    assert!(damaged(data.path()).is_empty());
}

#[test]
fn malformed_migration_marker_offers_only_the_folder() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let path = data.path().join("backups").join("migration.json");
    std::fs::write(&path, b"invalid JSON").unwrap();
    let mut h = harness(data.path(), false);
    settle(&mut h);
    assert!(has_text(&h, "升级标记无法识别"));
    assert!(has_text(&h, &path.display().to_string()));
    assert!(has_text(&h, "为安全起见，没有继续升级。"));
    assert!(button_enabled(&h, "打开数据文件夹"));
    assert!(!has_button(&h, "从快照恢复"));
    assert!(!has_button(&h, "重试清理"));
}

#[test]
fn observer_disables_cleanup_and_keeps_record() {
    let data = tempfile::tempdir().unwrap();
    let record = restore_record(data.path(), "installed");
    sqlite_with(&data.path().join(sa_store::DB_FILE), "snap");
    let mut h = harness(data.path(), true);
    until(&mut h, "observer cleanup inspection", |app| {
        app.recovery.is_some()
    });
    assert!(h.state().host.is_none());
    assert!(has_text(&h, "上次恢复没有完成清理"));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert!(!button_enabled(&h, "重试清理"));
    click(&mut h, "重试清理");
    assert!(record.is_file());
    assert!(!data.path().join(sa_engine::owner::LOCK_FILE).exists());
}

#[test]
fn losing_the_owner_lock_inspects_without_enabling_recovery_actions() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let _owner = sa_engine::owner::claim(data.path()).unwrap();
    let mut h = harness(data.path(), false);
    until(&mut h, "owner-lock observer inspection", |app| {
        app.recovery.is_some()
    });
    assert!(h.state().host.is_none());
    assert!(has_text(&h, "上次工单数据库升级没有完成"));
    assert!(has_text(&h, "请在主窗口处理。"));
    assert!(!button_enabled(&h, "从快照恢复"));
}

#[test]
fn restore_waits_for_reader_acknowledgement_and_leaves_reader_paused() {
    let data = tempfile::tempdir().unwrap();
    interrupted(data.path(), true);
    let mut h = harness(data.path(), false);
    settle(&mut h);
    let (tx, commands) = mpsc::channel();
    let (_messages, rx) = mpsc::channel();
    h.state_mut().worker = worker::Worker { tx, rx };
    let (entered, attempts) = mpsc::channel();
    let (release, finish) = mpsc::channel();
    let finish = Mutex::new(finish);
    h.state_mut().recovery.as_mut().unwrap().hooks.restore = Arc::new(move |_, _, _| {
        entered.send(()).unwrap();
        finish.lock().unwrap().recv().unwrap();
        Ok(RestoreOutcome::Installed)
    });
    click(&mut h, "从快照恢复");
    let ack = loop {
        if let worker::Cmd::Pause(ack) = commands.recv_timeout(Duration::from_secs(10)).unwrap() {
            break ack;
        }
    };
    assert!(matches!(
        h.state().recovery.as_ref().unwrap().phase,
        Phase::Restoring(_)
    ));
    assert!(matches!(
        attempts.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    ack.send(()).unwrap();
    attempts.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        !commands
            .try_iter()
            .any(|cmd| matches!(cmd, worker::Cmd::Resume))
    );
    release.send(()).unwrap();
    until(&mut h, "restore completion after reader closed", |app| {
        matches!(app.recovery.as_ref().unwrap().phase, Phase::Restored)
    });
    assert!(
        !commands
            .try_iter()
            .any(|cmd| matches!(cmd, worker::Cmd::Resume))
    );
}
