mod common;

use common::{create_ticket, writer};
use rusqlite::params;
use sa_store::Store;

#[test]
fn queue_count_excludes_unmet_dependencies_beyond_the_preview() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "queue projection", "codex");
    conn.execute_batch("CREATE TABLE job_deps(job INTEGER NOT NULL,needs INTEGER NOT NULL,PRIMARY KEY(job,needs));").unwrap();
    let mut ids = Vec::new();
    for _ in 0..85 {
        conn.execute("INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state) VALUES(?,?,'lead','codex','queued fixture',0,1,'queued')", params![root.ticket,root.id]).unwrap();
        ids.push(conn.last_insert_rowid());
    }
    // A prerequisite outside the bounded view still controls the global count.
    conn.execute("UPDATE jobs SET state='running' WHERE id=?", [ids[0]]).unwrap();
    conn.execute("INSERT INTO job_deps VALUES(?,?)", params![ids[1],ids[0]]).unwrap();
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let snap = reader.snapshot(root.ticket, 0).unwrap();
    assert_eq!(snap.jobs.len(), 80);
    assert_eq!(snap.running_peers, 1);
    assert_eq!(snap.queued_peers, 83);
    conn.execute("UPDATE jobs SET state='done' WHERE id=?", [ids[0]]).unwrap();
    assert_eq!(reader.snapshot(root.ticket, snap.cursor).unwrap().queued_peers, 84);
    conn.execute("UPDATE jobs SET state='failed' WHERE id=?", [ids[0]]).unwrap();
    assert_eq!(reader.snapshot(root.ticket, snap.cursor).unwrap().queued_peers, 83);
}
