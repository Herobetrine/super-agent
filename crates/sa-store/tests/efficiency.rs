use rusqlite::{Connection, OpenFlags};
use sa_store::efficiency::{self, Settings, Ttl};

#[test]
fn old_readonly_journal_uses_defaults_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite3");
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE old_journal(id INTEGER)").unwrap();
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(efficiency::load(&conn).unwrap(), Settings::default());
    assert!(!conn.prepare("SELECT 1 FROM sqlite_master WHERE name='studio_efficiency'").unwrap().exists([]).unwrap());
}

#[test]
fn settings_roundtrip_all_values_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    for ttl in [Ttl::Inherit, Ttl::FiveMinutes, Ttl::OneHour] {
        let settings = Settings { incremental_context: false, concise_handoffs: false, claude_cache_ttl: ttl };
        {
            let mut conn = Connection::open(&path).unwrap();
            efficiency::init(&conn).unwrap();
            let tx = conn.transaction().unwrap();
            efficiency::save(&tx, &settings).unwrap();
            tx.commit().unwrap();
        }
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(efficiency::load(&conn).unwrap(), settings);
    }
}

#[test]
fn defaults_allow_older_partial_settings_and_reject_invalid_ttl() {
    let conn = Connection::open_in_memory().unwrap();
    efficiency::init(&conn).unwrap();
    assert_eq!(efficiency::load(&conn).unwrap(), Settings::default());
    conn.execute("INSERT INTO studio_efficiency VALUES(1,?1)", [r#"{"incremental_context":false}"#]).unwrap();
    assert_eq!(efficiency::load(&conn).unwrap(), Settings { incremental_context: false, ..Default::default() });
    conn.execute("UPDATE studio_efficiency SET settings=?1", [r#"{"claude_cache_ttl":"2h"}"#]).unwrap();
    assert!(efficiency::load(&conn).is_err());
}

#[test]
fn failed_transaction_preserves_saved_settings() {
    let mut conn = Connection::open_in_memory().unwrap();
    efficiency::save(&conn, &Settings::default()).unwrap();
    {
        let tx = conn.transaction().unwrap();
        efficiency::save(&tx, &Settings { incremental_context: false, ..Default::default() }).unwrap();
    }
    assert_eq!(efficiency::load(&conn).unwrap(), Settings::default());
}
