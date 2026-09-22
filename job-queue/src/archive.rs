use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::io::{self, Write};
use std::path::Path;
use walkdir::WalkDir;

const SKIPPED: &[&str] = &["target", ".git"];

pub fn pack(parts: &[(&Path, &str)]) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    builder.follow_symlinks(false);
    for (path, name) in parts {
        if path.is_file() {
            builder.append_path_with_name(path, name)?;
        } else if path.is_dir() {
            append_dir(&mut builder, path, name)?;
        }
    }
    builder.into_inner()?.finish()
}

fn append_dir<W: Write>(builder: &mut tar::Builder<W>, root: &Path, name: &str) -> io::Result<()> {
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        e.depth() == 0 || !SKIPPED.contains(&e.file_name().to_string_lossy().as_ref())
    });
    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;
        let rel = entry.path().strip_prefix(root).map_err(io::Error::other)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        builder.append_path_with_name(entry.path(), Path::new(name).join(rel))?;
    }
    Ok(())
}

pub fn unpack(bytes: &[u8], dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    tar::Archive::new(GzDecoder::new(bytes)).unpack(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn round_trip_keeps_files_and_drops_build_output() {
        let base = std::env::temp_dir().join(format!("rq-archive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::create_dir_all(src.join("target")).unwrap();
        fs::write(src.join("lib.rs"), "a").unwrap();
        fs::write(src.join("nested/mod.rs"), "b").unwrap();
        fs::write(src.join("target/junk"), "c").unwrap();
        let manifest = base.join("Cargo.toml");
        fs::write(&manifest, "[workspace]").unwrap();

        let bytes = pack(&[(&src, "source"), (&manifest, "Cargo.toml")]).unwrap();
        let out = base.join("out");
        unpack(&bytes, &out).unwrap();

        assert_eq!(fs::read_to_string(out.join("source/lib.rs")).unwrap(), "a");
        assert_eq!(
            fs::read_to_string(out.join("source/nested/mod.rs")).unwrap(),
            "b"
        );
        assert_eq!(
            fs::read_to_string(out.join("Cargo.toml")).unwrap(),
            "[workspace]"
        );
        assert!(!out.join("source/target").exists());
    }

    #[test]
    fn missing_parts_are_skipped() {
        let bytes = pack(&[(Path::new("/nonexistent/dir"), "ws")]).unwrap();
        let out = std::env::temp_dir().join(format!("rq-empty-{}", std::process::id()));
        unpack(&bytes, &out).unwrap();
        assert!(!out.join("ws").exists());
    }
}
