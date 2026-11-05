//! Minimal stand-in for `Engine.create` / `Engine.log` used by the JS tests.
#![allow(dead_code)]

use rusqlite::{Connection, params};
use sa_store::DB_FILE;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// A separate writable connection, like the engine process.
pub fn writer(dir: &Path) -> Connection {
    let conn = Connection::open(dir.join(DB_FILE)).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

#[derive(Clone, Copy, Debug)]
pub struct Job {
    pub ticket: i64,
    pub id: i64,
    pub turn: i64,
}

/// `engine.log(job, kind, data)`: payload is `JSON.stringify(data)`.
pub fn log(conn: &Connection, job: Job, kind: &str, data: &Value) -> i64 {
    let payload = serde_json::to_string(data).unwrap();
    let sha = format!("{:x}", Sha256::digest(payload.as_bytes()));
    conn.execute(
        "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?,?,?,?)",
        params![now(), job.ticket, job.id, job.turn, kind, payload, sha],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// `engine.create({cwd, goal, lead})` without attachments or extensions.
pub fn create_ticket(conn: &Connection, cwd: &str, goal: &str, lead: &str) -> Job {
    conn.execute(
        "INSERT INTO tickets(goal,cwd,writes,lead,created,claude_route,agent_routes,
        follow_config,permission_profile,allow_lead_routing) VALUES(?,?,1,?,?,'user','{}',1,'full',1)",
        params![goal, cwd, lead, now()],
    )
    .unwrap();
    let ticket = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(ticket,sender,agent,body,writing,depth,state) VALUES(?,'user',?,?,1,0,'queued')",
        params![ticket, lead, goal],
    )
    .unwrap();
    let job = Job {
        ticket,
        id: conn.last_insert_rowid(),
        turn: 1,
    };
    log(conn, job, "user", &Value::String(goal.into()));
    job
}
