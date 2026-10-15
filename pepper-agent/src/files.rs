//! Files pepper-agent writes: under `.pepper/` in a project (kept out of
//! git by a `.gitignore` of its own), or in its folder in the user's temp.
//! Only the user can open them (0700 folders, 0600 files).

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `<base>/.pepper/<parts…>`, made if need be.
pub fn pepper_dir(base: &Path, parts: &[&str]) -> std::io::Result<PathBuf> {
    let top = base.join(".pepper");
    if !top.is_dir() {
        private_dir(&top)?;
        let _ = std::fs::write(top.join(".gitignore"), "# pepper-agent's files\n*\n");
    }
    let mut dir = top;
    for part in parts {
        dir.push(part);
        if !dir.is_dir() {
            private_dir(&dir)?;
        }
    }
    Ok(dir)
}

pub fn private_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Adds a line to a file only the user can read.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(format!("{line}\n").as_bytes())
}

/// Writes a file only the user can read, whole or not at all.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// A file name made of `text`: letters, digits, `.`, `_` and `-` kept, the
/// rest `-`, at most 48.
pub fn name_part(text: &str) -> String {
    let name: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let name = name.trim_matches(|c| c == '.' || c == '-');
    if name.is_empty() {
        "x".into()
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(name_part("my clip.mp4"), "my-clip.mp4");
        assert_eq!(name_part("../../etc"), "etc");
        assert_eq!(name_part("agent-2/web"), "agent-2-web");
        assert_eq!(name_part(".."), "x");
        assert_eq!(name_part("a\x1bb"), "a-b");
    }
}
