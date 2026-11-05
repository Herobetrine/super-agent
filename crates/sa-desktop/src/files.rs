//! Project file index for `@` mentions in the composer, drag-and-drop of
//! files and pasted clipboard images (the Electron `files` search and
//! attachment picker).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Files indexed per project at most.
pub const CAP: usize = 20_000;

/// Relative paths (`/`-separated) of the project's files, honoring
/// `.gitignore`, `.ignore` and git excludes; `.git` and hidden folders skipped.
pub fn scan(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .build()
    {
        let Ok(e) = entry else { continue };
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(rel) = e.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
        if out.len() >= CAP {
            break;
        }
    }
    out.sort();
    Ok(out)
}

/// Best matches for `query`: basename prefix, then basename substring, then
/// path substring; shorter paths first within a rank.
pub fn search<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, usize, &str)> = files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            let base = lower.rsplit('/').next().unwrap_or(&lower);
            let rank = if q.is_empty() || base.starts_with(&q) {
                0
            } else if base.contains(&q) {
                1
            } else if lower.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, f.len(), f.as_str()))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().take(limit).map(|(_, _, f)| f).collect()
}

/// The `@query` being typed at the end of `draft`, if any: returns the byte
/// offset of `@` and the query text.
pub fn mention(draft: &str) -> Option<(usize, &str)> {
    let start = draft.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + draft[i..].chars().next().map_or(1, char::len_utf8));
    let token = &draft[start..];
    let q = token.strip_prefix('@')?;
    (!q.contains('@')).then_some((start, q))
}

/// Save the clipboard image (if any) as a PNG under
/// `<project>/.superagent/pasted/` and return its project-relative path.
pub fn paste_image(project: &Path) -> Result<Option<String>> {
    let mut clipboard = arboard::Clipboard::new().context("无法读取剪贴板")?;
    let Ok(img) = clipboard.get_image() else {
        return Ok(None);
    };
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 || w > 16_384 || h > 16_384 {
        bail!("剪贴板图片尺寸无效");
    }
    let Some(buf) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) else {
        bail!("剪贴板图片格式无效");
    };
    let dir = project.join(".superagent").join("pasted");
    std::fs::create_dir_all(&dir)?;
    let name = format!("paste-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f"));
    let path: PathBuf = dir.join(&name);
    buf.save_with_format(&path, image::ImageFormat::Png).context("无法保存粘贴的图片")?;
    Ok(Some(format!(".superagent/pasted/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_honors_gitignore_and_search_ranks_basenames() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join(".gitignore"), "target/\n").unwrap();
        for f in ["src/main.rs", "src/deep/main_helper.rs", "target/main.o", "README.md", "src/domain.rs"] {
            std::fs::write(d.path().join(f), "x").unwrap();
        }
        let files = scan(d.path()).unwrap();
        assert!(!files.iter().any(|f| f.starts_with("target/")), "{files:?}");
        assert_eq!(search(&files, "main", 5), vec!["src/main.rs", "src/deep/main_helper.rs", "src/domain.rs"]);
        assert_eq!(search(&files, "readme", 5), vec!["README.md"]);
        assert_eq!(search(&files, "deep/", 5), vec!["src/deep/main_helper.rs"]);
    }

    /// `SA_CLIPBOARD_TEST=1`: puts a small image on the real clipboard,
    /// pastes it into a temp project, then restores the previous text.
    #[test]
    fn pasted_clipboard_image_is_saved_as_png() {
        if std::env::var("SA_CLIPBOARD_TEST").as_deref() != Ok("1") {
            return;
        }
        let mut cb = arboard::Clipboard::new().unwrap();
        let previous = cb.get_text().ok();
        let pixels: Vec<u8> = (0..4 * 3 * 2).map(|i| (i * 10) as u8).collect();
        cb.set_image(arboard::ImageData { width: 3, height: 2, bytes: pixels.into() }).unwrap();
        let d = tempfile::tempdir().unwrap();
        let rel = paste_image(d.path()).unwrap().expect("image on clipboard");
        if let Some(t) = previous {
            let _ = cb.set_text(t);
        }
        eprintln!("pasted {rel}");
        let img = image::open(d.path().join(&rel)).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        assert!(rel.starts_with(".superagent/pasted/paste-") && rel.ends_with(".png"));
    }

    #[test]
    fn mention_is_the_last_token() {
        assert_eq!(mention("看一下 @src/ma"), Some((10, "src/ma")));
        assert_eq!(mention("@"), Some((0, "")));
        assert_eq!(mention("a@b"), None);
        assert_eq!(mention("@x done"), None);
        assert_eq!(mention("邮件 me@x"), None);
    }
}
