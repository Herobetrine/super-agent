//! Skill catalog (port of `runtime/skills.cjs`, SK:1-62).
//!
//! A skill is a folder whose `SKILL.md` starts with YAML front matter holding
//! `name` and `description`. Its id is the SHA-256 of the real path of the
//! entry file, so the same folder keeps its id across sources.

use anyhow::{Context, Result, bail, ensure};
use sa_engine::protocol::{digest, js_whitespace};
use sa_engine::services::realpath;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Largest accepted `SKILL.md` (SK:11).
pub const MAX_SKILL_BYTES: u64 = 128 * 1024;
pub const IMPORTED: &str = "已导入";

static FRONT_MATTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)").expect("valid regex"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Catalog label: `Codex`, `用户`, `Claude`, `项目`, `项目 Claude`, `已导入`.
    pub source: String,
    pub imported: bool,
    /// Real path of `SKILL.md`.
    pub filename: String,
    pub body: String,
}

impl Skill {
    /// Catalog row without `filename` and `body` (X:111).
    pub fn meta(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "source": self.source,
            "imported": self.imported,
        })
    }
}

/// `readSkill(folder, source, imported)` (SK:7-29).
///
/// Differences from the JS: YAML is parsed by `serde_yaml`, which bounds
/// alias expansion itself instead of rejecting every alias
/// (`maxAliasCount: 0`); real paths come from `canonicalize`, which also
/// normalizes letter case on Windows.
pub fn read_skill(folder: &Path, source: &str, imported: bool) -> Result<Skill> {
    let dir = realpath(folder).context("技能目录不存在或无法访问")?;
    let filename = realpath(&dir.join("SKILL.md")).context("技能目录中没有 SKILL.md")?;
    ensure!(filename.parent() == Some(dir.as_path()), "技能入口重定向到了其他目录");
    ensure!(filename.metadata()?.len() <= MAX_SKILL_BYTES, "技能入口超过 128 KiB");
    let raw = std::fs::read(&filename)?;
    let text = String::from_utf8_lossy(&raw);
    let body = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
    let Some(meta) = FRONT_MATTER.captures(&body) else {
        bail!("未找到 SKILL.md 的 YAML 元数据");
    };
    let header: serde_yaml::Value = serde_yaml::from_str(&meta[1]).context("SKILL.md 的 YAML 元数据无效")?;
    let (Some(serde_yaml::Value::String(name)), Some(serde_yaml::Value::String(description))) =
        (header.get("name"), header.get("description"))
    else {
        bail!("技能缺少名称或说明");
    };
    ensure!(!name.chars().all(js_whitespace), "技能缺少名称或说明");
    let filename = filename.to_string_lossy().into_owned();
    Ok(Skill {
        id: digest(&filename),
        name: name.clone(),
        description: description.clone(),
        source: source.to_string(),
        imported,
        filename,
        body,
    })
}

/// Skill search roots (SK:31-41). `codex_home` is `$CODEX_HOME` or
/// `~/.codex`.
#[derive(Clone, Debug)]
pub struct SkillHomes {
    pub home: PathBuf,
    pub codex_home: PathBuf,
}

impl SkillHomes {
    /// Roots from the process environment (`os.homedir()`, `CODEX_HOME`).
    pub fn from_process() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .filter(|_| cfg!(windows))
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Self { home, codex_home }
    }

    fn roots(&self, cwd: Option<&Path>) -> Vec<(&'static str, PathBuf)> {
        let mut roots = vec![
            ("Codex", self.codex_home.join("skills")),
            ("用户", self.home.join(".agents").join("skills")),
            ("Claude", self.home.join(".claude").join("skills")),
        ];
        if let Some(cwd) = cwd {
            roots.push(("项目", cwd.join(".agents").join("skills")));
            roots.push(("项目 Claude", cwd.join(".claude").join("skills")));
        }
        roots
    }
}

/// Ordered id → skill map with JS `Map.set` semantics (a later source
/// replaces an entry in place).
#[derive(Clone, Debug, Default)]
pub struct Catalog(Vec<Skill>);

impl Catalog {
    fn set(&mut self, skill: Skill) {
        match self.0.iter_mut().find(|s| s.id == skill.id) {
            Some(slot) => *slot = skill,
            None => self.0.push(skill),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.0.iter().find(|s| s.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.0.iter()
    }
}

/// `scan(db, cwd)` (SK:30-60): home and project roots, then the imported
/// folders (`studio_skill_dirs`). Unusable skills are skipped. Symlinked
/// folders inside a root are skipped like `Dirent.isDirectory()`.
pub fn scan(homes: &SkillHomes, cwd: Option<&Path>, imported: &[String]) -> Catalog {
    let mut out = Catalog::default();
    for (source, root) in homes.roots(cwd) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if !is_dir || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Ok(skill) = read_skill(&entry.path(), source, false) {
                out.set(skill);
            }
        }
    }
    for folder in imported {
        if let Ok(skill) = read_skill(Path::new(folder), IMPORTED, true) {
            out.set(skill);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    const GOOD: &str = "---\nname: selected-skill\ndescription: Example\n---\nKeep exact bytes.\n";

    #[test]
    fn reads_front_matter_and_hashes_the_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("selected-skill");
        write_skill(&folder, &format!("\u{feff}{GOOD}"));
        let s = read_skill(&folder, IMPORTED, true).unwrap();
        assert_eq!(s.name, "selected-skill");
        assert_eq!(s.description, "Example");
        assert_eq!(s.body, GOOD);
        let real = realpath(&folder.join("SKILL.md")).unwrap();
        assert_eq!(s.filename, real.to_string_lossy());
        assert_eq!(s.id, digest(&s.filename));
        assert_eq!(
            s.meta(),
            json!({"id": s.id, "name": "selected-skill", "description": "Example", "source": "已导入", "imported": true})
        );
        // CRLF front matter and an empty body after the closing fence.
        write_skill(&folder, "---\r\nname: x\r\ndescription: ''\r\n---");
        assert_eq!(read_skill(&folder, "用户", false).unwrap().description, "");
    }

    #[test]
    fn rejects_unusable_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("s");
        let err = |body: &str| {
            write_skill(&folder, body);
            read_skill(&folder, "用户", false).unwrap_err().to_string()
        };
        assert_eq!(err("no front matter"), "未找到 SKILL.md 的 YAML 元数据");
        assert_eq!(err("---\nname: x\n---\n"), "技能缺少名称或说明");
        assert_eq!(err("---\nname: '  '\ndescription: d\n---\n"), "技能缺少名称或说明");
        assert_eq!(err("---\nname: 3\ndescription: d\n---\n"), "技能缺少名称或说明");
        assert_eq!(err("---\n- a\n---\n"), "技能缺少名称或说明");
        assert_eq!(
            err(&format!(
                "---\nname: x\ndescription: d\n---\n{}",
                "a".repeat(128 * 1024)
            )),
            "技能入口超过 128 KiB"
        );
        assert!(read_skill(&tmp.path().join("missing"), "用户", false).is_err());
    }

    #[test]
    fn scans_homes_project_and_imports_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let homes = SkillHomes {
            home: tmp.path().join("home"),
            codex_home: tmp.path().join("codex"),
        };
        let project = tmp.path().join("project");
        let skill = |name: &str| format!("---\nname: {name}\ndescription: d\n---\n");
        write_skill(&homes.codex_home.join("skills/a"), &skill("codex-a"));
        write_skill(&homes.home.join(".agents/skills/b"), &skill("user-b"));
        write_skill(&homes.home.join(".claude/skills/c"), &skill("claude-c"));
        write_skill(&homes.home.join(".claude/skills/.hidden"), &skill("hidden"));
        write_skill(&homes.home.join(".claude/skills/broken"), "nothing");
        fs::write(homes.home.join(".claude/skills/file.md"), skill("file")).unwrap();
        write_skill(&project.join(".agents/skills/p"), &skill("project-p"));
        write_skill(&project.join(".claude/skills/q"), &skill("project-q"));
        let outside = tmp.path().join("outside");
        write_skill(&outside, &skill("imported"));

        let names = |c: &Catalog| c.iter().map(|s| (s.name.clone(), s.source.clone())).collect::<Vec<_>>();
        let global = scan(&homes, None, &[]);
        assert_eq!(
            names(&global),
            [("codex-a", "Codex"), ("user-b", "用户"), ("claude-c", "Claude")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        // A re-imported home skill keeps its position and becomes `已导入`.
        let imports = vec![
            outside.to_string_lossy().into_owned(),
            homes.home.join(".agents/skills/b").to_string_lossy().into_owned(),
            tmp.path().join("gone").to_string_lossy().into_owned(),
        ];
        let all = scan(&homes, Some(&project), &imports);
        assert_eq!(
            names(&all),
            [
                ("codex-a", "Codex"),
                ("user-b", "已导入"),
                ("claude-c", "Claude"),
                ("project-p", "项目"),
                ("project-q", "项目 Claude"),
                ("imported", "已导入"),
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        let b = all.iter().find(|s| s.name == "user-b").unwrap();
        assert!(b.imported);
        assert_eq!(all.get(&b.id), Some(b));
    }
}
