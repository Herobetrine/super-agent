//! Port of `tests-node/studio.test.cjs`.

mod common;

use common::{create_ticket, writer};
use rusqlite::params;
use sa_store::Store;
use std::collections::HashSet;

#[test]
fn ticket_preview_and_collaboration_pages_preserve_every_task_address() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut conn = writer(dir.path());
    let root = create_ticket(
        &conn,
        &dir.path().to_string_lossy(),
        "test project",
        "deepseek",
    );
    let ticket = root.ticket;
    let tx = conn.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state) VALUES(?,?,'deepseek','codex',?,0,1,'done')",
            )
            .unwrap();
        for n in 0..207 {
            insert
                .execute(params![ticket, root.id, format!("check {n}")])
                .unwrap();
        }
    }
    tx.commit().unwrap();

    let view = store.snapshot(ticket, 0).unwrap();
    assert_eq!(view.total_jobs, 208);
    assert_eq!(view.running_peers, 0);
    assert!((view.jobs.len() as i64) < view.total_jobs);
    assert_eq!(view.jobs[0].id, root.id, "root job sorts first");

    let mut ids = HashSet::new();
    let mut before = None;
    loop {
        let page = store.jobs_page(ticket, before).unwrap();
        before = page.next;
        for job in &page.jobs {
            assert!(ids.insert(job.id), "duplicate job {}", job.id);
        }
        if !page.more || before.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 208);
    assert!(ids.contains(&root.id));
}
