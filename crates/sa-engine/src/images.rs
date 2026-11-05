//! Native image inputs are read from immutable attachment objects before
//! context compaction. Original project files are never reopened here.

use anyhow::{Context, Result, bail, ensure};
use image::{ImageFormat, ImageReader, Limits};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
    sync::Arc,
};

pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub const UNSUPPORTED: &str =
    "当前执行端尚未接入原生图片输入，请选择 Codex 或 Claude Code，并使用支持视觉输入的模型";

pub fn supports_native_images(agent: &str) -> bool {
    matches!(agent, "codex" | "claude")
}

/// Used by the composer for early feedback. Image attachments are selected
/// by name and their actual format is then verified before native delivery.
pub fn is_image_name(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "avif" | "heic"
    )
}

fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

#[derive(Clone)]
pub struct ImageInput {
    name: String,
    media_type: &'static str,
    data: Arc<[u8]>,
}

impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput")
            .field("name", &self.name)
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl ImageInput {
    pub fn from_bytes(name: String, data: Vec<u8>) -> Result<Self> {
        ensure!(data.len() <= MAX_IMAGE_BYTES, "每张图片最多 5 MiB");
        let format = image::guess_format(&data).context("图片内容无法识别")?;
        let media_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            _ => bail!("原生图片输入目前只支持 PNG 和 JPEG"),
        };
        let ext = extension(&name);
        if is_image_name(&name) {
            ensure!(
                matches!(
                    (ext.as_str(), format),
                    ("png", ImageFormat::Png) | ("jpg" | "jpeg", ImageFormat::Jpeg)
                ),
                "图片文件后缀与实际格式不符"
            );
        }
        let reader = ImageReader::with_format(Cursor::new(&data), format);
        let (w, h) = reader.into_dimensions().context("图片头损坏")?;
        ensure!(
            w > 0 && h > 0 && w <= 8192 && h <= 8192 && u64::from(w) * u64::from(h) <= 16_000_000,
            "图片尺寸超过限制（单边 8192 像素、总计 1600 万像素）"
        );
        let mut reader = ImageReader::with_format(Cursor::new(&data), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        reader.decode().context("图片数据损坏或超出解码限制")?;
        Ok(Self {
            name,
            media_type,
            data: data.into(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn media_type(&self) -> &'static str {
        self.media_type
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Validate snapshot paths, sizes, digests and image data. Non-image
/// attachments retain their existing file-only behavior.
pub(crate) fn from_manifest(manifest: &Value, directory: &Path) -> Result<Vec<ImageInput>> {
    let Some(files) = manifest.as_array() else {
        return Ok(Vec::new());
    };
    if !files
        .iter()
        .any(|f| f["name"].as_str().is_some_and(is_image_name))
    {
        return Ok(Vec::new());
    }
    let root = directory.canonicalize().context("附件快照目录无法读取")?;
    let mut images = Vec::new();
    let mut total = 0usize;
    for entry in files {
        let name = entry["name"].as_str().context("附件名称缺失")?;
        if !is_image_name(name) {
            continue;
        }
        let digest = entry["sha256"].as_str().context("附件摘要缺失")?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "附件摘要无效"
        );
        let path = Path::new(entry["object_path"].as_str().context("附件快照路径缺失")?);
        let canonical = path.canonicalize().context("附件快照无法读取")?;
        ensure!(
            canonical.parent() == Some(root.as_path())
                && canonical.file_name().and_then(|s| s.to_str()) == Some(digest),
            "附件快照位于对象目录外"
        );
        let mut file = File::open(&canonical).context("附件快照无法打开")?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "每张图片最多 5 MiB");
        ensure!(
            entry["bytes"].as_u64() == Some(bytes.len() as u64),
            "图片快照长度不符"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == digest,
            "图片快照摘要校验失败"
        );
        total += bytes.len();
        ensure!(
            images.len() < MAX_IMAGES && total <= MAX_TOTAL_BYTES,
            "每轮最多 4 张图片，合计最多 16 MiB"
        );
        images.push(ImageInput::from_bytes(name.into(), bytes)?);
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn encoded(format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbImage::new(3, 2)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    fn object(dir: &Path, name: &str, data: &[u8]) -> Value {
        let digest = format!("{:x}", Sha256::digest(data));
        let path = dir.join(&digest);
        fs::write(&path, data).unwrap();
        serde_json::json!({"name":name,"bytes":data.len(),"sha256":digest,"object_path":path})
    }

    #[test]
    fn snapshot_images_keep_original_bytes_and_accept_uppercase_chinese_names() {
        let d = tempfile::tempdir().unwrap();
        let png = encoded(ImageFormat::Png);
        let jpg = encoded(ImageFormat::Jpeg);
        let files = serde_json::json!([
            object(d.path(), "图片.PNG", &png),
            object(d.path(), "目录/照片.JPEG", &jpg)
        ]);
        let images = from_manifest(&files, d.path()).unwrap();
        assert_eq!(images[0].data(), png);
        assert_eq!(images[0].media_type(), "image/png");
        assert_eq!(images[1].data(), jpg);
        assert_eq!(images[1].media_type(), "image/jpeg");
        // Resumed turns obtain the same immutable snapshot bytes.
        assert_eq!(from_manifest(&files, d.path()).unwrap()[0].data(), png);
    }

    #[test]
    fn rejects_corruption_forgery_outside_paths_and_too_many_images() {
        let d = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let png = encoded(ImageFormat::Png);
        assert!(ImageInput::from_bytes("fake.png".into(), b"not an image".to_vec()).is_err());
        assert!(ImageInput::from_bytes("wrong.jpg".into(), png.clone()).is_err());
        assert!(ImageInput::from_bytes("big.png".into(), vec![0; MAX_IMAGE_BYTES + 1]).is_err());
        let mut wide = Cursor::new(Vec::new());
        image::RgbImage::new(8193, 1)
            .write_to(&mut wide, ImageFormat::Png)
            .unwrap();
        assert!(ImageInput::from_bytes("wide.png".into(), wide.into_inner()).is_err());
        let row = object(d.path(), "ok.png", &png);
        assert!(from_manifest(&serde_json::json!(vec![row.clone(); 5]), d.path()).is_err());
        let outside = object(other.path(), "outside.png", &png);
        assert!(from_manifest(&serde_json::json!([outside]), d.path()).is_err());
        fs::write(row["object_path"].as_str().unwrap(), vec![0u8; png.len()]).unwrap();
        assert!(from_manifest(&serde_json::json!([row]), d.path()).is_err());
    }

    #[test]
    fn ordinary_files_remain_file_attachments() {
        let d = tempfile::tempdir().unwrap();
        let files = serde_json::json!([object(d.path(), "notes.txt", b"plain text")]);
        assert!(from_manifest(&files, d.path()).unwrap().is_empty());
        assert!(supports_native_images("codex") && supports_native_images("claude"));
        assert!(!supports_native_images("deepseek") && !supports_native_images("antigravity"));
    }
}
