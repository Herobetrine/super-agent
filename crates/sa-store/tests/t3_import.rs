use rusqlite::{Connection, params};
use sa_store::projects::{self, Project};
use sa_store::t3_import::{self, Progress, ScanError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const DB: &str = "statev2.sqlite";
const NEWEST: &str = "a1b2c3d4-main";
const ARCHIVED: &str = "b2c3d4e5-old";
const BROKEN: &str = "c3d4e5f6-broken";
const OUTPUT: &str = "COMMAND_STDOUT_MUST_NOT_LEAK";
const DIFF: &str = "DIFF_BODY_MUST_NOT_LEAK";
const TOOL_OUTPUT: &str = "DYNAMIC_RESULT_MUST_NOT_LEAK";
const REASONING: &str = "REASONING_MUST_NOT_LEAK";
const ATTACHMENT_BYTES: &str = "ATTACHMENT_BYTES_MUST_NOT_LEAK";

const FIXTURE_SCHEMA: &str = "
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

struct Fixture {
    _temp: tempfile::TempDir,
    folder: PathBuf,
    output: PathBuf,
    listed_path: PathBuf,
    new_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("T3 & # 合成");
        let output = temp.path().join("archives").join("t3");
        let listed_path = temp.path().join("existing workspace");
        let new_path = temp.path().join("new workspace");
        let missing_path = temp.path().join("missing workspace");
        fs::create_dir(&folder).unwrap();
        fs::create_dir(&listed_path).unwrap();
        fs::create_dir(&new_path).unwrap();
        fs::create_dir(folder.join("attachments")).unwrap();
        fs::write(
            folder.join("attachments").join("demo.png"),
            ATTACHMENT_BYTES,
        )
        .unwrap();
        let db = Connection::open(folder.join(DB)).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        db.execute_batch(FIXTURE_SCHEMA).unwrap();
        for (id, title, path, deleted, pinned, archived) in [
            ("p1", "NUL", &listed_path, None, Some("2024-03-04"), None),
            ("p2", "Other", &missing_path, None, None, Some("2024-03-04")),
            ("p3", "New", &new_path, None, None, None),
            (
                "p4",
                "Deleted project",
                &missing_path,
                Some("2024-03-04"),
                None,
                None,
            ),
        ] {
            db.execute(
                "INSERT INTO projection_projects VALUES (?1,?2,?3,'2024-03-01','2024-03-04',?4,?5,?6)",
                params![id, title, path.to_string_lossy(), deleted, pinned, archived],
            ).unwrap();
        }

        for (id, p, title, updated, archived, deleted) in [
            (NEWEST, "p1", "CON: a/b*?", "2024-03-07", None, None),
            (
                ARCHIVED,
                "p1",
                "Archive example",
                "2024-03-04",
                Some("2024-03-04"),
                None,
            ),
            (BROKEN, "p2", "Breakable", "2024-03-05", None, None),
            ("d4e5f6a7-empty", "p3", "  .  ", "2024-03-03", None, None),
            (
                "deleted-thread",
                "p1",
                "Deleted thread",
                "2024-03-02",
                None,
                Some("2024-03-07"),
            ),
            (
                "deleted-project-thread",
                "p4",
                "Deleted project's thread",
                "2024-03-02",
                None,
                None,
            ),
        ] {
            db.execute(
                "INSERT INTO orchestration_v2_projection_threads
                 VALUES (?1,?2,?3,'test-provider','2024-03-02T01:00:00.000Z',?4,?5,?6,?7)",
                params![id, p, title, updated, archived, deleted, json!({
                    "modelSelection": {"model": "fixture-model"},
                    "lineage": {"relationshipToParent": if archived.is_some() { Some("subagent") } else { None }},
                }).to_string()],
            ).unwrap();
        }
        for (id, role, time, body) in [
            (
                "u1",
                "user",
                "2024-03-02T01:01:00.000Z",
                json!({
                    "text": "question\nsecond line",
                    "attachments": [{"name": "discard/path/demo.png", "data": ATTACHMENT_BYTES}],
                }),
            ),
            (
                "a1",
                "assistant",
                "2024-03-02T01:05:00.000Z",
                json!({"text": "answer", "attachments": []}),
            ),
            (
                "s1",
                "system",
                "2024-03-02T01:06:00.000Z",
                json!({"text": "notice"}),
            ),
        ] {
            db.execute(
                "INSERT INTO orchestration_v2_projection_messages VALUES (?1,?2,'run',?3,?4,?5,'DUPLICATE_SEARCH_COLUMN')",
                params![id, NEWEST, role, time, body.to_string()],
            ).unwrap();
        }
        let input = format!("{}TRUNCATE_ME", "界".repeat(300));
        let mut items = vec![
            (
                "user_message",
                json!({"messageId": "u1", "textOmitted": true, "text": "DUPLICATE_USER_TEXT"}),
            ),
            (
                "command_execution",
                json!({"input": input, "output": OUTPUT, "exitCode": 0}),
            ),
            (
                "file_change",
                json!({"fileName": "src/x.rs", "additions": 12, "deletions": 3, "diffStr": DIFF}),
            ),
            (
                "file_search",
                json!({"pattern": "fixture", "results": [{"text": "SEARCH_BODY"}]}),
            ),
            (
                "web_search",
                json!({"patterns": ["fixture"], "results": []}),
            ),
            (
                "dynamic_tool",
                json!({"toolName": "test.tool", "input": {"x": 1}, "output": TOOL_OUTPUT}),
            ),
            ("reasoning", json!({"text": REASONING})),
        ];
        items.extend(
            [
                "subagent",
                "todo_list",
                "proposed_plan",
                "approval_request",
                "error",
                "compaction",
                "notification",
                "user_input_request",
                "checkpoint",
                "run_interrupt_request",
                "run_interrupt_result",
                "system_notice",
                "handoff",
                "fork",
                "thread_created",
                "secret_request",
                "link_request",
                "future_tool",
            ]
            .into_iter()
            .map(|kind| (kind, json!({}))),
        );
        items.push((
            "assistant_message",
            json!({
                "messageId": "a1", "textOmitted": true, "text": "DUPLICATE_ASSISTANT_TEXT",
            }),
        ));
        for (ordinal, (kind, body)) in items.into_iter().enumerate().rev() {
            db.execute(
                "INSERT INTO orchestration_v2_projection_turn_items
                 VALUES (?1,?2,'run',?3,?4,'completed','2024-03-02T01:02:00.000Z',?5)",
                params![
                    format!("item-{ordinal}"),
                    NEWEST,
                    i64::try_from(ordinal).unwrap(),
                    kind,
                    body.to_string()
                ],
            )
            .unwrap();
        }
        db.close().unwrap();
        Self {
            _temp: temp,
            folder,
            output,
            listed_path,
            new_path,
        }
    }

    fn store(&self) -> Connection {
        let store = Connection::open_in_memory().unwrap();
        projects::init(&store).unwrap();
        store
            .execute_batch("CREATE TABLE tickets (cwd TEXT NOT NULL)")
            .unwrap();
        store
            .execute(
                "INSERT INTO tickets VALUES (?1)",
                [self.listed_path.to_string_lossy()],
            )
            .unwrap();
        store
    }

    fn export(&self) -> t3_import::Report {
        t3_import::export_threads(&self.folder, &self.output, &AtomicBool::new(false), &|_| {})
            .unwrap()
    }
}

fn archive_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(archive_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn fingerprint(path: &Path) -> (Vec<u8>, SystemTime) {
    let bytes = fs::read(path).unwrap();
    (
        Sha256::digest(&bytes).to_vec(),
        fs::metadata(path).unwrap().modified().unwrap(),
    )
}

#[test]
fn scan_error_states_and_optional_columns() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(DB);
    let store = Connection::open_in_memory().unwrap();
    projects::init(&store).unwrap();
    assert_eq!(
        t3_import::scan(temp.path(), &store),
        Err(ScanError::NotFound)
    );
    fs::write(&path, [0xff; 128]).unwrap();
    assert!(matches!(
        t3_import::scan(temp.path(), &store),
        Err(ScanError::Unreadable(_))
    ));
    fs::remove_file(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    db.close().unwrap();
    assert_eq!(
        t3_import::scan(temp.path(), &store),
        Err(ScanError::Unsupported {
            missing: "projection_projects".into(),
        })
    );
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE projection_projects(project_id TEXT)")
        .unwrap();
    db.close().unwrap();
    assert_eq!(
        t3_import::scan(temp.path(), &store),
        Err(ScanError::Unsupported {
            missing: "projection_projects.title".into(),
        })
    );
    let db = Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE projection_projects").unwrap();
    db.execute_batch(FIXTURE_SCHEMA).unwrap();
    db.execute_batch(
        "
        ALTER TABLE projection_projects DROP COLUMN pinned_at;
        ALTER TABLE projection_projects DROP COLUMN archived_at;
        ALTER TABLE orchestration_v2_projection_threads DROP COLUMN archived_at;
        ALTER TABLE orchestration_v2_projection_messages DROP COLUMN search_text;
        INSERT INTO projection_projects(project_id,title,workspace_root,updated_at)
            VALUES ('old','Old','missing','2024-03-02');
        INSERT INTO orchestration_v2_projection_threads VALUES
            ('old-id','old','Old thread','test','2024-03-01','2024-03-02',NULL,'{}');
    ",
    )
    .unwrap();
    db.close().unwrap();
    let scan = t3_import::scan(temp.path(), &store).unwrap();
    assert_eq!(scan.threads, 1);
    assert!(!scan.projects[0].pinned);
    // Output cannot go inside the source directory, including via missing parents.
    assert!(
        t3_import::export_threads(
            temp.path(),
            &temp.path().join("archives"),
            &AtomicBool::new(false),
            &|_| {}
        )
        .is_err()
    );
    assert!(!temp.path().join("archives").exists());
    let archives = tempfile::tempdir().unwrap();
    let report = t3_import::export_threads(
        temp.path(),
        archives.path(),
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(report.exported, 1);
    let archive = fs::read_to_string(report.folder.join("projects.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&archive).unwrap()[0]["archived"],
        false
    );
}

#[test]
fn scan_and_add_only_eligible_projects() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let report = t3_import::scan(&fixture.folder, &store).unwrap();
    assert_eq!(report.folder, fixture.folder);
    assert_eq!(report.threads, 4);
    assert_eq!(report.projects.len(), 3);
    let first = report.projects.iter().find(|p| p.id == "p1").unwrap();
    assert!(first.pinned && first.exists && first.already_listed);
    let missing = report.projects.iter().find(|p| p.id == "p2").unwrap();
    assert!(!missing.exists && !missing.already_listed);
    let new = report.projects.iter().find(|p| p.id == "p3").unwrap();
    assert!(new.exists && !new.already_listed);
    assert!(!projects::has_source(&store, "t3").unwrap());
    assert_eq!(
        t3_import::import_projects(&store, &report.projects).unwrap(),
        1
    );
    assert!(projects::has_source(&store, "t3").unwrap());
    assert_eq!(
        projects::list(&store).unwrap()[0].path,
        fixture.new_path.to_string_lossy()
    );
    assert_eq!(
        t3_import::import_projects(&store, &report.projects).unwrap(),
        0
    );
    assert!(
        t3_import::scan(&fixture.folder, &store)
            .unwrap()
            .projects
            .iter()
            .any(|p| p.id == "p3" && p.already_listed)
    );
}

#[test]
fn export_layout_interleaving_and_redaction_preserve_source() {
    let fixture = Fixture::new();
    let db = fixture.folder.join(DB);
    let before = fingerprint(&db);
    let store = fixture.store();
    t3_import::scan(&fixture.folder, &store).unwrap();
    assert_eq!(fingerprint(&db), before);
    let report = fixture.export();
    assert_eq!(
        (report.exported, report.total, report.stopped),
        (4, 4, false)
    );
    assert!(report.failed.is_empty());
    assert_eq!(fingerprint(&db), before);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !fixture.folder.join(format!("{DB}{suffix}")).exists(),
            "{suffix}"
        );
    }
    let name = report.folder.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), 15);
    assert_eq!(name.as_bytes()[8], b'-');
    let files = archive_files(&report.folder);
    assert_eq!(files.len(), 10);
    let newest = files
        .iter()
        .find(|path| path.ends_with("2024-03-02-CON- a-b---a1b2c3d4.md"))
        .unwrap();
    let project_dir = newest
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!project_dir.eq_ignore_ascii_case("NUL"));
    let md = fs::read_to_string(newest).unwrap();
    let jl = fs::read_to_string(newest.with_extension("jsonl")).unwrap();
    for forbidden in [
        OUTPUT,
        DIFF,
        TOOL_OUTPUT,
        REASONING,
        ATTACHMENT_BYTES,
        "discard/path",
        "DUPLICATE_USER_TEXT",
        "DUPLICATE_ASSISTANT_TEXT",
        "DUPLICATE_SEARCH_COLUMN",
        "TRUNCATE_ME",
        "SEARCH_BODY",
    ] {
        assert!(
            !md.contains(forbidden) && !jl.contains(forbidden),
            "{forbidden}"
        );
    }
    assert!(md.contains("### 用户 · 2024-03-02T01:01:00.000Z\n\nquestion\nsecond line"));
    assert!(md.contains("附件：demo.png"));
    assert!(md.contains("> 工具：文件修改 · src/x.rs · +12 −3"));
    assert!(md.contains("### 系统 · 2024-03-02T01:06:00.000Z"));
    assert!(md.find("question").unwrap() < md.find("工具：命令").unwrap());
    assert!(md.find("工具：命令").unwrap() < md.find("### 助手").unwrap());
    let records: Vec<Value> = jl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["type"], "thread");
    assert_eq!(records[0]["model"], "fixture-model");
    assert_eq!(records[1]["attachments"], json!(["demo.png"]));
    assert_eq!(records[2]["kind"], "command_execution");
    assert_eq!(records[2]["details"]["output_bytes"], OUTPUT.len());
    assert_eq!(
        records[2]["details"]["input"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        300
    );
    let tools: Vec<_> = records.iter().filter(|r| r["type"] == "tool").collect();
    assert_eq!(tools.len(), 23);
    assert!(tools.windows(2).all(|pair| pair[0]["ordinal"].as_i64().unwrap() < pair[1]["ordinal"].as_i64().unwrap()));
    let index = fs::read_to_string(report.folder.join("index.md")).unwrap();
    assert!(index.contains("CON-%20a-b---a1b2c3d4.md"));
    assert!(index.find("a1b2c3d4.md").unwrap() < index.find("b2c3d4e5.md").unwrap());
    assert!(index.contains("已归档"));
    assert!(!index.contains("Deleted project") && !index.contains("Deleted thread"));
    assert!(
        files
            .iter()
            .any(|path| path.ends_with("2024-03-02-untitled-d4e5f6a7.md"))
    );
    let archived = files
        .iter()
        .find(|path| path.ends_with("2024-03-02-Archive example-b2c3d4e5.md"))
        .unwrap();
    assert!(fs::read_to_string(archived).unwrap().contains("已归档：是"));
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(report.folder.join("projects.json")).unwrap())
            .unwrap();
    assert_eq!(stored.as_array().unwrap().len(), 3);
    let second = fixture.export();
    assert_ne!(report.folder, second.folder);
    assert_eq!(second.exported, 4);
    assert_eq!(fs::read_to_string(newest).unwrap(), md);
    assert_eq!(fingerprint(&db), before);
}

#[test]
fn stop_and_thread_failure_preserve_complete_pairs() {
    let fixture = Fixture::new();
    let stop = AtomicBool::new(false);
    let progress = RefCell::new(Vec::new());
    let report = t3_import::export_threads(&fixture.folder, &fixture.output, &stop, &|p| {
        progress.borrow_mut().push(p);
        stop.store(true, Ordering::Relaxed);
    })
    .unwrap();
    assert_eq!(progress.into_inner(), vec![Progress { done: 1, total: 4 }]);
    assert_eq!(
        (report.exported, report.total, report.stopped),
        (1, 4, true)
    );
    assert_eq!(archive_files(&report.folder).len(), 4);
    assert!(!report.folder.join("errors.txt").exists());

    let db = Connection::open(fixture.folder.join(DB)).unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_messages VALUES
         ('malformed',?1,'run','user','2024-03-02','{malformed','ignored')",
        [BROKEN],
    )
    .unwrap();
    db.close().unwrap();
    let progress = RefCell::new(Vec::new());
    let second = t3_import::export_threads(
        &fixture.folder,
        &fixture.output,
        &AtomicBool::new(false),
        &|p| progress.borrow_mut().push(p),
    )
    .unwrap();
    assert_eq!(
        progress.into_inner(),
        (1..=4)
            .map(|done| Progress { done, total: 4 })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (second.exported, second.total, second.stopped),
        (3, 4, false)
    );
    assert_eq!(second.failed.len(), 1);
    assert_eq!(second.failed[0].0, BROKEN);
    let errors = fs::read_to_string(second.folder.join("errors.txt")).unwrap();
    assert!(errors.starts_with(BROKEN));
    assert!(!errors.contains("{malformed"));
    let files = archive_files(&second.folder);
    assert_eq!(files.len(), 9);
    assert!(
        !files
            .iter()
            .any(|path| path.to_string_lossy().contains("c3d4e5f6"))
    );
    assert_eq!(archive_files(&report.folder).len(), 4);
}

#[test]
fn reads_active_and_orphan_wal_without_modifying_source_database() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let path = fixture.folder.join(DB);
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    writer.execute(
        "UPDATE orchestration_v2_projection_threads SET deleted_at='2024-03-03' WHERE thread_id=?1",
        [ARCHIVED],
    ).unwrap();
    let before = fingerprint(&path);
    assert_eq!(t3_import::scan(&fixture.folder, &store).unwrap().threads, 3);
    assert_eq!(fixture.export().exported, 3);
    assert_eq!(fingerprint(&path), before);

    let orphan = tempfile::tempdir().unwrap();
    fs::copy(&path, orphan.path().join(DB)).unwrap();
    let wal = format!("{DB}-wal");
    fs::copy(fixture.folder.join(&wal), orphan.path().join(&wal)).unwrap();
    writer.close().unwrap();
    let db_before = fingerprint(&orphan.path().join(DB));
    let wal_before = fingerprint(&orphan.path().join(&wal));
    assert_eq!(t3_import::scan(orphan.path(), &store).unwrap().threads, 3);
    let report = t3_import::export_threads(
        orphan.path(),
        &fixture.output,
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(report.exported, 3);
    assert_eq!(fingerprint(&orphan.path().join(DB)), db_before);
    assert_eq!(fingerprint(&orphan.path().join(wal)), wal_before);
    assert!(!orphan.path().join(format!("{DB}-shm")).exists());
}

fn project(path: &str, name: &str, pinned: bool, updated: &str) -> Project {
    Project {
        path: path.to_owned(),
        name: name.to_owned(),
        source: "t3".to_owned(),
        pinned,
        source_id: None,
        source_updated: Some(updated.to_owned()),
        added: 1.0,
    }
}

#[test]
fn projects_deduplicate_and_recent_order_respects_cap() {
    let conn = Connection::open_in_memory().unwrap();
    projects::init(&conn).unwrap();
    projects::init(&conn).unwrap();
    let first = project(r"C:\Work\Tree", "Tree", false, "2024-03-01");
    let duplicate_path = if cfg!(windows) {
        "c:/work/TREE/".to_owned()
    } else {
        first.path.clone()
    };
    let duplicate = project(&duplicate_path, "Duplicate", true, "2024-03-05");
    assert_eq!(
        projects::add(&conn, &[first.clone(), duplicate.clone()]).unwrap(),
        1
    );
    assert_eq!(projects::add(&conn, &[duplicate]).unwrap(), 0);
    assert_eq!(projects::list(&conn).unwrap(), vec![first.clone()]);
    let mut imported = vec![
        first,
        project("c:/stale", "Stale", false, "2024-01-01"),
        project("c:/new", "New", false, "2024-04-01"),
        project("c:/pinned", "Pinned", true, "2024-02-01"),
        project("c:/pinned-new", "Pinned New", true, "2024-03-01"),
    ];
    for index in 0..10 {
        imported.push(project(
            &format!("c:/filler{index}"),
            "Filler",
            false,
            "2024-01-01",
        ));
    }
    let cwds = [
        "c:/ticket".to_owned(),
        "c:/super-agent-project-1".to_owned(),
        duplicate_path,
        "c:/ticket".to_owned(),
    ];
    let recent = projects::recent(&cwds, &imported, 8);
    assert_eq!(recent.len(), 8);
    assert_eq!(recent[0].path, "c:/ticket");
    assert_eq!(recent[0].name, None);
    assert_eq!(recent[1].name.as_deref(), Some("Tree"));
    assert_eq!(recent[2].name.as_deref(), Some("Pinned New"));
    assert_eq!(recent[3].name.as_deref(), Some("Pinned"));
    assert_eq!(recent[4].name.as_deref(), Some("New"));
    assert!(projects::recent(&cwds, &imported, 0).is_empty());
}
