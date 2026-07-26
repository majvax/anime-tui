//! On-disk cache for posters and metadata. Keys are sanitized before they ever
//! touch the filesystem so a hostile provider id cannot escape the cache dir.

use crate::errors::Result;
use std::path::{Path, PathBuf};

pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(sanitize_key(key))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Reduce an arbitrary identifier to a safe single filename component:
/// keep `[A-Za-z0-9._-]`, replace everything else with `_`, and never allow
/// `.`/`..` or path separators through.
pub fn sanitize_key(key: &str) -> String {
    let mut out: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = "_".into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sanitize_key;

    #[test]
    fn strips_path_traversal() {
        // Separators become `_`; dots are kept but can't form a path since the
        // result is a single filename component -> no traversal.
        assert_eq!(sanitize_key("../../etc/passwd"), ".._.._etc_passwd");
        assert!(!sanitize_key("../../etc/passwd").contains('/'));
        assert_eq!(sanitize_key("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_key(".."), "_");
        assert_eq!(sanitize_key(""), "_");
    }

    #[test]
    fn keeps_safe_chars() {
        assert_eq!(sanitize_key("Anime_Title-01.jpg"), "Anime_Title-01.jpg");
    }
}
