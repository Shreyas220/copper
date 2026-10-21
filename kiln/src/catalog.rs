//! The image catalogue: content-addressed base images that nodes fetch by
//! hash. Blobs live as `<catalog dir>/<sha256>.img`; the `images` table maps
//! a platform-neutral name and an arch to a blob.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context};
use copper_api::*;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::db;

/// Copy `src` into the catalogue under its SHA-256 and point `name`/`arch` at
/// it. Re-importing identical content is a no-op apart from the mapping.
pub fn import(conn: &Connection, dir: &Path, name: &str, arch: Arch, src: &Path) -> anyhow::Result<Image> {
    if !valid_image_name(name) {
        bail!("image names are lowercase letters, digits, '.', '_' or '-', starting with a letter or digit");
    }
    let mut input = std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut magic = [0u8; 4];
    input.read_exact(&mut magic).context("reading image header")?;
    // Both backends start from a qcow2 cloud image (Ubuntu's .img files are qcow2).
    if &magic != b"QFI\xfb" {
        bail!("{} is not a qcow2 image", src.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".import-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut out = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(magic);
        out.write_all(&magic)?;
        let mut size = magic.len() as u64;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 { break }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            size += n as u64;
        }
        out.sync_all()?;
        let sha256 = format!("{:x}", hasher.finalize());
        let blob = dir.join(format!("{sha256}.img"));
        if blob.exists() {
            std::fs::remove_file(&tmp)?;
        } else {
            std::fs::rename(&tmp, &blob).with_context(|| format!("publishing {}", blob.display()))?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        anyhow::Ok(Image { name: name.to_string(), arch, sha256, size_bytes: size })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let image = result?;
    db::upsert_image(conn, &image)?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_is_content_addressed_and_rejects_non_qcow2() {
        let dir = std::env::temp_dir().join(format!("kiln-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(Path::new(":memory:")).unwrap();
        let src = dir.join("ubuntu.img");
        std::fs::write(&src, b"QFI\xfbrest of a disk").unwrap();
        let a = import(&conn, &dir.join("catalog"), "ubuntu-24.04", Arch::X86_64, &src).unwrap();
        let b = import(&conn, &dir.join("catalog"), "ubuntu-24.04", Arch::Aarch64, &src).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.size_bytes, 18);
        assert!(dir.join("catalog").join(format!("{}.img", a.sha256)).is_file());
        assert_eq!(db::list_images(&conn).unwrap().len(), 2);
        assert!(db::blob_referenced(&conn, &a.sha256).unwrap());
        std::fs::write(&src, b"not an image").unwrap();
        assert!(import(&conn, &dir.join("catalog"), "bad", Arch::X86_64, &src).is_err());
        assert!(import(&conn, &dir.join("catalog"), "../escape", Arch::X86_64, &src).is_err());
        let leftovers = std::fs::read_dir(dir.join("catalog")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(leftovers, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
