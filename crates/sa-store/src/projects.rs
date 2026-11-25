//! Projects explicitly added to the welcome page.

use anyhow::Result;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub path: String,
    pub name: String,
    pub pinned: bool,
    pub source: String,
    pub source_id: Option<String>,
    pub source_updated: Option<String>,
    pub added: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub path: String,
    pub name: Option<String>,
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS studio_projects (
            path TEXT PRIMARY KEY, name TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
            source TEXT NOT NULL, source_id TEXT, source_updated TEXT, added REAL NOT NULL
        )",
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT path, name, pinned, source, source_id, source_updated, added
         FROM studio_projects ORDER BY added, path",
    )?;
    Ok(stmt
        .query_map([], |row| {
            Ok(Project {
                path: row.get(0)?,
                name: row.get(1)?,
                pinned: row.get(2)?,
                source: row.get(3)?,
                source_id: row.get(4)?,
                source_updated: row.get(5)?,
                added: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn add(conn: &Connection, projects: &[Project]) -> Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut known = path_keys(&tx, "SELECT path FROM studio_projects")?;
    let mut added = 0;
    {
        let mut insert = tx.prepare(
            "INSERT INTO studio_projects (path, name, pinned, source, source_id, source_updated, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for p in projects {
            if known.insert(path_key(&p.path)) {
                added += insert.execute(params![
                    p.path,
                    p.name,
                    p.pinned,
                    p.source,
                    p.source_id,
                    p.source_updated,
                    p.added
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(added)
}

pub fn has_source(conn: &Connection, source: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM studio_projects WHERE source = ?1)",
        [source],
        |row| row.get(0),
    )?)
}

pub fn recent(
    ticket_cwds_newest_first: &[String],
    imported: &[Project],
    limit: usize,
) -> Vec<Recent> {
    recent_with(ticket_cwds_newest_first, imported, limit, path_key)
}

fn recent_with(
    ticket_cwds_newest_first: &[String],
    imported: &[Project],
    limit: usize,
    resolve: impl Fn(&str) -> PathBuf,
) -> Vec<Recent> {
    if limit == 0 {
        return Vec::new();
    }
    let names: HashMap<_, _> = imported
        .iter()
        .map(|p| (p.path.as_str(), &p.name))
        .collect();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut unnamed = HashMap::new();
    for path in ticket_cwds_newest_first {
        if result.len() == limit {
            break;
        }
        if path.is_empty() || normalized_path(path).contains("super-agent-project-") {
            continue;
        }
        let key = resolve(path);
        if seen.insert(key.clone()) {
            let name = names.get(path.as_str()).map(|name| (*name).clone());
            if name.is_none() && !imported.is_empty() {
                unnamed.insert(key, result.len());
            }
            result.push(Recent {
                path: path.clone(),
                name,
            });
        }
    }

    if result.len() == limit && unnamed.is_empty() {
        return result;
    }
    let mut sorted: Vec<_> = imported.iter().collect();
    sorted.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.source_updated.cmp(&a.source_updated))
    });
    for p in sorted {
        if result.len() == limit && unnamed.is_empty() {
            break;
        }
        let key = resolve(&p.path);
        if let Some(index) = unnamed.remove(&key) {
            result[index].name = Some(p.name.clone());
        }
        if result.len() < limit && seen.insert(key) {
            result.push(Recent {
                path: p.path.clone(),
                name: Some(p.name.clone()),
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn recent_does_not_resolve_candidates_past_the_cap_when_no_names_are_pending() {
        let make = |path: &str, pinned| Project {
            path: path.into(),
            name: path.into(),
            pinned,
            source: "t3".into(),
            source_id: None,
            source_updated: None,
            added: 0.0,
        };
        let imported = [make("later", false), make("first", true)];
        let resolved = RefCell::new(Vec::new());
        let resolve = |path: &str| {
            resolved.borrow_mut().push(path.to_owned());
            PathBuf::from(path)
        };
        assert_eq!(recent_with(&[], &imported, 1, resolve)[0].path, "first");
        assert_eq!(&*resolved.borrow(), &["first"]);
        resolved.borrow_mut().clear();
        assert_eq!(
            recent_with(&["first".into(), "unused".into()], &imported, 1, resolve)[0]
                .name
                .as_deref(),
            Some("first")
        );
        assert_eq!(&*resolved.borrow(), &["first"]);
    }
}

pub(crate) fn path_key(path: &str) -> PathBuf {
    if cfg!(windows) {
        // Same approach as sa-ext::directory (which depends on sa-store via
        // sa-engine). Keep the stored case in an existing directory's real path.
        match Path::new(path).canonicalize() {
            Ok(real) => return real,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return PathBuf::from(path);
            }
            _ => {}
        }
        // Keep a failed lookup separate from a resolved (verbatim) path.
        return PathBuf::from(normalized_path(path));
    }
    PathBuf::from(path)
}

pub(crate) fn normalized_path(path: &str) -> String {
    if cfg!(windows) {
        use std::path::Component;
        std::path::Path::new(path)
            .components()
            .map(|component| match component {
                Component::RootDir => String::new(),
                _ => component
                    .as_os_str()
                    .to_string_lossy()
                    .replace('/', "\\")
                    .to_lowercase(),
            })
            .collect::<Vec<_>>()
            .join("/")
    } else {
        path.to_owned()
    }
}

pub(crate) fn path_keys(conn: &Connection, sql: &str) -> Result<HashSet<PathBuf>> {
    let mut stmt = conn.prepare(sql)?;
    Ok(stmt
        .query_map([], |row| Ok(path_key(row.get_ref(0)?.as_str()?)))?
        .collect::<rusqlite::Result<_>>()?)
}
