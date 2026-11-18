//! Print a ticket's usage ledger from a journal file (diagnostic; read-only use of a copy).
//! `ledger_dump <hub.sqlite3> <ticket>`
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "用法：ledger_dump <hub.sqlite3> <ticket>");
    let conn = rusqlite::Connection::open_with_flags(&args[0], rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ledger = sa_store::ledger::ticket_ledger(&conn, args[1].parse()?)?;
    println!("{}", serde_json::to_string_pretty(&ledger)?);
    Ok(())
}
