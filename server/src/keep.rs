//! Kept images: agents point at screenshots in /tmp or caches that vanish (9 broken pictures on the phone on
//! 2026-10-05). An image a surface references, or one the phone loaded through `/api/file`, is copied to
//! `~/.cache/sheprd/keep/<fnv(path)>.<ext>`; when the original is gone, `/api/file` serves the copy for that
//! same path. Only image types, at most `MAX_FILE` each; the directory is pruned oldest-first above `MAX_DIR`.
//! A copy is only ever made of a file `/api/file` was allowed to serve, and the fallback reads only from the
//! keep directory by hash, so it adds no new reachable paths.
//! Hooks from main.rs: `stash` (api_surface, api_file), `find` (api_file).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_FILE: u64 = 12 << 20;
const MAX_DIR: u64 = 400 << 20;
const IMAGE_EXT: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "heic", "avif"];

fn dir() -> PathBuf {
    std::env::var_os("SHEPRD_KEEP_DIR").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".cache/sheprd/keep"))
}

fn key(raw: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.trim().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn image_ext(p: &Path) -> Option<String> {
    let e = p.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXT.contains(&e.as_str()).then_some(e)
}

/// Copies `src` (already resolved and allowed) as the kept image for the path string `raw`. Blocking.
pub fn stash(raw: &str, src: &Path) {
    let Some(ext) = image_ext(src) else { return };
    let Ok(m) = std::fs::metadata(src) else { return };
    if !m.is_file() || m.len() == 0 || m.len() > MAX_FILE || src.starts_with(dir()) {
        return;
    }
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    let dest = d.join(format!("{}.{ext}", key(raw)));
    if std::fs::metadata(&dest).is_ok_and(|k| k.len() == m.len()) {
        // same picture (by size): just mark it recently used
        let _ = std::fs::File::options().append(true).open(&dest).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return;
    }
    let tmp = dest.with_extension("tmp");
    if std::fs::copy(src, &tmp).is_ok() {
        let _ = std::fs::rename(&tmp, &dest);
        prune();
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The kept copy for the path string `raw`, if any.
pub fn find(raw: &str) -> Option<PathBuf> {
    let k = key(raw);
    IMAGE_EXT.iter().map(|e| dir().join(format!("{k}.{e}"))).find(|p| p.is_file())
}

static PRUNING: Mutex<()> = Mutex::new(());

fn prune() {
    let Ok(_g) = PRUNING.try_lock() else { return };
    let Ok(rd) = std::fs::read_dir(dir()) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= MAX_DIR {
        return;
    }
    files.sort();
    for (_, len, p) in files {
        if total <= MAX_DIR * 9 / 10 {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_and_finds_by_path() {
        let base = std::env::temp_dir().join(format!("mm-keep-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        // SAFETY: tests in this module are the only users of SHEPRD_KEEP_DIR.
        unsafe { std::env::set_var("SHEPRD_KEEP_DIR", base.join("keep")) };
        let src = base.join("shot.png");
        std::fs::write(&src, b"\x89PNG fake").unwrap();
        let raw = src.to_string_lossy().to_string();
        stash(&raw, &src);
        std::fs::remove_file(&src).unwrap();
        let k = find(&raw).expect("kept copy");
        assert!(k.starts_with(base.join("keep")));
        assert_eq!(std::fs::read(&k).unwrap(), b"\x89PNG fake");
        // non-images are never kept
        let txt = base.join("notes.txt");
        std::fs::write(&txt, b"secret").unwrap();
        stash(&txt.to_string_lossy(), &txt);
        assert!(find(&txt.to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
