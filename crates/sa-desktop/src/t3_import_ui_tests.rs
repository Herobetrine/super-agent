//! "从 T3 Code 导入" with real App clicks, a synthetic T3 data folder and
//! disposable data directories. Nothing here reads the user's T3 Code data.

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
use rusqlite::{Connection, params};
use sa_store::projects::{self, Project};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const SCHEMA: &str = "
    CREATE TABLE projection_projects (
        project_id TEXT PRIMARY KEY, title TEXT, workspace_root TEXT, created_at TEXT,
        updated_at TEXT, deleted_at TEXT, pinned_at TEXT, archived_at TEXT
    );
    CREATE TABLE orchestration_v2_projection_threads (
        thread_id TEXT PRIMARY KEY, project_id TEXT, title TEXT, default_provider TEXT,
        created_at TEXT, updated_at TEXT, archived_at TEXT, deleted_at TEXT, payload_json TEXT
    );
    CREATE TABLE orchestration_v2_projection_messages (
        message_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, role TEXT,
        created_at TEXT, payload_json TEXT, search_text TEXT
    );
    CREATE TABLE orchestration_v2_projection_turn_items (
        turn_item_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, ordinal INTEGER,
        type TEXT, status TEXT, updated_at TEXT, payload_json TEXT
    );";

/// A synthetic T3 data folder: three projects (one new, one already listed in
/// the journal, one whose folder is gone) and three conversations.
struct Fixture {
    _temp: tempfile::TempDir,
    folder: PathBuf,
    alpha: PathBuf,
    beta: PathBuf,
}

fn fixture(broken_thread: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("t3-userdata");
    let alpha = temp.path().join("alpha-work");
    let beta = temp.path().join("beta-work");
    let gone = temp.path().join("gone-work");
    for dir in [&folder, &alpha, &beta] {
        fs::create_dir(dir).unwrap();
    }
    let db = Connection::open(folder.join("statev2.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    for (id, title, path) in [("p1", "Alpha", &alpha), ("p2", "Beta", &beta), ("p3", "Gone", &gone)] {
        db.execute(
            "INSERT INTO projection_projects VALUES (?1,?2,?3,'2024-03-01','2024-03-04',NULL,NULL,NULL)",
            params![id, title, path.to_string_lossy()],
        )
        .unwrap();
    }
    for (index, id) in ["t1", "t2", "t3"].into_iter().enumerate() {
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads
             VALUES (?1,'p1',?2,'test-provider','2024-03-02T01:00:00.000Z','2024-03-05',NULL,NULL,'{}')",
            params![id, format!("Conversation {}", index + 1)],
        )
        .unwrap();
        let payload = if broken_thread && id == "t3" { "{malformed" } else { "{\"text\":\"hello\"}" };
        db.execute(
            "INSERT INTO orchestration_v2_projection_messages
             VALUES (?1,?2,'run','user','2024-03-02T01:01:00.000Z',?3,'')",
            params![format!("m-{id}"), id, payload],
        )
        .unwrap();
    }
    db.close().unwrap();
    Fixture { _temp: temp, folder, alpha, beta }
}

fn project(path: &Path, name: &str, source: &str, pinned: bool, updated: &str) -> Project {
    Project {
        path: path.to_string_lossy().into_owned(),
        name: name.into(),
        pinned,
        source: source.into(),
        source_id: None,
        source_updated: Some(updated.into()),
        added: 1.0,
    }
}

fn harness(data: &Path, t3: Option<&Path>, observe: bool, demo: bool, seed: Vec<Project>) -> Harness<'static, App> {
    let dir = data.to_path_buf();
    let t3 = t3.map(Path::to_path_buf);
    Harness::builder()
        .with_size(egui::vec2(1280.0, 1000.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = if observe || demo {
                Err("只读预览".into())
            } else {
                Host::start_with(dir.clone(), fakes, move || ctx.request_repaint()).map_err(|e| format!("{e:#}"))
            };
            if !seed.is_empty() {
                let conn = Connection::open(dir.join(sa_store::DB_FILE)).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                projects::init(&conn).unwrap();
                projects::add(&conn, &seed).unwrap();
            }
            let worker = worker::spawn(crate::data::StoreSource::opener(dir.clone()), cc.egui_ctx.clone());
            App::new(
                cc,
                Options { data: Some(dir), observe, demo, t3_folder: t3, ..Default::default() },
                worker,
                Arc::new(Mutex::new(None)),
                host,
            )
        })
}

fn writable(data: &Path, fx: &Fixture, seed: Vec<Project>) -> Harness<'static, App> {
    harness(data, Some(&fx.folder), false, false, seed)
}

/// The fixture's "Beta" folder is already in the journal.
fn listed_beta(fx: &Fixture) -> Vec<Project> {
    vec![project(&fx.beta, "Beta", "manual", false, "2024-03-01")]
}

fn until(h: &mut Harness<'_, App>, what: &str, cond: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            return;
        }
        if Instant::now() >= end {
            panic!("timed out waiting for {what}; step={}", h.state().t3.step_name());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn label_visible(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_by_label(text).is_some()
}

fn contains_visible(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_by_label_contains(text).is_some()
}

fn enabled(h: &Harness<'_, App>, role: Role, label: &str) -> bool {
    !h.get_by_role_and_label(role, label).accesskit_node().is_disabled()
}

fn click_in_view(h: &mut Harness<'_, App>, role: Role, label: &str) {
    h.get_by_role_and_label(role, label).scroll_to_me();
    h.run_steps(3);
    // ScrollIntoView can move the widget. Query its post-scroll coordinates.
    h.get_by_role_and_label(role, label).click();
    h.run_steps(3);
}

fn open_dialog(h: &mut Harness<'_, App>) {
    // The card and the recent list arrive asynchronously and move the link.
    until(h, "welcome page settled", |a| a.t3.settled());
    h.run_steps(5);
    h.get_by_label("从 T3 Code 导入…").click();
    h.run_steps(3);
}

struct MapStorage(HashMap<String, String>);

impl eframe::Storage for MapStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.into(), value);
    }
    fn flush(&mut self) {}
}

// ---- entry points ---------------------------------------------------------

#[test]
fn welcome_card_follows_its_conditions_and_dismissal_persists() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, vec![]);
    until(&mut h, "card", |a| a.t3.card_state() == "shown");
    assert!(label_visible(&h, "从 T3 Code 导入"));
    assert!(contains_visible(
        &h,
        "找到 T3 Code 的数据：3 个项目、3 个对话。可以把项目加到这里，并把对话导出为本地归档（仅供查阅）。"
    ));
    assert!(label_visible(&h, "开始导入…") && label_visible(&h, "不再提示"));
    assert!(label_visible(&h, "从 T3 Code 导入…"), "persistent link");

    h.get_by_label("不再提示").click();
    h.run_steps(3);
    assert!(h.state().t3.dismissed());
    assert!(!label_visible(&h, "开始导入…"));
    assert!(label_visible(&h, "从 T3 Code 导入…"), "the link stays after dismissal");
    let mut storage = MapStorage(HashMap::new());
    eframe::App::save(h.state_mut(), &mut storage);
    assert_eq!(storage.0.get(crate::t3_import_ui::DISMISS_KEY).map(String::as_str), Some("1"));
}

#[test]
fn welcome_card_is_hidden_without_data_after_import_and_in_read_only_windows() {
    let fx = fixture(false);
    // Nothing to import: the default folder has no data.
    let data = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), Some(empty.path()), false, false, vec![]);
    until(&mut h, "probe", |a| a.t3.card_state() == "hidden");
    assert!(!label_visible(&h, "开始导入…"));
    assert!(label_visible(&h, "从 T3 Code 导入…"));

    // Already imported once.
    let data = tempfile::tempdir().unwrap();
    let imported = vec![project(&fx.alpha, "Alpha", "t3", false, "2024-03-01")];
    let mut h = writable(data.path(), &fx, imported);
    until(&mut h, "probe", |a| a.t3.card_state() == "hidden");
    assert!(!label_visible(&h, "开始导入…"));
    assert!(label_visible(&h, "从 T3 Code 导入…"));

    // A synthetic folder holding no project and no conversation.
    let blank = tempfile::tempdir().unwrap();
    let db = Connection::open(blank.path().join("statev2.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.close().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), Some(blank.path()), false, false, vec![]);
    until(&mut h, "counting", |a| a.t3.card_state() == "hidden");
    assert!(!label_visible(&h, "开始导入…"));

    // Observer and demo windows show neither the card nor the link.
    for (observe, demo) in [(true, false), (false, true)] {
        let data = tempfile::tempdir().unwrap();
        let mut h = harness(data.path(), Some(&fx.folder), observe, demo, vec![]);
        h.run_steps(30);
        assert_eq!(h.state().t3.card_state(), "idle");
        assert!(!label_visible(&h, "开始导入…"));
        assert!(!label_visible(&h, "从 T3 Code 导入…"));
    }
}

// ---- step 1 ---------------------------------------------------------------

#[test]
fn step_one_shows_each_state() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();

    // Default location without data.
    let missing = data.path().join("no-t3-here");
    let mut h = harness(data.path(), Some(&missing), false, false, vec![]);
    open_dialog(&mut h);
    until(&mut h, "not found", |a| a.t3.step_name() == "locate");
    assert!(contains_visible(&h, "T3 Code 数据文件夹："));
    assert!(contains_visible(
        &h,
        &format!("没有在默认位置找到 T3 Code 数据（{}）。如果你改过数据目录，请点击“选择文件夹…”。", missing.display())
    ));
    assert!(label_visible(&h, "选择文件夹…"));
    assert!(label_visible(&h, "取消"));

    // A folder chosen by hand that has no data.
    let empty = tempfile::tempdir().unwrap();
    h.state_mut().t3_choose_folder(empty.path().to_path_buf());
    until(&mut h, "chosen not found", |a| a.t3.step_name() == "locate");
    assert!(label_visible(&h, "所选文件夹中没有找到 T3 Code 数据（缺少 statev2.sqlite），请重新选择。"));

    // A file that is no database: unreadable, with a retry.
    let broken = tempfile::tempdir().unwrap();
    fs::write(broken.path().join("statev2.sqlite"), vec![7u8; 4096]).unwrap();
    h.state_mut().t3_choose_folder(broken.path().to_path_buf());
    until(&mut h, "unreadable", |a| a.t3.step_name() == "locate");
    assert!(contains_visible(&h, "无法读取 T3 Code 数据："));
    assert!(contains_visible(&h, "如果 T3 Code 正在运行，请先退出后重试。"));
    h.get_by_role_and_label(Role::Button, "重试").click();
    until(&mut h, "unreadable again", |a| a.t3.step_name() == "locate");
    assert!(label_visible(&h, "重试"));

    // A database of another T3 version.
    let other = tempfile::tempdir().unwrap();
    let db = Connection::open(other.path().join("statev2.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE unrelated(x)").unwrap();
    db.close().unwrap();
    h.state_mut().t3_choose_folder(other.path().to_path_buf());
    until(&mut h, "unsupported", |a| a.t3.step_name() == "locate");
    assert!(contains_visible(&h, "无法识别这个版本的 T3 Code 数据（缺少 "));
    assert!(contains_visible(&h, "），没有做任何改动。"));

    // Valid but empty.
    let blank = tempfile::tempdir().unwrap();
    let db = Connection::open(blank.path().join("statev2.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.close().unwrap();
    h.state_mut().t3_choose_folder(blank.path().to_path_buf());
    until(&mut h, "empty", |a| a.t3.step_name() == "locate");
    assert!(label_visible(&h, "没有找到可导入的项目或对话。"));

    // The real fixture moves on to the selection.
    h.state_mut().t3_choose_folder(fx.folder.clone());
    until(&mut h, "select", |a| a.t3.step_name() == "select");
}

// ---- step 2 ---------------------------------------------------------------

#[test]
fn step_two_selection_rules() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    open_dialog(&mut h);
    until(&mut h, "select", |a| a.t3.step_name() == "select");
    assert!(label_visible(&h, "项目（3）") && label_visible(&h, "对话（3）"));
    assert!(label_visible(&h, "已在列表中") && label_visible(&h, "文件夹已不存在"));
    assert!(contains_visible(&h, "导入后会出现在欢迎页的“最近的项目”里（最多显示 8 个，有工单的项目排在前面）。"));
    assert!(contains_visible(&h, "保存位置："));
    assert!(label_visible(&h, "导出的对话仅供查阅，不能在 Super Agent 里继续。"));
    // Alpha is selectable and on; Beta and Gone are fixed rows.
    assert!(enabled(&h, Role::CheckBox, "Alpha"));
    assert!(!enabled(&h, Role::CheckBox, "Beta"));
    assert!(!enabled(&h, Role::CheckBox, "Gone"));
    assert_eq!(h.state().t3.picked(), vec![true, false, false]);
    assert!(enabled(&h, Role::Button, "开始导入"));

    // Clicking fixed rows changes nothing.
    h.get_by_role_and_label(Role::CheckBox, "Beta").click();
    h.get_by_role_and_label(Role::CheckBox, "Gone").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.picked(), vec![true, false, false]);

    // Select none, then all.
    h.get_by_label("全不选").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.picked(), vec![false, false, false]);
    h.get_by_label("全选").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.picked(), vec![true, false, false]);

    // Nothing new: the start button is off and says why.
    h.get_by_label("全不选").click();
    h.run_steps(3);
    assert!(enabled(&h, Role::Button, "开始导入"), "the export alone is still something to do");
    assert!(!label_visible(&h, "没有可导入的新内容"));
    h.get_by_role_and_label(Role::CheckBox, "把对话导出为本地归档（仅供查阅）").click();
    h.run_steps(3);
    assert!(!enabled(&h, Role::Button, "开始导入"));
    assert!(label_visible(&h, "没有可导入的新内容"));
    h.get_by_label("全选").click();
    h.run_steps(3);
    assert!(enabled(&h, Role::Button, "开始导入"));
    assert!(!label_visible(&h, "没有可导入的新内容"));

    h.get_by_label("取消").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.step_name(), "closed");
}

#[test]
fn selection_step_can_switch_folders_and_resets_both_selections() {
    let first = fixture(false);
    let second = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &first, listed_beta(&first));
    open_dialog(&mut h);
    until(&mut h, "first folder's selection laid out", |a| a.t3.select_settled());
    assert!(contains_visible(&h, &first.folder.to_string_lossy()));
    click_in_view(&mut h, Role::Button, "全不选");
    assert_eq!(h.state().t3.picked(), [false; 3]);
    click_in_view(&mut h, Role::CheckBox, "把对话导出为本地归档（仅供查阅）");
    assert!(!enabled(&h, Role::Button, "开始导入"));

    h.state_mut().t3.next_picked_folder = Some(second.folder.clone());
    h.get_by_role_and_label(Role::Button, "选择文件夹…").click();
    until(&mut h, "new folder's selections", |a| a.t3.select_settled() && a.t3.picked() == [true, true, false]);
    assert!(contains_visible(&h, &second.folder.to_string_lossy()));
    assert_eq!(
        h.get_by_role_and_label(Role::CheckBox, "把对话导出为本地归档（仅供查阅）")
            .accesskit_node().toggled(),
        Some(egui::accesskit::Toggled::True)
    );
}

// ---- steps 3 and 4 ---------------------------------------------------------

/// Block the journal's writer so the project import stays in flight.
fn hold_journal(data: &Path) -> Connection {
    let conn = Connection::open(data.join(sa_store::DB_FILE)).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn
}

fn start_import(h: &mut Harness<'_, App>, export: bool) {
    open_dialog(h);
    until(h, "select", |a| a.t3.step_name() == "select");
    h.run_steps(10);
    if !export {
        h.get_by_role_and_label(Role::CheckBox, "把对话导出为本地归档（仅供查阅）").click();
        h.run_steps(5);
    }
    h.get_by_role_and_label(Role::Button, "开始导入").click();
}

#[test]
fn projects_only_import_shows_progress_then_the_project_result() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    h.run_steps(3);
    let blocker = hold_journal(data.path());
    start_import(&mut h, false);
    until(&mut h, "run", |a| a.t3.step_name() == "run");
    assert!(label_visible(&h, "正在导入项目…"));
    assert!(!enabled(&h, Role::Button, "停止"), "a project-only import has nothing to stop");
    blocker.execute_batch("ROLLBACK").unwrap();
    until(&mut h, "done", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "导入完成"));
    assert!(label_visible(&h, "已加入 1 个项目。"));
    assert!(label_visible(&h, "完成"));
    assert!(!label_visible(&h, "打开归档文件夹"), "nothing was exported");
    // The journal now holds the T3 project and the card is gone for good.
    let conn = Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
    assert!(projects::has_source(&conn, "t3").unwrap());
    assert!(!data.path().join("archives").exists());
    h.get_by_label("完成").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.step_name(), "closed");
    until(&mut h, "card hidden", |a| a.t3.card_state() == "hidden");
    until(&mut h, "project added to welcome cache", |a| {
        a.t3_recent().iter().any(|entry| entry.name.as_deref() == Some("Alpha"))
    });
}

#[test]
fn export_reports_progress_ignores_close_while_running_and_finishes() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    let hold = Arc::new(AtomicBool::new(true));
    h.state_mut().t3.hold = Some(hold.clone());
    start_import(&mut h, true);
    until(&mut h, "first conversation", |a| a.t3.export_progress() == Some(1));
    assert!(label_visible(&h, "正在导出对话 1/3…"));
    assert!(label_visible(&h, "停止"));
    assert_eq!(h.query_all_by_role(Role::ProgressIndicator).count(), 2, "spinner and bar");

    // Esc and the close button do nothing while exporting.
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert_eq!(h.state().t3.step_name(), "run");
    h.get_by_label("关闭对话框").click();
    h.run_steps(3);
    assert_eq!(h.state().t3.step_name(), "run");

    hold.store(false, Ordering::Relaxed);
    until(&mut h, "done", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "导入完成"));
    assert!(label_visible(&h, "已加入 1 个项目，已导出 3 个对话。"));
    assert!(label_visible(&h, "打开归档文件夹"));
    let archive = h.state().t3.archive_folder().expect("archive folder");
    assert!(archive.starts_with(data.path().join("archives").join("t3").canonicalize().unwrap()));
    assert!(archive.join("index.md").is_file());
}

#[test]
fn stop_asks_for_the_current_conversation_then_shows_the_stopped_result() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    let hold = Arc::new(AtomicBool::new(true));
    h.state_mut().t3.hold = Some(hold.clone());
    start_import(&mut h, true);
    until(&mut h, "first conversation", |a| a.t3.export_progress() == Some(1));
    h.get_by_role_and_label(Role::Button, "停止").click();
    h.run_steps(3);
    assert!(label_visible(&h, "正在完成当前对话后停止（1/3）…"));
    assert!(label_visible(&h, "正在停止…"));
    assert!(!enabled(&h, Role::Button, "正在停止…"));
    hold.store(false, Ordering::Relaxed);
    until(&mut h, "stopped", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "已停止"));
    assert!(label_visible(
        &h,
        "已停止导入。已加入 1 个项目，已导出 1/3 个对话。再次导入会保存到新的文件夹，不会覆盖已有文件。"
    ));
    assert!(label_visible(&h, "打开归档文件夹"), "one conversation was exported");
}

#[test]
fn closing_the_app_waits_for_the_export_pair_and_index() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    let hold = Arc::new(AtomicBool::new(true));
    h.state_mut().t3.hold = Some(hold.clone());
    start_import(&mut h, true);
    until(&mut h, "first conversation", |a| a.t3.export_progress() == Some(1));
    let archives = data.path().join("archives").join("t3");
    let started = fs::read_dir(archives).unwrap().next().unwrap().unwrap().path();
    assert!(!started.join("index.md").exists());
    let stop = h.state().t3.export_stop_flag();
    let release = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        hold.store(false, Ordering::Relaxed);
        assert!(stop.load(Ordering::Relaxed));
    });
    eframe::App::on_exit(h.state_mut(), None);
    release.join().unwrap();
    let folder = h.state().t3.archive_folder().expect("archive after shutdown");
    let index = fs::read_to_string(folder.join("index.md")).unwrap();
    let project = fs::read_dir(folder.join("threads")).unwrap().next().unwrap().unwrap().path();
    let files: Vec<_> = fs::read_dir(project).unwrap().map(|file| file.unwrap().path()).collect();
    assert_eq!(files.len(), 2);
    let md = files.iter().find(|path| path.extension().is_some_and(|ext| ext == "md")).unwrap();
    assert!(md.with_extension("jsonl").is_file());
    assert!(index.contains(md.file_name().unwrap().to_string_lossy().replace(' ', "%20").as_str()));
}

#[test]
fn a_failed_conversation_gives_the_partial_result() {
    let fx = fixture(true);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    start_import(&mut h, true);
    until(&mut h, "done", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "导入部分完成"));
    assert!(label_visible(
        &h,
        "已加入 1 个项目，已导出 2/3 个对话。1 个对话导出失败，原因见归档文件夹中的 errors.txt。"
    ));
    assert!(label_visible(&h, "打开归档文件夹") && label_visible(&h, "完成"));
    let archive = h.state().t3.archive_folder().expect("archive folder");
    assert!(archive.join("errors.txt").is_file());
}

#[test]
fn a_failed_export_gives_the_failure_result_and_retry_starts_over() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    // A file where the archive root must be makes the export fail as a whole.
    fs::write(data.path().join("archives"), "not a folder").unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    start_import(&mut h, true);
    until(&mut h, "done", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "导入部分完成"));
    assert!(contains_visible(&h, "已加入 1 个项目。对话导出失败："));
    assert!(label_visible(&h, "重试导出") && label_visible(&h, "完成"));
    assert!(!label_visible(&h, "打开归档文件夹"));
    fs::remove_file(data.path().join("archives")).unwrap();
    let blocker = hold_journal(data.path());
    h.get_by_role_and_label(Role::Button, "重试导出").click();
    // An export can finish with the journal locked. A repeated project import
    // would be blocked by that writer lock.
    until(&mut h, "retry finished", |a| a.t3.archive_folder().is_some());
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(label_visible(&h, "已加入 1 个项目，已导出 3 个对话。"));
    let conn = Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
    assert_eq!(projects::list(&conn).unwrap().iter().filter(|p| p.source == "t3").count(), 1);
}

#[test]
fn all_conversations_failing_also_offers_export_only_retry_after_project_import() {
    let fx = fixture(false);
    let conn = Connection::open(fx.folder.join("statev2.sqlite")).unwrap();
    conn.execute_batch("UPDATE orchestration_v2_projection_messages SET payload_json = '{malformed'").unwrap();
    conn.close().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    start_import(&mut h, true);
    until(&mut h, "all exports failed", |a| a.t3.step_name() == "done");
    assert!(label_visible(&h, "导入部分完成"));
    assert!(contains_visible(&h, "已加入 1 个项目。对话导出失败："));
    assert!(contains_visible(&h, "errors.txt"));
    assert!(label_visible(&h, "重试导出"));
}

#[test]
fn export_only_import_persists_first_import_and_leaves_the_link_available() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    open_dialog(&mut h);
    until(&mut h, "selection laid out", |a| a.t3.select_settled());
    click_in_view(&mut h, Role::Button, "全不选");
    assert_eq!(h.state().t3.picked(), [false; 3]);
    h.get_by_role_and_label(Role::Button, "开始导入").click();
    until(&mut h, "exported", |a| a.t3.archive_folder().is_some());
    assert!(label_visible(&h, "已导出 3 个对话。"));
    let conn = Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
    until(&mut h, "journal marker", |_| sa_store::t3_import::has_imported(&conn).unwrap());
    assert!(!projects::has_source(&conn, "t3").unwrap());
    drop(conn);
    drop(h);

    let mut reopened = writable(data.path(), &fx, Vec::new());
    until(&mut reopened, "welcome settled on restart", |a| a.t3.settled());
    assert_eq!(reopened.state().t3.card_state(), "hidden");
    assert!(!label_visible(&reopened, "开始导入…"));
    open_dialog(&mut reopened);
    until(&mut reopened, "quiet link opened selection", |a| a.t3.step_name() == "select");
}

#[test]
fn export_only_with_all_threads_failed_shows_failure_and_retry() {
    let fx = fixture(false);
    let conn = Connection::open(fx.folder.join("statev2.sqlite")).unwrap();
    conn.execute_batch("UPDATE orchestration_v2_projection_messages SET payload_json = '{malformed'").unwrap();
    conn.close().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut h = writable(data.path(), &fx, listed_beta(&fx));
    open_dialog(&mut h);
    until(&mut h, "selection laid out", |a| a.t3.select_settled());
    click_in_view(&mut h, Role::Button, "全不选");
    assert_eq!(h.state().t3.picked(), [false; 3]);
    h.get_by_role_and_label(Role::Button, "开始导入").click();
    until(&mut h, "export failed", |a| a.t3.step_name() == "done");
    let archive = fs::read_dir(data.path().join("archives").join("t3"))
        .unwrap().next().unwrap().unwrap().path().canonicalize().unwrap();
    assert!(archive.join("errors.txt").is_file());
    assert!(label_visible(&h, "导入失败"));
    assert!(label_visible(&h, &format!(
        "无法完成导入：没有导出任何对话，原因见 {} 中的 errors.txt。", archive.display()
    )));
    assert!(label_visible(&h, "关闭") && !label_visible(&h, "打开归档文件夹"));
    h.get_by_role_and_label(Role::Button, "重试").click();
    until(&mut h, "retry returns to selection", |a| a.t3.step_name() == "select");
}

// ---- the welcome list ------------------------------------------------------

#[test]
fn recent_projects_list_tickets_first_then_up_to_eight_with_t3_names() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let ticket_dir = work.path().join("ticket-project");
    fs::create_dir(&ticket_dir).unwrap();
    // Ten imported projects; the older ones sort after the pinned one.
    let imported: Vec<Project> = (0..10)
        .map(|i| {
            let dir = work.path().join(format!("imp-{i}"));
            fs::create_dir(&dir).unwrap();
            project(&dir, &format!("T3 项目 {i}"), "t3", i == 9, &format!("2024-03-{:02}", i + 1))
        })
        .collect();
    let mut h = writable(data.path(), &fx, imported);
    until(&mut h, "history", |a| a.history_loaded && a.t3.imported_ready());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&resolutions);
    let paint_thread = std::thread::current().id();
    h.state_mut().t3.recent_resolving = Some(Arc::new(move || {
        assert_ne!(std::thread::current().id(), paint_thread);
        count.fetch_add(1, Ordering::Relaxed);
    }));
    h.state_mut().history = vec![sa_store::TicketSummary {
        id: 1,
        goal: "goal".into(),
        cwd: ticket_dir.to_string_lossy().into_owned(),
        lead: "codex".into(),
        created: 1.0,
        state: "done".into(),
        title: None,
        pinned: false,
        archived: false,
        attention: None,
    }];
    h.state_mut().t3_history_changed();
    until(&mut h, "recent cache updated from history", |a| {
        a.t3_recent().first().is_some_and(|entry| entry.path == ticket_dir.to_string_lossy())
    });
    h.run_steps(10);
    assert_eq!(resolutions.load(Ordering::Relaxed), 1, "repaints must use the cache");
    assert!(label_visible(&h, "最近的项目"));
    // Eight entries: the ticket project plus seven imported ones.
    let shown: Vec<String> = (0..10).map(|i| format!("T3 项目 {i}")).filter(|n| label_visible(&h, n)).collect();
    assert_eq!(shown.len(), 7, "{shown:?}");
    assert!(label_visible(&h, "ticket-project"));
    assert!(label_visible(&h, "T3 项目 9"), "pinned projects come first");
    assert!(label_visible(&h, "T3 项目 8"));
    assert!(!label_visible(&h, "T3 项目 0"));
    let ticket_x = h.get_by_label("ticket-project").rect().min.x;
    let pinned_x = h.get_by_label("T3 项目 9").rect().min.x;
    assert!(ticket_x < pinned_x, "ticket projects first");

    // Clicking an imported project selects it for the next conversation.
    h.get_by_label("T3 项目 8").click();
    h.run_steps(3);
    assert_eq!(h.state().composer.project.as_deref(), Some(work.path().join("imp-8").as_path()));
}

#[test]
fn welcome_import_link_can_be_scrolled_into_view_and_clicked_at_minimum_size() {
    let fx = fixture(false);
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let seed = (0..8).map(|index| {
        let dir = work.path().join(format!("project-{index}"));
        fs::create_dir(&dir).unwrap();
        project(&dir, &format!("项目 {index} {}", "较长的项目名".repeat(6)), "manual", false, "2024-03-01")
    }).collect();
    let mut h = writable(data.path(), &fx, seed);
    h.set_size(egui::vec2(880.0, 600.0));
    h.state_mut().composer.draft = "一行输入\n".repeat(12);
    until(&mut h, "small welcome settled", |a| a.t3.settled());
    assert_eq!(h.state().t3.card_state(), "shown");
    let link = h.get_by_role_and_label(Role::Button, "从 T3 Code 导入…");
    let before = link.rect().center().y;
    link.scroll_to_me();
    h.run_steps(3);
    let link = h.get_by_role_and_label(Role::Button, "从 T3 Code 导入…");
    assert!(link.rect().center().y < before, "the welcome body must scroll the link into view");
    link.click();
    h.run_steps(3);
    assert_ne!(h.state().t3.step_name(), "closed", "the scrolled link must accept a pointer click");
    until(&mut h, "import link opened the dialog", |a| a.t3.step_name() == "select");
}
