//! Management API checks (X:108-255) on a temporary journal.

use super::*;
use std::fs;

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ext: ProjectExtensions,
    exe: String,
}

fn fx() -> Fx {
    let dir = tempfile::Builder::new()
        .prefix("sa-ext-unit-")
        .tempdir()
        .unwrap();
    let root = dir.path().to_path_buf();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(if cfg!(windows) {
        "server.exe"
    } else {
        "server"
    });
    fs::write(&exe, b"").unwrap();
    let opts = ExtOptions {
        homes: SkillHomes {
            home: root.join("home"),
            codex_home: root.join("codex"),
        },
        path: Some(std::env::join_paths([&bin]).unwrap()),
    };
    let ext = ProjectExtensions::open_with(root.join("state"), opts).unwrap();
    let exe = sa_engine::services::realpath(&exe)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    Fx {
        _dir: dir,
        root,
        ext,
        exe,
    }
}

fn row(ext: &ProjectExtensions, id: &str) -> Map<String, Value> {
    ext.inner
        .db
        .with(|q| q.one("SELECT * FROM studio_servers WHERE id=?", &[&id]))
        .unwrap()
        .unwrap()
}

fn set_tools(ext: &ProjectExtensions, id: &str, tools: Value) {
    ext.inner
        .db
        .with(|q| {
            q.run(
                "UPDATE studio_servers SET tools=? WHERE id=?",
                &[&stringify(&tools), &id],
            )
        })
        .unwrap();
}

fn skill(dir: &Path, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: about {name}\n---\nbody of {name}\n"),
    )
    .unwrap();
}

#[test]
fn save_server_validates_like_the_js() {
    let f = fx();
    let err = |v: Value| f.ext.save_server(&v).unwrap_err().to_string();
    let bad = "MCP 服务参数无效";
    assert_eq!(err(json!({"command": "server"})), bad);
    assert_eq!(err(json!({"name": " ", "command": "server"})), bad);
    assert_eq!(
        err(json!({"name": "x".repeat(101), "command": "server"})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "enabled": 1})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "enabled": null})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "args": null})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "args": [1]})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "args": ["\0"]})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "args": vec!["a"; 65]})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "command": "server", "args": ["x".repeat(8193)]})),
        bad
    );
    assert_eq!(
        err(json!({"name": "a", "transport": "ws"})),
        "不支持的 MCP 传输"
    );
    assert_eq!(
        err(json!({"name": "a", "command": "missing"})),
        "未找到可执行程序，请填写已安装程序的绝对路径"
    );
    assert_eq!(
        err(json!({"name": "a", "transport": "http", "url": "http://remote.invalid/mcp"})),
        validate::BAD_ENDPOINT
    );
    assert_eq!(
        err(
            json!({"name": "a", "transport": "http", "url": "https://x.test/mcp", "tokenEnv": "GITHUB_TOKEN"})
        ),
        "请使用 MCP_ 开头的服务专用环境变量名"
    );
    assert_eq!(
        err(json!({"id": "gone", "name": "a", "command": "server"})),
        "此服务已删除"
    );

    // stdio: the PATH program resolves to its real path; http ignores command.
    let id = f
        .ext
        .save_server(&json!({"name": "a", "command": "server", "args": ["--x"]}))
        .unwrap();
    let r = row(&f.ext, &id);
    assert_eq!(r["command"], json!(f.exe));
    assert_eq!(r["args"], json!(r#"["--x"]"#));
    assert_eq!(r["enabled"], json!(0));
    assert_eq!(r["transport"], json!("stdio"));
    let web = f
        .ext
        .save_server(&json!({"name": "w", "transport": "http", "url": "https://x.test/mcp", "command": "ignored", "tokenEnv": "MCP_X", "enabled": true}))
        .unwrap();
    let r = row(&f.ext, &web);
    assert_eq!(
        (
            r["command"].as_str(),
            r["url"].as_str(),
            r["token_env"].as_str()
        ),
        (Some(""), Some("https://x.test/mcp"), Some("MCP_X"))
    );
}

#[test]
fn revisions_tools_and_defaults_follow_launch_parameters() {
    let f = fx();
    let base = json!({"name": "a", "command": f.exe, "args": ["1"], "enabled": true});
    let id = f.ext.save_server(&base).unwrap();
    let rev = row(&f.ext, &id)["revision"].clone();
    set_tools(
        &f.ext,
        &id,
        json!([{"name": "t", "description": "", "inputSchema": {}}]),
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": [id, id]}), None)
        .unwrap();
    assert_eq!(
        f.ext.defaults().unwrap(),
        json!({"skills": [], "servers": [id]})
    );

    // Renaming keeps revision, tools and defaults.
    let mut renamed = base.clone();
    renamed["id"] = json!(id);
    renamed["name"] = json!("b");
    assert_eq!(f.ext.save_server(&renamed).unwrap(), id);
    let r = row(&f.ext, &id);
    assert_eq!(r["revision"], rev);
    assert!(r["tools"].is_string());
    assert_eq!(f.ext.defaults().unwrap()["servers"], json!([id]));

    // Disabling issues a new revision, keeps tools, drops the default.
    let mut off = renamed.clone();
    off["enabled"] = json!(false);
    f.ext.save_server(&off).unwrap();
    let r = row(&f.ext, &id);
    assert_ne!(r["revision"], rev);
    assert!(r["tools"].is_string());
    assert_eq!(f.ext.defaults().unwrap()["servers"], json!([]));

    // Changing arguments discards the probed tools.
    let mut changed = renamed.clone();
    changed["args"] = json!(["2"]);
    f.ext.save_server(&changed).unwrap();
    assert!(row(&f.ext, &id)["tools"].is_null());
    let err = f
        .ext
        .set_defaults(&json!({"skills": [], "servers": [id]}), None)
        .unwrap_err();
    assert_eq!(err.to_string(), "请先启用服务并连接读取工具");

    let listed = f.ext.list(None).unwrap();
    assert_eq!(
        listed["servers"][0],
        json!({"id": id, "name": "b", "command": f.exe, "args": ["2"], "enabled": true, "tools": null,
            "transport": "stdio", "url": "", "tokenEnv": "", "scopeRoot": ""})
    );
    set_tools(&f.ext, &id, json!([{"name": "t1"}, {"name": "t2"}]));
    assert_eq!(
        f.ext.list(None).unwrap()["servers"][0]["tools"],
        json!(["t1", "t2"])
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": [id]}), None)
        .unwrap();
    f.ext.remove(&id).unwrap();
    assert_eq!(f.ext.list(None).unwrap()["servers"], json!([]));
    assert_eq!(
        f.ext.defaults().unwrap(),
        json!({"skills": [], "servers": []})
    );
}

#[test]
fn skills_import_preview_prepare_and_forget() {
    let f = fx();
    let project = f.root.join("project");
    skill(&project.join(".claude/skills/local"), "local");
    skill(&f.root.join("home/.agents/skills/mine"), "mine");
    let outside = f.root.join("outside");
    skill(&outside, "outside");
    let imported = f.ext.import_skill(&outside).unwrap();
    assert!(f.ext.import_skill(&f.root.join("nothing")).is_err());

    let listed = f.ext.list(Some(&project)).unwrap();
    let names: Vec<&str> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mine", "local", "outside"]);
    assert_eq!(listed["skills"][2]["id"], json!(imported));
    assert_eq!(listed["skills"][2]["imported"], json!(true));
    assert!(listed["skills"][0].get("body").is_none());
    let local = listed["skills"][1]["id"].as_str().unwrap().to_string();

    assert_eq!(
        f.ext.preview(&imported, None).unwrap(),
        json!({"name": "outside", "body": "---\nname: outside\ndescription: about outside\n---\nbody of outside\n", "source": "已导入"})
    );
    assert_eq!(
        f.ext.preview(&local, None).unwrap_err().to_string(),
        "技能不存在"
    );
    assert_eq!(
        f.ext.preview(&local, Some(&project)).unwrap()["source"],
        json!("项目 Claude")
    );

    // Project skills need the project; defaults are checked against cwd.
    let pick = json!({"skills": [local], "servers": []});
    assert_eq!(
        f.ext.set_defaults(&pick, None).unwrap_err().to_string(),
        "所选技能在当前项目不可用，请重新选择"
    );
    f.ext.set_defaults(&pick, Some(&project)).unwrap();
    let scope = f.ext.prepare_scope(None, Some(&project)).unwrap();
    let body = "---\nname: local\ndescription: about local\n---\nbody of local\n";
    let s = &scope["skills"][0];
    assert_eq!(s["id"], json!(local));
    assert_eq!(s["body"], json!(body));
    assert_eq!(s["sha256"], json!(sa_engine::protocol::digest(body)));
    assert!(s["source"].as_str().unwrap().ends_with("SKILL.md"));
    assert_eq!(
        s.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["id", "name", "source", "body", "sha256"]
    );
    assert_eq!(scope["servers"], json!([]));
    assert_eq!(
        f.ext
            .prepare_scope(Some(&json!({"skills": "x", "servers": []})), None)
            .unwrap_err()
            .to_string(),
        "扩展授权格式无效"
    );

    f.ext
        .set_defaults(&json!({"skills": [imported], "servers": []}), None)
        .unwrap();
    f.ext.forget_skill(&imported).unwrap();
    assert_eq!(f.ext.defaults().unwrap()["skills"], json!([]));
    assert_eq!(
        f.ext.list(None).unwrap()["skills"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn opening_is_idempotent_and_keeps_the_js_layout() {
    let f = fx();
    let again = ProjectExtensions::open(f.root.join("state")).unwrap();
    let cols = again
        .inner
        .db
        .with(|q| q.all("SELECT name FROM pragma_table_info('studio_servers')", &[]))
        .unwrap();
    let cols: Vec<&str> = cols.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        cols,
        [
            "id",
            "name",
            "command",
            "args",
            "enabled",
            "revision",
            "tools",
            "transport",
            "url",
            "token_env",
            "scope_root"
        ]
    );
    assert_eq!(
        again.summary(1).unwrap(),
        json!({"skills": [], "servers": []})
    );
    assert_eq!(again.results_of(1).unwrap(), Vec::<Value>::new());
    // A journal that never had the extension tables (read-only observers).
    let bare = Connection::open_in_memory().unwrap();
    assert_eq!(
        summary_in(&bare, 1).unwrap(),
        json!({"skills": [], "servers": []})
    );
}

#[test]
fn directory_boundaries_and_closest_complete_defaults() {
    let f = fx();
    let project = f.root.join("project");
    let child = project.join("nested");
    let grandchild = child.join("deeper");
    let sibling = f.root.join("project-other");
    let worktree = f.root.join("external-worktree");
    for path in [&grandchild, &sibling, &worktree] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}/.git/worktrees/external", project.display()),
    )
    .unwrap();
    let global = f
        .ext
        .save_server(&json!({"name": "global", "command": f.exe, "enabled": true}))
        .unwrap();
    let base = json!({"name": "project", "command": f.exe, "enabled": true, "scopeRoot": project.join("nested/..")});
    let scoped = f.ext.save_server(&base).unwrap();
    for id in [&global, &scoped] {
        set_tools(&f.ext, id, json!([{"name": "t"}]));
    }
    let expected_root = directory::real_directory(&project).unwrap();
    assert_eq!(
        row(&f.ext, &scoped)["scope_root"],
        expected_root.to_string_lossy().as_ref()
    );
    assert_eq!(
        f.ext.list(None).unwrap()["servers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.ext.list(Some(&grandchild)).unwrap()["servers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for outside in [&sibling, &worktree] {
        assert_eq!(
            f.ext.list(Some(outside)).unwrap()["servers"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            f.ext
                .prepare_scope(
                    Some(&json!({"skills": [], "servers": [scoped]})),
                    Some(outside)
                )
                .is_err()
        );
    }
    assert!(
        f.ext
            .set_defaults(&json!({"skills": [], "servers": [scoped]}), None)
            .is_err()
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": [global]}), None)
        .unwrap();
    f.ext
        .set_defaults(&json!({"skills": [], "servers": [scoped]}), Some(&project))
        .unwrap();
    assert_eq!(f.ext.defaults().unwrap()["servers"], json!([global]));
    assert_eq!(
        f.ext.defaults_for(Some(&grandchild)).unwrap()["servers"],
        json!([scoped])
    );
    assert_eq!(
        f.ext.prepare_scope(None, Some(&grandchild)).unwrap()["servers"][0]["id"],
        scoped
    );
    assert_eq!(
        f.ext.defaults_for(Some(&sibling)).unwrap()["servers"],
        json!([global])
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": []}), Some(&child))
        .unwrap();
    assert_eq!(
        f.ext.defaults_for(Some(&grandchild)).unwrap()["servers"],
        json!([])
    );
    assert_eq!(
        f.ext.list(Some(&grandchild)).unwrap()["defaults"]["servers"],
        json!([])
    );
    // Even a legacy/global selection which predates directory scoping cannot leak.
    f.ext
        .inner
        .db
        .with(|q| {
            q.run(
                "UPDATE studio_extension_defaults SET selected=?",
                &[&stringify(&json!({"skills": [], "servers": [scoped]}))],
            )
        })
        .unwrap();
    assert_eq!(
        f.ext.prepare_scope(None, Some(&sibling)).unwrap()["servers"],
        json!([])
    );
    assert_eq!(f.ext.defaults_for(None).unwrap()["servers"], json!([]));
    for bad in [json!(123), json!(f.root.join("missing")), json!(f.exe)] {
        let mut invalid = base.clone();
        invalid["scopeRoot"] = bad;
        assert!(f.ext.save_server(&invalid).is_err());
    }
}

#[test]
fn changing_directory_scope_revokes_and_clears_every_default() {
    let f = fx();
    let first = f.root.join("first");
    let second = f.root.join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let mut config = json!({"name": "server", "command": f.exe, "enabled": true});
    let id = f.ext.save_server(&config).unwrap();
    config["id"] = json!(id);
    set_tools(&f.ext, &id, json!([{"name": "t"}]));
    let selected = json!({"skills": [], "servers": [id]});
    for root in [None, Some(first.as_path()), Some(second.as_path())] {
        f.ext.set_defaults(&selected, root).unwrap();
    }
    let old_revision = row(&f.ext, &id)["revision"].clone();
    config["scopeRoot"] = json!(first);
    f.ext.save_server(&config).unwrap();
    assert_ne!(row(&f.ext, &id)["revision"], old_revision);
    assert!(row(&f.ext, &id)["tools"].is_string());
    for root in [None, Some(first.as_path()), Some(second.as_path())] {
        assert_eq!(f.ext.defaults_for(root).unwrap()["servers"], json!([]));
    }
    f.ext.set_defaults(&selected, Some(&first)).unwrap();
    config["enabled"] = json!(false);
    f.ext.save_server(&config).unwrap();
    assert_eq!(
        f.ext.defaults_for(Some(&first)).unwrap()["servers"],
        json!([])
    );
    config["enabled"] = json!(true);
    f.ext.save_server(&config).unwrap();
    f.ext.set_defaults(&selected, Some(&first)).unwrap();
    f.ext.remove(&id).unwrap();
    assert_eq!(
        f.ext.defaults_for(Some(&first)).unwrap()["servers"],
        json!([])
    );
}

#[test]
fn original_server_schema_migrates_as_global_without_losing_selections() {
    let f = fx();
    f.ext.inner.db.with(|q| {
        q.conn().execute_batch("DROP TABLE studio_servers;
            CREATE TABLE studio_servers(id TEXT PRIMARY KEY,name TEXT NOT NULL,command TEXT NOT NULL,args TEXT NOT NULL,
            enabled INTEGER NOT NULL,revision TEXT NOT NULL,tools TEXT);
            INSERT INTO studio_servers VALUES('legacy','old','command','[]',1,'revision','[]');
            INSERT INTO studio_extension_defaults VALUES(1,'{\"skills\":[],\"servers\":[\"legacy\"]}');")?;
        Ok(())
    }).unwrap();
    let reopened =
        ProjectExtensions::open_with(f.root.join("state"), f.ext.inner.opts.clone()).unwrap();
    assert_eq!(reopened.list(None).unwrap()["servers"][0]["scopeRoot"], "");
    assert_eq!(reopened.defaults().unwrap()["servers"], json!(["legacy"]));
    assert_eq!(
        reopened.prepare_scope(None, Some(&f.root)).unwrap()["servers"][0]["id"],
        "legacy"
    );
}

#[test]
fn ancestor_override_inherits_only_its_selected_project_skills() {
    let f = fx();
    let project = f.root.join("project");
    let child = project.join("child");
    let sibling = f.root.join("project-sibling");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(&sibling).unwrap();
    skill(&project.join(".agents/skills/chosen"), "chosen");
    skill(&project.join(".agents/skills/unselected"), "unselected");
    let entries = f.ext.list(Some(&project)).unwrap();
    let chosen = entries["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "chosen")
        .unwrap()["id"]
        .clone();
    let unselected = entries["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "unselected")
        .unwrap()["id"]
        .clone();
    let server = f
        .ext
        .save_server(
            &json!({"name": "scoped", "command": f.exe, "enabled": true, "scopeRoot": project}),
        )
        .unwrap();
    set_tools(&f.ext, &server, json!([{"name": "t"}]));
    let selected = json!({"skills": [chosen], "servers": [server]});
    assert!(f.ext.prepare_scope(Some(&selected), Some(&child)).is_err());
    f.ext.set_defaults(&selected, Some(&project)).unwrap();
    let prepared = f.ext.prepare_scope(None, Some(&child)).unwrap();
    assert_eq!(prepared["skills"][0]["id"], chosen);
    assert_eq!(prepared["servers"][0]["id"], server);
    assert_eq!(
        f.ext.list(Some(&child)).unwrap()["skills"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        f.ext
            .prepare_scope(
                Some(&json!({"skills": [unselected], "servers": []})),
                Some(&child)
            )
            .is_err()
    );
    assert_eq!(
        f.ext.prepare_scope(None, Some(&sibling)).unwrap()["skills"],
        json!([])
    );
    assert!(
        f.ext
            .prepare_scope(Some(&selected), Some(&sibling))
            .is_err()
    );
    f.ext.set_defaults(&selected, Some(&child)).unwrap();
    assert_eq!(
        f.ext.prepare_scope(None, Some(&child)).unwrap()["skills"][0]["id"],
        chosen
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": []}), Some(&project))
        .unwrap();
    assert_eq!(
        f.ext.prepare_scope(None, Some(&child)).unwrap()["skills"][0]["id"],
        chosen
    );
    f.ext
        .set_defaults(&json!({"skills": [], "servers": []}), Some(&child))
        .unwrap();
    assert!(f.ext.prepare_scope(Some(&selected), Some(&child)).is_err());
}

#[test]
fn real_paths_prevent_link_and_parent_directory_escapes() {
    let f = fx();
    let project = f.root.join("project");
    let child = project.join("child");
    let outside = f.root.join("outside");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(&outside).unwrap();
    let escape = project.join("escape");
    let alias = f.root.join("alias");
    for (link, target) in [(&escape, &outside), (&alias, &project)] {
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success()
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    let config = json!({"name": "scoped", "command": f.exe, "enabled": true, "scopeRoot": alias});
    let server = f.ext.save_server(&config).unwrap();
    set_tools(&f.ext, &server, json!([{"name": "t"}]));
    let selected = json!({"skills": [], "servers": [server]});
    assert!(
        f.ext
            .prepare_scope(Some(&selected), Some(&alias.join("child")))
            .is_ok()
    );
    for cwd in [&escape, &project.join("child/../../outside")] {
        assert_eq!(f.ext.list(Some(cwd)).unwrap()["servers"], json!([]));
        assert!(f.ext.prepare_scope(Some(&selected), Some(cwd)).is_err());
    }
    #[cfg(windows)]
    {
        let upper = PathBuf::from(project.to_string_lossy().to_uppercase());
        assert!(f.ext.prepare_scope(Some(&selected), Some(&upper)).is_ok());
        let revision = row(&f.ext, &server)["revision"].clone();
        let mut equivalent = config;
        equivalent["id"] = json!(server);
        equivalent["scopeRoot"] = json!(upper);
        f.ext.save_server(&equivalent).unwrap();
        assert_eq!(row(&f.ext, &server)["revision"], revision);
        fs::remove_dir(&escape).unwrap();
        fs::remove_dir(&alias).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn distinct_case_sensitive_directories_keep_separate_grants() {
    let f = fx();
    let parent = f.root.join("case-sensitive");
    fs::create_dir(&parent).unwrap();
    let enabled = std::process::Command::new("fsutil.exe")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(&parent)
        .arg("enable")
        .output()
        .unwrap();
    if !enabled.status.success() {
        eprintln!("NTFS case-sensitive directory test unavailable on this host");
        return;
    }
    let upper = parent.join("A");
    let lower = parent.join("a");
    fs::create_dir(&upper).unwrap();
    fs::create_dir(&lower).unwrap();
    let upper = directory::real_directory(&upper).unwrap();
    let lower = directory::real_directory(&lower).unwrap();
    assert_ne!(upper, lower);
    let server = f
        .ext
        .save_server(
            &json!({"name": "scoped", "command": f.exe, "enabled": true, "scopeRoot": upper}),
        )
        .unwrap();
    set_tools(&f.ext, &server, json!([{"name": "t"}]));
    let selected = json!({"skills": [], "servers": [server]});
    f.ext.set_defaults(&selected, Some(&upper)).unwrap();
    assert_eq!(
        f.ext.defaults_for(Some(&lower)).unwrap()["servers"],
        json!([])
    );
    assert_eq!(f.ext.list(Some(&lower)).unwrap()["servers"], json!([]));
    assert!(f.ext.prepare_scope(Some(&selected), Some(&lower)).is_err());
    f.ext
        .set_defaults(&json!({"skills": [], "servers": []}), Some(&lower))
        .unwrap();
    assert_eq!(
        f.ext.defaults_for(Some(&upper)).unwrap()["servers"],
        json!([server])
    );
}
