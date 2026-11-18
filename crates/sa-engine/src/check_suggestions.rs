//! Read-only completion-check discovery. Returned argv is never executed here.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;

const MANIFEST_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSuggestion {
    pub label: String,
    pub reason: String,
    /// Complete argv, starting with the executable, relative to the project.
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionReport {
    pub suggestions: Vec<CheckSuggestion>,
    pub issues: Vec<String>,
}

/// Inspect only root manifests and known lockfile/wrapper names.
/// This does not install tools, execute scripts, or persist any settings.
pub fn inspect(project: &Path) -> Result<SuggestionReport> {
    ensure!(project.is_dir(), "项目目录不存在或无法读取");
    let mut report = SuggestionReport::default();
    if project.join("Cargo.toml").is_file() {
        let wrapper = cfg!(windows) && project.join("scripts/cargo.ps1").is_file();
        for action in ["test", "build"] {
            let mut command: Vec<String> = if wrapper {
                [
                    "powershell.exe",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    "scripts/cargo.ps1",
                ]
                .map(String::from)
                .into()
            } else {
                vec!["cargo".into()]
            };
            command.extend([action.into(), "--workspace".into()]);
            report.suggestions.push(CheckSuggestion {
                label: format!("Cargo {action}"),
                reason: if wrapper {
                    "发现 Cargo.toml 和 scripts/cargo.ps1，使用项目现有 Windows Cargo 封装检查工作区".into()
                } else {
                    "发现 Cargo.toml，建议检查 Cargo 工作区".into()
                },
                command,
            });
        }
    }
    if let Some(package) = read_package(project, &mut report.issues)? {
        suggest_scripts(project, &package, &mut report);
    }
    if report.suggestions.len() > 4 {
        report.suggestions.truncate(4);
        report
            .issues
            .push("建议最多显示 4 条，已优先保留 Cargo 检查及 package.json 的 test、lint".into());
    }
    if report.suggestions.is_empty() && report.issues.is_empty() {
        report
            .issues
            .push("未发现 Cargo.toml 或 package.json 中可建议的 test、lint、build 脚本".into());
    }
    Ok(report)
}

pub fn suggest(project: &Path) -> Result<Vec<CheckSuggestion>> {
    Ok(inspect(project)?.suggestions)
}

fn read_package(project: &Path, issues: &mut Vec<String>) -> Result<Option<Value>> {
    let path = project.join("package.json");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法读取 package.json 元数据"),
    };
    if !metadata.is_file() || metadata.len() > MANIFEST_LIMIT {
        issues.push("package.json 不是普通文件或超过 1 MiB，已跳过".into());
        return Ok(None);
    }
    let source = fs::read(&path).context("无法读取 package.json")?;
    match serde_json::from_slice::<Value>(&source) {
        Ok(package) if package.is_object() => Ok(Some(package)),
        _ => {
            issues.push("package.json 必须是有效 JSON 对象，已跳过其检查建议".into());
            Ok(None)
        }
    }
}

fn package_manager(
    project: &Path,
    package: &Value,
) -> std::result::Result<(String, String), String> {
    let mut locks = Vec::new();
    for (manager, names) in [
        ("npm", &["package-lock.json", "npm-shrinkwrap.json"][..]),
        ("pnpm", &["pnpm-lock.yaml"][..]),
        ("yarn", &["yarn.lock"][..]),
        ("bun", &["bun.lock", "bun.lockb"][..]),
    ] {
        if names.iter().any(|name| project.join(name).is_file()) {
            locks.push(manager);
        }
    }
    if locks.len() > 1 {
        return Err("发现不同包管理器的锁文件，需确认使用哪个包管理器".into());
    }
    if let Some(declaration) = package.get("packageManager") {
        let declared = declaration.as_str().and_then(|value| value.split_once('@'));
        let Some((manager, version)) = declared else {
            return Err("package.json 的 packageManager 声明无法识别，需确认包管理器".into());
        };
        if !matches!(manager, "npm" | "pnpm" | "yarn") || version.trim().is_empty() {
            return Err("packageManager 未声明受支持的 npm、pnpm 或 yarn 版本，已跳过".into());
        }
        if locks.first().is_some_and(|locked| *locked != manager) {
            return Err("packageManager 与锁文件使用不同包管理器，需确认后再选择检查".into());
        }
        return Ok((manager.into(), "package.json 的 packageManager 声明".into()));
    }
    match locks.first() {
        Some(&"bun") => Err("发现 Bun 锁文件，此处仅提供 npm、pnpm 和 yarn 检查建议".into()),
        Some(manager) => Ok(((*manager).into(), "项目根目录的包管理器锁文件".into())),
        None => Ok((
            "npm".into(),
            "package.json 未指定包管理器且无锁文件，采用 npm 默认命令".into(),
        )),
    }
}

fn suggest_scripts(project: &Path, package: &Value, report: &mut SuggestionReport) {
    let Some(scripts) = package.get("scripts").and_then(Value::as_object) else {
        report
            .issues
            .push("package.json 未声明 scripts 对象，未生成 JavaScript 检查建议".into());
        return;
    };
    let (manager, source) = match package_manager(project, package) {
        Ok(manager) => manager,
        Err(issue) => {
            report.issues.push(issue);
            return;
        }
    };
    let before = report.suggestions.len();
    for action in ["test", "lint", "build"] {
        let Some(value) = scripts.get(action) else {
            continue;
        };
        let Some(script) = value.as_str().filter(|script| !script.trim().is_empty()) else {
            report
                .issues
                .push(format!("scripts.{action} 不是非空字符串，已跳过"));
            continue;
        };
        if action == "test" && script.to_ascii_lowercase().contains("no test specified") {
            report
                .issues
                .push("scripts.test 是 npm 的 no test specified 占位脚本，已跳过".into());
            continue;
        }
        report.suggestions.push(CheckSuggestion {
            label: format!("{manager} {action}"),
            reason: format!(
                "package.json 声明了 scripts.{action}，包管理器依据为{source}；尚未执行或验证脚本"
            ),
            command: vec![manager.clone(), "run".into(), action.into()],
        });
    }
    if before == report.suggestions.len() {
        report
            .issues
            .push("package.json 没有可建议的 test、lint、build 脚本".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture(package: &str) -> tempfile::TempDir {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("package.json"), package).unwrap();
        directory
    }

    #[test]
    fn only_declared_nonempty_scripts_and_no_execution() {
        let directory = fixture(
            r#"{"scripts":{"test":"node -e \"require('fs').writeFileSync('EXECUTED','x')\"","lint":" ","dev":"vite"}}"#,
        );
        let report = inspect(directory.path()).unwrap();
        assert_eq!(report.suggestions.len(), 1);
        assert_eq!(report.suggestions[0].command, ["npm", "run", "test"]);
        assert!(!directory.path().join("EXECUTED").exists());
        assert!(!directory.path().join("node_modules").exists());
    }

    #[test]
    fn skips_placeholder_test() {
        let directory = fixture(
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1","build":"vite build"}}"#,
        );
        let report = inspect(directory.path()).unwrap();
        assert_eq!(report.suggestions.len(), 1);
        assert_eq!(report.suggestions[0].command, ["npm", "run", "build"]);
        assert!(report.issues.iter().any(|issue| issue.contains("占位")));
    }

    #[test]
    fn manager_declarations_and_lockfiles() {
        for manager in ["npm", "pnpm", "yarn"] {
            let directory = fixture(&format!(
                r#"{{"packageManager":"{manager}@1.0.0","scripts":{{"lint":"eslint ."}}}}"#
            ));
            assert_eq!(
                suggest(directory.path()).unwrap()[0].command,
                [manager, "run", "lint"]
            );
        }
        for (manager, lock) in [
            ("npm", "package-lock.json"),
            ("pnpm", "pnpm-lock.yaml"),
            ("yarn", "yarn.lock"),
        ] {
            let directory = fixture(r#"{"scripts":{"test":"test-runner"}}"#);
            fs::write(directory.path().join(lock), "").unwrap();
            assert_eq!(suggest(directory.path()).unwrap()[0].command[0], manager);
        }
    }

    #[test]
    fn conflicting_or_unsupported_managers_report_issues() {
        for declaration in [r#""npm@10""#, r#""bun@1""#, "null", r#""pnpm""#] {
            let directory = fixture(&format!(
                r#"{{"packageManager":{declaration},"scripts":{{"test":"test-runner"}}}}"#
            ));
            fs::write(directory.path().join("pnpm-lock.yaml"), "").unwrap();
            let report = inspect(directory.path()).unwrap();
            assert!(report.suggestions.is_empty());
            assert!(!report.issues.is_empty());
        }
        let directory = fixture(r#"{"scripts":{"build":"build-tool"}}"#);
        for lock in ["package-lock.json", "yarn.lock"] {
            fs::write(directory.path().join(lock), "").unwrap();
        }
        assert!(suggest(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn cargo_wrapper_is_relative_and_platform_specific_and_limit_is_four() {
        let directory =
            fixture(r#"{"scripts":{"test":"runner","lint":"linter","build":"builder"}}"#);
        fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nmembers=[]",
        )
        .unwrap();
        fs::create_dir(directory.path().join("scripts")).unwrap();
        fs::write(
            directory.path().join("scripts/cargo.ps1"),
            "throw 'must not execute'",
        )
        .unwrap();
        let report = inspect(directory.path()).unwrap();
        assert_eq!(report.suggestions.len(), 4);
        let command = &report.suggestions[0].command;
        if cfg!(windows) {
            assert_eq!(
                command,
                &[
                    "powershell.exe",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    "scripts/cargo.ps1",
                    "test",
                    "--workspace"
                ]
            );
        } else {
            assert_eq!(command, &["cargo", "test", "--workspace"]);
        }
        assert!(report.issues.iter().any(|issue| issue.contains("4 条")));
    }

    #[test]
    fn malformed_manifest_does_not_hide_cargo_suggestions() {
        let directory = fixture("invalid json");
        fs::write(directory.path().join("Cargo.toml"), "[package]").unwrap();
        let report = inspect(directory.path()).unwrap();
        assert_eq!(report.suggestions.len(), 2);
        assert!(!report.issues.is_empty());
    }

    #[test]
    fn empty_project_and_missing_directory() {
        let directory = tempdir().unwrap();
        let report = inspect(directory.path()).unwrap();
        assert!(report.suggestions.is_empty());
        assert!(!report.issues.is_empty());
        assert!(inspect(&directory.path().join("missing")).is_err());
    }
}
