//! Path validation: every command that takes a capture names it by id (the
//! directory name the sink chose) and never by path. An id is resolved
//! against the captures directory and refused unless it is a real directory
//! directly inside it (canonicalised, no symlink or junction).

use std::fs;
use std::path::{Path, PathBuf};

/// Longest id accepted (the sink's names are `<unix seconds>_<8 chars>[-n]`).
const MAX_ID_LEN: usize = 64;

/// Name of the result file `streamtap-sink mux` writes into a capture.
pub(crate) const RESULT_NAME: &str = "out.mp4";

/// Checks that `id` has the shape of a capture directory the sink creates:
/// `<digits>_<letters, digits, '_' or '-'>` (`<unix seconds>_<session>[-n]`).
/// That is a plain name (no separator, dot, drive or stream syntax) and it
/// keeps every other folder out: when the captures folder is pointed at an
/// ordinary folder, its subfolders (`src`, `Documents`) are no captures and
/// can neither be listed nor deleted by this app.
pub(crate) fn validate_id(id: &str) -> Result<&str, String> {
    let ok = id.len() <= MAX_ID_LEN
        && id.split_once('_').is_some_and(|(secs, rest)| {
            !secs.is_empty()
                && secs.bytes().all(|b| b.is_ascii_digit())
                && !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        });
    if ok {
        Ok(id)
    } else {
        Err("invalid capture id".into())
    }
}

/// Resolves `id` to the capture directory inside `root`. The returned path is
/// `root/id` (not the canonical form, which carries a `\\?\` prefix on
/// Windows that other programs do not all understand) after the canonical
/// form was checked to lie directly in the canonical root.
pub(crate) fn resolve_capture(root: &Path, id: &str) -> Result<PathBuf, String> {
    let id = validate_id(id)?;
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("captures directory is not available: {e}"))?;
    let path = root.join(id);
    let meta = fs::symlink_metadata(&path).map_err(|_| "capture not found".to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing a symbolic link or junction".into());
    }
    if !meta.is_dir() {
        return Err("capture is not a directory".into());
    }
    let canon = path
        .canonicalize()
        .map_err(|e| format!("capture is not available: {e}"))?;
    if canon.parent() != Some(canon_root.as_path()) {
        return Err("capture is outside the captures directory".into());
    }
    Ok(path)
}

/// `dir/out.mp4` when it is a regular file (not a link), with its size.
pub(crate) fn result_file(dir: &Path) -> Option<(PathBuf, u64)> {
    let path = dir.join(RESULT_NAME);
    let meta = fs::symlink_metadata(&path).ok()?;
    if meta.is_file() && !meta.file_type().is_symlink() {
        Some((path, meta.len()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("streamtap-app-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ids_must_be_plain_names() {
        for good in [
            "1730000000_abcd1234",
            "1730000000_abcd1234-2",
            "1730000000_anon",
            "1_x",
            "1730000000_A_b-c",
        ] {
            assert!(validate_id(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:",
            "C:\\x",
            "x:stream",
            "1_x:stream",
            "1_x.",
            "1_x.y",
            ".hidden",
            "_x",
            "1_",
            "src",
            "Documents",
            "a.b",
            "cap1",
            "x_1",
            "1a_x",
            "trail.",
            "sp ace",
            "tab\t",
            "uni\u{e4}",
            "nul\0",
            "a/../b",
            "..\\..",
        ] {
            assert!(validate_id(bad).is_err(), "{bad:?}");
        }
        assert!(validate_id(&format!("1_{}", "a".repeat(MAX_ID_LEN - 1))).is_err());
        assert!(validate_id(&format!("1_{}", "a".repeat(MAX_ID_LEN - 2))).is_ok());
    }

    #[test]
    fn an_ordinary_subfolder_is_no_capture() {
        // The captures folder pointed at a normal folder: its subfolders
        // must not be reachable (delete would remove them).
        let root = tmp("ordinary");
        for name in ["src", "Documents", "backup.2024"] {
            fs::create_dir(root.join(name)).unwrap();
            assert!(resolve_capture(&root, name).is_err(), "{name}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolves_a_directory_inside_the_root() {
        let root = tmp("resolve");
        fs::create_dir(root.join("1_cap1")).unwrap();
        let p = resolve_capture(&root, "1_cap1").unwrap();
        assert_eq!(p, root.join("1_cap1"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_missing_files_and_traversal() {
        let root = tmp("refuse");
        fs::write(root.join("1_afile"), b"x").unwrap();
        assert!(resolve_capture(&root, "1_nothere").is_err());
        assert!(resolve_capture(&root, "1_afile").is_err());
        assert!(resolve_capture(&root, "..").is_err());
        assert!(resolve_capture(&root, "../other").is_err());
        assert!(resolve_capture(&root.join("missing-root"), "1_cap").is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks() {
        let root = tmp("symlink");
        let outside = tmp("symlink-outside");
        std::os::unix::fs::symlink(&outside, root.join("1_link")).unwrap();
        assert!(resolve_capture(&root, "1_link").is_err());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn refuses_junctions() {
        // A junction needs no privilege (`mklink /J`), unlike a symlink.
        let root = tmp("junction");
        let outside = tmp("junction-outside");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("1_link"))
            .arg(&outside)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(resolve_capture(&root, "1_link").is_err());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn result_file_needs_a_regular_file() {
        let root = tmp("result");
        assert!(result_file(&root).is_none());
        fs::create_dir(root.join(RESULT_NAME)).unwrap();
        assert!(result_file(&root).is_none(), "a directory is no result");
        fs::remove_dir(root.join(RESULT_NAME)).unwrap();
        fs::write(root.join(RESULT_NAME), b"1234").unwrap();
        assert_eq!(result_file(&root).map(|r| r.1), Some(4));
        fs::remove_dir_all(&root).unwrap();
    }
}
