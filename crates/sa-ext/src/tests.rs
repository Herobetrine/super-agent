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
            "transport": "stdio", "url": "", "tokenEnv": ""})
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
            "token_env"
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
