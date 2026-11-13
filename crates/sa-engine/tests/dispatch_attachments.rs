mod common;

use common::*;
use sa_engine::services::{FsSnapshotter, Snapshotter};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn attachment_directories_and_parent_traversal_remain_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let objects = temporary.path().join("objects");
    std::fs::create_dir_all(project.join("docs")).unwrap();
    std::fs::write(temporary.path().join("outside.txt"), b"outside").unwrap();
    for name in ["docs", "../outside.txt"] {
        assert!(FsSnapshotter.snapshot(project.clone(), vec![name.into()], objects.clone()).await.is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_output_attachment_fails_then_corrected_dispatch_preserves_input_snapshot() {
    let invalid = Arc::new(AtomicBool::new(true));
    let choice = invalid.clone();
    let fx = Fx::new(
        script(move |c| {
            let invalid = choice.load(Ordering::SeqCst);
            async move {
                if c.depth() == 0 {
                    if c.turn() > 1 {
                        return Ok("received child report".into());
                    }
                    let files = if invalid { vec!["docs/new-plan.md"] } else { vec!["input.txt"] };
                    let command = send(json!({
                        "body": "Create docs/new-plan.md using input.txt",
                        "write": true,
                        "files": files
                    }));
                    if !invalid {
                        c.emit("delta", json!(format!("{command}\n")));
                    }
                    return Ok(command);
                }
                assert!(c.req.writable);
                assert_eq!(c.packet["assignment"], "Create docs/new-plan.md using input.txt");
                let file = &c.packet["files"][0];
                assert_eq!(file["name"], "input.txt");
                assert_eq!(file["bytes"], 14);
                assert_eq!(file["sha256"], sha(b"existing input"));
                assert_eq!(std::fs::read(file["object_path"].as_str().unwrap()).unwrap(), b"existing input");
                Ok("verified input attachment".into())
            }
        }),
        t1_catalog(),
    );
    std::fs::create_dir(fx.project.join("docs")).unwrap();
    std::fs::write(fx.project.join("input.txt"), b"existing input").unwrap();
    let failed = fx.begin(fx.params().profile("full")).await;
    fx.finish(failed, "failed").await;
    assert_eq!(fx.int("SELECT COUNT(*) FROM jobs WHERE parent IS NOT NULL"), 0);
    assert_eq!(fx.int("SELECT COUNT(*) FROM events WHERE kind='send'"), 0);
    let result = &fx.all("SELECT result FROM jobs")[0]["result"];
    let message = result.as_str().unwrap();
    assert!(message.contains("附件不存在"), "{message}");
    assert!(message.contains("docs/new-plan.md"), "{message}");
    assert!(message.contains("body"), "{message}");
    assert!(!fx.project.join("docs/new-plan.md").exists());

    invalid.store(false, Ordering::SeqCst);
    let corrected = fx.begin(fx.params().profile("full")).await;
    fx.finish(corrected, "done").await;
    assert_eq!(fx.int("SELECT COUNT(*) FROM jobs WHERE parent IS NOT NULL"), 1);
    assert_eq!(fx.int("SELECT COUNT(*) FROM events WHERE kind='send'"), 1);
    assert!(!fx.project.join("docs/new-plan.md").exists());
    fx.engine.close().await;
}
