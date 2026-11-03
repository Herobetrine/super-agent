//! Journal schema, copied verbatim from `runtime/engine-schema.cjs` and the
//! `studio_plan` table created by `runtime/studio.cjs`.

pub(crate) const SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS tickets(
  id INTEGER PRIMARY KEY, goal TEXT NOT NULL, cwd TEXT NOT NULL,
  writes INTEGER NOT NULL, lead TEXT NOT NULL, created REAL NOT NULL,
  claude_route TEXT NOT NULL DEFAULT 'user');
CREATE TABLE IF NOT EXISTS jobs(
  id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,
  parent INTEGER REFERENCES jobs, sender TEXT, agent TEXT NOT NULL,
  body TEXT NOT NULL, writing INTEGER NOT NULL, depth INTEGER NOT NULL,
  turn INTEGER NOT NULL DEFAULT 1, state TEXT NOT NULL, result TEXT, session TEXT,
  source_turn INTEGER, msgkey TEXT, role TEXT NOT NULL DEFAULT '',
  assigned_role TEXT NOT NULL DEFAULT '', UNIQUE(parent,source_turn,msgkey));
CREATE TABLE IF NOT EXISTS events(
  seq INTEGER PRIMARY KEY, ts REAL NOT NULL,ticket INTEGER NOT NULL REFERENCES tickets,
  job INTEGER REFERENCES jobs, turn INTEGER,kind TEXT NOT NULL, payload TEXT NOT NULL,
  sha256 TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_ticket ON events(ticket,seq);
CREATE INDEX IF NOT EXISTS jobs_queue ON jobs(state,id);
CREATE TABLE IF NOT EXISTS job_files(job INTEGER PRIMARY KEY REFERENCES jobs,manifest TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mailbox(
  seq INTEGER PRIMARY KEY REFERENCES events(seq),target INTEGER REFERENCES jobs,
  source INTEGER REFERENCES jobs,source_turn INTEGER,msgkey TEXT,consumed_turn INTEGER,
  UNIQUE(source,source_turn,msgkey));
CREATE INDEX IF NOT EXISTS mailbox_target ON mailbox(target,consumed_turn,seq);
CREATE TABLE IF NOT EXISTS progress_receipts(
  job INTEGER NOT NULL REFERENCES jobs,source_turn INTEGER NOT NULL,msgkey TEXT NOT NULL,
  seq INTEGER NOT NULL REFERENCES events,PRIMARY KEY(job,source_turn,msgkey)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS route_defaults(singleton INTEGER PRIMARY KEY CHECK(singleton=1), routes TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS native_routes(job INTEGER PRIMARY KEY REFERENCES jobs,route TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS approvals(
  id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,job INTEGER NOT NULL REFERENCES jobs,
  turn INTEGER NOT NULL,action TEXT NOT NULL,decision TEXT NOT NULL DEFAULT 'pending');
CREATE INDEX IF NOT EXISTS approvals_pending ON approvals(id) WHERE decision='pending';
CREATE TABLE IF NOT EXISTS continuous_runs(
  ticket INTEGER PRIMARY KEY REFERENCES tickets, phase TEXT NOT NULL DEFAULT 'work',
  milestone INTEGER NOT NULL DEFAULT 0, learning TEXT NOT NULL DEFAULT '',
  boundary_job INTEGER NOT NULL DEFAULT 0, boundary_seq INTEGER NOT NULL DEFAULT 0,
  probe INTEGER NOT NULL DEFAULT 0, success INTEGER NOT NULL DEFAULT 0,
  stalls INTEGER NOT NULL DEFAULT 0, last_signal TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS jobs_ticket_id ON jobs(ticket,id);
CREATE INDEX IF NOT EXISTS jobs_ticket_root ON jobs(ticket) WHERE parent IS NULL;
CREATE INDEX IF NOT EXISTS events_ticket_kind ON events(ticket,kind,seq);"#;

/// Columns added to older databases, in the same order as `engine-schema.cjs`.
pub(crate) const ADDITIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "tickets",
        &[
            ("claude_route", "TEXT NOT NULL DEFAULT 'user'"),
            ("agent_routes", "TEXT NOT NULL DEFAULT '{}'"),
            ("permission_profile", "TEXT NOT NULL DEFAULT 'legacy'"),
            ("objective", "TEXT"),
            ("auto_cycles", "INTEGER NOT NULL DEFAULT 0"),
            ("allow_lead_routing", "INTEGER NOT NULL DEFAULT 0"),
            ("follow_config", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
    (
        "jobs",
        &[
            ("dispatch_turn", "INTEGER"),
            ("ready_after", "REAL NOT NULL DEFAULT 0"),
            ("route_override", "TEXT"),
            ("permission_override", "TEXT"),
            ("role", "TEXT NOT NULL DEFAULT ''"),
            ("assigned_role", "TEXT NOT NULL DEFAULT ''"),
        ],
    ),
];

/// Created by the (non read-only) `Studio` constructor.
pub(crate) const STUDIO_PLAN: &str = "CREATE TABLE IF NOT EXISTS studio_plan(id INTEGER PRIMARY KEY,
      ticket INTEGER NOT NULL REFERENCES tickets,body TEXT NOT NULL,done INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS studio_plan_ticket ON studio_plan(ticket,id)";
