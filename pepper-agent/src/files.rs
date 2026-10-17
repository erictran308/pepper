//! Files pepper-agent writes: under `.pepper/` in a project (kept out of
//! git by a `.gitignore` of its own), or in its folder in the user's temp.
//! Only the user can open them (0700 folders, 0600 files).
//!
//! A project can come with a `.pepper/` in it already (from a repository or
//! an archive), holding links that lead elsewhere. Nothing here follows one:
//! its folders must be the user's own, a file is written aside and moved
//! over the name, a line is added only to a plain file, and the name a
//! program writes is cleared first.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const O_NONBLOCK: i32 = 0x0004;
const O_NOFOLLOW: i32 = 0x0100;

/// `<base>/.pepper/<parts…>`, made if need be.
pub fn pepper_dir(base: &Path, parts: &[&str]) -> std::io::Result<PathBuf> {
    let top = base.join(".pepper");
    if own_dir(&top)? {
        let _ = write_private(&top.join(".gitignore"), b"# pepper-agent's files\n*\n");
    }
    sub_dir(&top, parts)
}

/// `<dir>/<parts…>`, each made if need be, each a folder of the user's.
pub fn sub_dir(dir: &Path, parts: &[&str]) -> std::io::Result<PathBuf> {
    let mut dir = dir.to_owned();
    for part in parts {
        dir.push(part);
        own_dir(&dir)?;
    }
    Ok(dir)
}

/// A folder of the user's at `path`, not a link, that only they can open:
/// made if there's nothing there (true), made private again if others could
/// open it.
pub fn own_dir(path: &Path) -> std::io::Result<bool> {
    let fail = |what: String| std::io::Error::other(format!("{}: {what}", path.display()));
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => Err(fail("a link or a file, not a folder".into())),
        Ok(meta) if meta.uid() != crate::sys::uid() => Err(fail("not yours".into())),
        Ok(meta) => {
            if meta.mode() & 0o077 != 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| fail(e.to_string()))?;
            }
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|e| fail(e.to_string()))?;
            Ok(true)
        }
        Err(e) => Err(fail(e.to_string())),
    }
}

pub fn private_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Adds a line to a file only the user can read: a plain file, never
/// through a link.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    file.write_all(format!("{line}\n").as_bytes())
}

/// Writes a file only the user can read, whole or not at all: a new file
/// aside, then moved over the name (a link there is replaced, not followed).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{n}", std::process::id()));
    make_way(&tmp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Clears `path` for a program to write a new file there: whatever has the
/// name goes (a link itself, not what it leads to). A folder stays.
pub fn make_way(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether `path` is a plain file itself, not a link to one.
pub fn is_plain(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Whether `path` is a plain file, or nothing yet.
pub fn plain_or_none(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_file(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// A plain file's text, not read through a link: what's kept in the cache.
pub fn read_plain(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pepper-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn links_are_never_followed() {
        let dir = scratch("links");
        let (project, outside) = (dir.join("project"), dir.join("outside"));
        std::fs::create_dir_all(project.join(".pepper")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("target");
        std::fs::write(&target, "keep").unwrap();
        let link = |name: &str, to: &Path| {
            std::os::unix::fs::symlink(to, project.join(".pepper").join(name)).unwrap()
        };

        // A line isn't added through a link, or to one that leads nowhere yet.
        link("events.jsonl", &target);
        assert!(append_line(&project.join(".pepper/events.jsonl"), "x").is_err());
        link("new.jsonl", &outside.join("made"));
        assert!(append_line(&project.join(".pepper/new.jsonl"), "x").is_err());
        assert!(!outside.join("made").exists());
        assert!(!plain_or_none(&project.join(".pepper/events.jsonl")));
        assert!(plain_or_none(&project.join(".pepper/none.jsonl")));

        // A file written over a link replaces the link.
        link("probe.json", &target);
        assert!(read_plain(&project.join(".pepper/probe.json")).is_none());
        assert!(!is_plain(&project.join(".pepper/probe.json")));
        write_private(&project.join(".pepper/probe.json"), b"{}").unwrap();
        assert_eq!(
            read_plain(&project.join(".pepper/probe.json")).as_deref(),
            Some("{}")
        );
        make_way(&project.join(".pepper/events.jsonl")).unwrap();
        assert!(!project.join(".pepper/events.jsonl").exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        // A folder that's a link is refused; one others could open is made private.
        link("cache", &outside);
        assert!(pepper_dir(&project, &["cache", "key"]).is_err());
        assert!(!outside.join("key").exists());
        std::fs::set_permissions(
            project.join(".pepper"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let made = pepper_dir(&project, &["points"]).unwrap();
        assert!(made.is_dir());
        let mode = std::fs::metadata(project.join(".pepper")).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::create_dir(dir.join("via")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("via/.pepper")).unwrap();
        assert!(pepper_dir(&dir.join("via"), &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
