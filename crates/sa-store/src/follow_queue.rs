use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    Stopped,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedFollow {
    pub id: i64,
    pub job: i64,
    pub position: i64,
    pub body: String,
    pub files: Vec<String>,
    pub steering: bool,
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE follow_queue(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ticket INTEGER NOT NULL REFERENCES tickets, job INTEGER NOT NULL REFERENCES jobs,
            position INTEGER NOT NULL, body TEXT NOT NULL, manifest TEXT NOT NULL,
            resume INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('queued','steering')),
            note_seq INTEGER UNIQUE REFERENCES events(seq));
         CREATE INDEX follow_queue_order ON follow_queue(job,position,id);
         CREATE TABLE follow_queue_status(
            job INTEGER PRIMARY KEY REFERENCES jobs,
            hold TEXT CHECK(hold IN ('stopped','failed','interrupted')),
            awaiting_admission INTEGER NOT NULL DEFAULT 0,
            resume_after_drain INTEGER NOT NULL DEFAULT 0);",
    )?;
    Ok(())
}

pub(crate) fn migrate_assignments(conn: &Connection) -> Result<()> {
    // Widen the state constraint without reusing IDs of previously deleted
    // items. A v2 head already in the journal cannot be published again.
    conn.execute_batch(
        "CREATE TABLE follow_queue_v3(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ticket INTEGER NOT NULL REFERENCES tickets, job INTEGER NOT NULL REFERENCES jobs,
            position INTEGER NOT NULL, body TEXT NOT NULL, manifest TEXT NOT NULL,
            resume INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('queued','assigned','steering')),
            note_seq INTEGER UNIQUE REFERENCES events(seq));
         INSERT INTO follow_queue_v3 SELECT * FROM follow_queue;
         UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM sqlite_sequence
             WHERE name='follow_queue'),0)) WHERE name='follow_queue_v3';
         DROP TABLE follow_queue;
         ALTER TABLE follow_queue_v3 RENAME TO follow_queue;
         CREATE INDEX follow_queue_order ON follow_queue(job,position,id);
         CREATE UNIQUE INDEX follow_queue_assigned ON follow_queue(job) WHERE state='assigned';
         ALTER TABLE follow_queue_status RENAME COLUMN awaiting_admission TO legacy_delivered;
         ALTER TABLE follow_queue_status ADD COLUMN delivery_turn INTEGER;
         ALTER TABLE follow_queue_status ADD COLUMN release_on_admission INTEGER NOT NULL DEFAULT 0;
         UPDATE follow_queue_status SET hold=NULL,resume_after_drain=0
             WHERE NOT EXISTS(SELECT 1 FROM follow_queue f WHERE f.job=follow_queue_status.job);
         INSERT INTO follow_queue_status(job,hold)
             SELECT j.id,'failed' FROM jobs j WHERE j.parent IS NULL AND j.state='draining'
             AND EXISTS(SELECT 1 FROM follow_queue f WHERE f.job=j.id)
             ON CONFLICT(job) DO UPDATE SET hold='failed'
             WHERE hold IS NULL AND resume_after_drain=0;
         CREATE TRIGGER follow_queue_empty AFTER DELETE ON follow_queue
             WHEN NOT EXISTS(SELECT 1 FROM follow_queue WHERE job=OLD.job)
             BEGIN UPDATE follow_queue_status SET hold=NULL,resume_after_drain=0,
                 release_on_admission=0,delivery_turn=NULL WHERE job=OLD.job; END;",
    )?;
    Ok(())
}

pub(crate) fn snapshot(
    conn: &Connection,
    ticket: i64,
) -> Result<(Vec<QueuedFollow>, Option<HoldReason>)> {
    // Read the hold and the ordered rows from the same SQLite snapshot.
    let mut stmt = conn.prepare_cached(
        "SELECT s.hold,f.id,f.job,f.position,f.body,f.state='steering',
                (SELECT json_group_array(json_extract(value,'$.name')) FROM json_each(f.manifest))
         FROM jobs j LEFT JOIN follow_queue_status s ON s.job=j.id
         LEFT JOIN follow_queue f ON f.job=j.id AND f.ticket=j.ticket
         WHERE j.ticket=? AND j.parent IS NULL ORDER BY f.state='assigned' DESC,f.position,f.id",
    )?;
    let mut rows = stmt.query([ticket])?;
    let mut items = Vec::new();
    let mut hold = None;
    while let Some(row) = rows.next()? {
        if items.is_empty() {
            hold = row
                .get::<_, Option<String>>(0)?
                .map(|reason| serde_json::from_value(reason.into()))
                .transpose()?;
        }
        if let Some(id) = row.get::<_, Option<i64>>(1)? {
            items.push(QueuedFollow {
                id,
                job: row.get(2)?,
                position: row.get(3)?,
                body: row.get(4)?,
                steering: row.get(5)?,
                files: serde_json::from_str(&row.get::<_, String>(6)?)?,
            });
        }
    }
    Ok((items, hold))
}
