//! Repository-relative path validation.
//!
//! Every path that reaches the filesystem — from the working copy, from commit
//! history, from a remote repository or from an HTTP request — must pass
//! [`validate_rel_path`] and be resolved with [`safe_join`]. History is
//! untrusted input: a crafted commit must not be able to write outside the
//! working copy, into version-control metadata, or through a symbolic link.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, VcsError};

/// Metadata directory names that repository content may never contain.
const RESERVED_NAMES: &[&str] = &[".vcrs", ".git"];

/// Check that `rel` is a canonical repository-relative path: `/`-separated,
/// non-empty components, no `.`/`..`, no drive or stream specifiers, no control
/// characters and no reserved metadata directory at any level.
pub fn validate_rel_path(rel: &str) -> Result<()> {
    if rel.is_empty() {
        return Err(invalid(rel, "empty path"));
    }
    if rel.contains('\\') {
        return Err(invalid(rel, "backslash in path"));
    }
    for comp in rel.split('/') {
        if comp.is_empty() {
            return Err(invalid(rel, "empty path component"));
        }
        if comp == "." || comp == ".." {
            return Err(invalid(rel, "relative path component"));
        }
        if comp.chars().any(char::is_control) {
            return Err(invalid(rel, "control character in path"));
        }
        // "C:" / "C:foo" escape the root when pushed onto a Windows path; on
        // Windows any ':' may also name an NTFS alternate data stream.
        if is_drive_prefix(comp) || (cfg!(windows) && comp.contains(':')) {
            return Err(invalid(rel, "drive or stream specifier in path"));
        }
        if is_reserved_component(comp) {
            return Err(invalid(rel, "reserved metadata directory"));
        }
    }
    Ok(())
}

/// True when `comp` names a metadata directory on any supported filesystem:
/// compared case-insensitively (macOS/Windows defaults), ignoring the trailing
/// dots/spaces and stream suffix that Windows strips, and including the NTFS
/// 8.3 alias (`VCRS~1`).
pub fn is_reserved_component(comp: &str) -> bool {
    let base = comp.split(':').next().unwrap_or(comp);
    let trimmed = base.trim_end_matches(['.', ' ']);
    RESERVED_NAMES
        .iter()
        .any(|r| trimmed.eq_ignore_ascii_case(r) || is_short_name_of(trimmed, r))
}

fn is_short_name_of(name: &str, reserved: &str) -> bool {
    let stem = reserved.trim_start_matches('.');
    let stem = &stem[..stem.len().min(6)];
    let Some((head, tail)) = name.split_once('~') else {
        return false;
    };
    head.eq_ignore_ascii_case(stem) && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())
}

fn is_drive_prefix(comp: &str) -> bool {
    let b = comp.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Resolve a validated repository-relative path under `root`, refusing to
/// traverse a symbolic link in any intermediate component (a link planted by
/// history must not redirect later writes outside the working copy).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    validate_rel_path(rel)?;
    let mut out = root.to_path_buf();
    let mut comps = rel.split('/').peekable();
    while let Some(comp) = comps.next() {
        out.push(comp);
        if comps.peek().is_some()
            && fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(invalid(rel, "path traverses a symbolic link"));
        }
    }
    Ok(out)
}

/// Repository-relative form of a filesystem path found under `root`, or `None`
/// when the name cannot be represented as a valid repository path.
pub fn rel_from_fs(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?.to_str()?;
    let rel = if cfg!(windows) {
        rel.replace('\\', "/")
    } else {
        rel.to_owned()
    };
    validate_rel_path(&rel).ok()?;
    Some(rel)
}

/// Normalize a user-supplied repository-relative path (`./a//b/` -> `a/b`,
/// Windows separators -> `/`). The result still needs [`validate_rel_path`].
pub fn normalize_rel(input: &str) -> String {
    let input = if cfg!(windows) {
        input.replace('\\', "/")
    } else {
        input.to_owned()
    };
    input
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Convert a path given on the command line (relative to `cwd` or absolute) to
/// a repository-relative path. Returns an empty string for the root itself.
pub fn user_path_to_rel(root: &Path, cwd: &Path, input: &str) -> Result<String> {
    let joined = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        cwd.join(input)
    };
    let mut norm = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let rel = norm
        .strip_prefix(root)
        .map_err(|_| VcsError::PathOutsideRepository(input.to_owned()))?;
    let rel = rel
        .to_str()
        .ok_or_else(|| invalid(input, "path is not valid UTF-8"))?;
    let rel = normalize_rel(rel);
    if !rel.is_empty() {
        validate_rel_path(&rel)?;
    }
    Ok(rel)
}

fn invalid(path: &str, reason: &'static str) -> VcsError {
    VcsError::InvalidPath {
        path: path.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_nested_paths() {
        for ok in [
            "a.txt",
            "src/main.rs",
            ".vcrsignore",
            "dir/.gitignore",
            "a b/c",
        ] {
            assert!(validate_rel_path(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_escapes_and_metadata() {
        for bad in [
            "",
            "/abs",
            "a/",
            "a//b",
            "../x",
            "a/../../x",
            "./a",
            "C:/Windows",
            "foo/C:/x",
            "C:evil",
            ".vcrs/hooks/pre-commit",
            ".VCRS/hooks/pre-commit",
            "sub/.Vcrs/x",
            ".vcrs./x",
            ".vcrs /x",
            ".vcrs::$INDEX_ALLOCATION/x",
            "VCRS~1/x",
            ".git/hooks/post-checkout",
            ".GIT/config",
            "GIT~1/config",
            "a\\b",
            "new\nline",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_refuses_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(safe_join(dir.path(), "link/file").is_err());
        // The link itself may be addressed (it is replaced, not followed).
        assert!(safe_join(dir.path(), "link").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn user_paths_resolve_against_cwd() {
        let root = Path::new("/repo");
        assert_eq!(
            user_path_to_rel(root, Path::new("/repo/src"), "main.rs").unwrap(),
            "src/main.rs"
        );
        assert_eq!(
            user_path_to_rel(root, Path::new("/repo/src"), "../README.md").unwrap(),
            "README.md"
        );
        assert_eq!(user_path_to_rel(root, Path::new("/repo"), ".").unwrap(), "");
        assert!(user_path_to_rel(root, Path::new("/repo"), "../outside").is_err());
    }

    proptest::proptest! {
        #[test]
        fn valid_paths_never_leave_the_root(input in "[a-zA-Z.:/\\\\~ -]{0,24}") {
            if validate_rel_path(&input).is_ok() {
                let joined = Path::new("/root").join(&input);
                proptest::prop_assert!(joined.starts_with("/root"));
                proptest::prop_assert!(joined
                    .components()
                    .skip(2)
                    .all(|c| matches!(c, Component::Normal(_))));
            }
        }
    }
}
