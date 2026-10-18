//! Files pepper-agent writes: under `.pepper/` in a project (kept out of
//! git by a `.gitignore` of its own), or in its folder in the user's temp.
//! Only the user can open them (0700 folders, 0600 files).
//!
//! A project can come with a `.pepper/` in it already (from a repository or
//! an archive), holding links that lead elsewhere, or files git tracks.
//! Nothing here follows a link: its folders must be the user's own, a file
//! is written aside and moved over the name, a line is added only to a
//! plain file of the user's with no other name, and the name a program
//! writes is cleared first. Its `.gitignore` is made pepper-agent's again
//! whenever it isn't. What the viewer writes while it runs goes through
//! folders it holds open ([`Dir`]), into files it made.

use std::ffi::{CString, c_char, c_int, c_uint};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const O_WRONLY: c_int = 0x0001;
const O_NONBLOCK: c_int = 0x0004;
const O_APPEND: c_int = 0x0008;
const O_NOFOLLOW: c_int = 0x0100;
const O_CREAT: c_int = 0x0200;
const O_EXCL: c_int = 0x0800;
const O_DIRECTORY: c_int = 0x0010_0000;
const O_CLOEXEC: c_int = 0x0100_0000;
const AT_REMOVEDIR: c_int = 0x0080;

unsafe extern "C" {
    fn openat(fd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    fn mkdirat(fd: c_int, path: *const c_char, mode: u16) -> c_int;
    fn renameat(from_fd: c_int, from: *const c_char, to_fd: c_int, to: *const c_char) -> c_int;
    fn unlinkat(fd: c_int, path: *const c_char, flags: c_int) -> c_int;
    fn clonefile(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
}

/// clonefile(2): don't follow a link at `from`.
const CLONE_NOFOLLOW: c_uint = 0x0001;

/// A copy of `from` made at `to`, a new name, sharing its blocks until one
/// changes (APFS): `to` stays as `from` was, whatever happens to `from`
/// after. Fails across volumes.
pub fn clone_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_encoded_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (from, to) = (c(from)?, c(to)?);
    // SAFETY: two NUL-terminated paths; clonefile makes `to` (never over a
    // file there) and reads them only during the call.
    if unsafe { clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether a file starts as one mpv reads for the files it names, whatever
/// its name: an mpv EDL, a playlist, a cue sheet, an archive.
pub fn names_others(file: &Path) -> bool {
    let mut head = [0u8; 64];
    let read = File::open(file)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let head = head[..read].to_ascii_lowercase();
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&head);
    let start = head
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(head.len());
    let head = &head[start..];
    [
        b"# mpv edl".as_slice(),
        b"#extm3u",
        b"ffconcat",
        b"[playlist]",
        b"[reference]",
        b"[internetshortcut]",
        b"<asx",
        b"<?xml",
        b"file \"",
        b"rem ",
        b"pk\x03\x04",
        b"rar!",
        b"7z\xbc\xaf\x27\x1c",
    ]
    .iter()
    .any(|magic| head.starts_with(magic))
}

/// What `.pepper/.gitignore` holds: everything in `.pepper/` (itself too)
/// is kept out of git, and git looks for no other rule below it.
const GITIGNORE: &str = "# pepper-agent's files\n*\n";
/// Events files a viewer tries before giving up on `.pepper/`.
const EVENTS_TRIES: u32 = 20;

/// `<base>/.pepper/<parts…>`, made if need be, with pepper-agent's own
/// `.gitignore`, made again whenever it's missing or another (a project's
/// own would let git see what's here). Not in a folder others could
/// change, nor when that `.gitignore` can't be written.
pub fn pepper_dir(base: &Path, parts: &[&str]) -> std::io::Result<PathBuf> {
    others_could_change(base)?;
    let top = base.join(".pepper");
    own_dir(&top)?;
    let ignore = top.join(".gitignore");
    if read_plain(&ignore).as_deref() != Some(GITIGNORE) {
        write_private(&ignore, GITIGNORE.as_bytes())
            .map_err(|e| std::io::Error::other(format!("{}: {e}", ignore.display())))?;
    }
    sub_dir(&top, parts)
}

/// A project folder another user's, or one others may write in (without
/// the sticky bit): they could put a link where `.pepper` is.
fn others_could_change(base: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(base)?;
    let open = meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0;
    if meta.uid() != crate::sys::uid() || open {
        return Err(std::io::Error::other(format!(
            "{}: others can change what's in it",
            base.display()
        )));
    }
    Ok(())
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

/// Adds a line to a file only the user can read: a plain file of theirs,
/// never through a link.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    alone(&file)?;
    file.write_all(format!("{line}\n").as_bytes())
}

/// Whether an open file is a plain file of the user's with this one name:
/// another name for it (a hard link an archive came with) would have what's
/// written turn up there too.
pub fn alone(file: &File) -> std::io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    if meta.uid() != crate::sys::uid() {
        return Err(std::io::Error::other("not yours"));
    }
    if meta.nlink() != 1 {
        return Err(std::io::Error::other("it has another name (a hard link)"));
    }
    Ok(())
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

/// Whether `path` is a folder of the user's itself, not a link to one.
pub fn is_own_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == crate::sys::uid())
}

/// A plain file's text, not read through a link: what's kept in the cache.
pub fn read_plain(path: &Path) -> Option<String> {
    read_plain_at_most(path, u64::MAX)
}

/// [`read_plain`], for a file of at most `max` bytes.
pub fn read_plain_at_most(path: &Path, max: u64) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let mut text = String::new();
    file.take(max).read_to_string(&mut text).ok()?;
    Some(text)
}

/// A folder held open: what's made through it lands in that folder, even
/// once its path leads elsewhere (a `.pepper` replaced by a link while the
/// viewer runs). Names in it are plain names, never paths.
pub struct Dir {
    file: File,
    path: PathBuf,
}

impl Dir {
    /// The folder at `path`, the user's and not a link (made private if
    /// others could open it).
    pub fn open(path: &Path) -> std::io::Result<Dir> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(path)?;
        Dir::checked(file, path.to_owned())
    }

    fn checked(file: File, path: PathBuf) -> std::io::Result<Dir> {
        let meta = file.metadata()?;
        if !meta.is_dir() || meta.uid() != crate::sys::uid() {
            return Err(std::io::Error::other(format!(
                "{}: not a folder of yours",
                path.display()
            )));
        }
        if meta.mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Dir { file, path })
    }

    /// Where it was opened: what answers name.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its folder `name`, made if need be.
    pub fn sub(&self, name: &str) -> std::io::Result<Dir> {
        match self.make_dir(name) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let file = self.open_at(name, O_DIRECTORY | O_NOFOLLOW, 0)?;
        Dir::checked(file, self.path.join(name))
    }

    /// A new folder in it whose name no one can guess, for a program that
    /// writes by path: through a `.pepper` that became a link, its path
    /// leads nowhere. Its name, and the folder.
    pub fn stage(&self) -> std::io::Result<(String, Dir)> {
        let name = format!(".stage-{}", crate::sys::random_hex()?);
        self.make_dir(&name)?;
        let dir = self.sub(&name)?;
        Ok((name, dir))
    }

    /// A new file in it for appending, `name` not taken (not by a link
    /// either): this program's alone.
    pub fn create_new(&self, name: &str) -> std::io::Result<File> {
        self.open_at(
            name,
            O_WRONLY | O_APPEND | O_CREAT | O_EXCL | O_NOFOLLOW,
            0o600,
        )
    }

    /// Writes `name` whole: a new file aside, then moved over the name (a
    /// link there is replaced, not followed).
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let tmp = format!(".{name}.tmp{}-{n}", std::process::id());
        let _ = self.remove(&tmp);
        let mut file = self.create_new(&tmp)?;
        let written = file.write_all(bytes);
        drop(file);
        let moved = written.and_then(|_| self.adopt(self, &tmp, name));
        if moved.is_err() {
            let _ = self.remove(&tmp);
        }
        moved
    }

    /// Moves `from`, in `dir`, to `to` in this folder.
    pub fn adopt(&self, dir: &Dir, from: &str, to: &str) -> std::io::Result<()> {
        let (from, to) = (c_name(from)?, c_name(to)?);
        // SAFETY: two folder descriptors of ours and two C strings.
        let done = unsafe { renameat(dir.fd(), from.as_ptr(), self.fd(), to.as_ptr()) };
        if done != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Removes the file `name` (a link itself, not what it leads to).
    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        self.unlink(name, 0)
    }

    /// Removes the empty folder `name`.
    pub fn remove_dir(&self, name: &str) -> std::io::Result<()> {
        self.unlink(name, AT_REMOVEDIR)
    }

    fn unlink(&self, name: &str, flags: c_int) -> std::io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: a folder descriptor of ours and a C string.
        if unsafe { unlinkat(self.fd(), name.as_ptr(), flags) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn make_dir(&self, name: &str) -> std::io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: a folder descriptor of ours and a C string.
        if unsafe { mkdirat(self.fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn open_at(&self, name: &str, flags: c_int, mode: c_uint) -> std::io::Result<File> {
        let name = c_name(name)?;
        // SAFETY: a folder descriptor of ours and a C string; the mode is
        // passed as the variadic argument open takes.
        let fd = unsafe { openat(self.fd(), name.as_ptr(), flags | O_CLOEXEC, mode) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a descriptor just opened, owned by nothing else.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }
}

/// A name in a folder: not a path, not `.` or `..`.
fn c_name(name: &str) -> std::io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(std::io::Error::other(format!(
            "{name:?} isn't a file's name"
        )));
    }
    CString::new(name).map_err(|_| std::io::Error::other("a NUL in a file's name"))
}

/// A new file for a viewer's events in `dir`, which it made: `events.jsonl`,
/// or when there's one already (an earlier viewer's, or one a project came
/// with, which git may track) `events-<id>.jsonl`, then `events-<id>-2.jsonl`
/// and on. Its path, and the file to add lines to.
pub fn new_events_file(dir: &Dir, id: &str) -> std::io::Result<(PathBuf, File)> {
    let names = std::iter::once("events.jsonl".to_owned())
        .chain(std::iter::once(format!("events-{id}.jsonl")))
        .chain((2..EVENTS_TRIES).map(|n| format!("events-{id}-{n}.jsonl")));
    for name in names {
        match dir.create_new(&name) {
            Ok(file) => return Ok((dir.path().join(name), file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no name left for an events file"))
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
        assert!(!is_plain(&project.join(".pepper/events.jsonl")));

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

        // A line isn't added to a file with another name.
        std::fs::write(outside.join("log"), "").unwrap();
        std::fs::hard_link(outside.join("log"), project.join(".pepper/log")).unwrap();
        assert!(append_line(&project.join(".pepper/log"), "x").is_err());
        assert_eq!(std::fs::read_to_string(outside.join("log")).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clips_that_name_others_and_clones() {
        let dir = scratch("names-others");
        let clip = dir.join("clip.mp4");
        for (body, names) in [
            (&b"# mpv EDL v0\n%3%/x\n"[..], true),
            (b"\xef\xbb\xbf  #EXTM3U\n/x.mp4\n", true),
            (b"[Playlist]\nFile1=/x\n", true),
            (b"ffconcat version 1.0\n", true),
            (b"PK\x03\x04rest", true),
            (b"\x00\x00\x00\x18ftypmp42", false),
            (b"\x1a\x45\xdf\xa3 matroska", false),
        ] {
            std::fs::write(&clip, body).unwrap();
            assert_eq!(names_others(&clip), names, "{body:?}");
        }
        // A clone is the file as it was, and never made over another.
        let (a, b) = (dir.join("a.bin"), dir.join("b.bin"));
        std::fs::write(&a, b"one").unwrap();
        clone_file(&a, &b).unwrap();
        std::fs::write(&a, b"two").unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"one");
        assert!(clone_file(&a, &b).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn gitignore_is_made_ours_again() {
        let dir = scratch("gitignore");
        let project = dir.join("project");
        let top = project.join(".pepper");
        // A .pepper a repository came with: a file in it, no .gitignore;
        // then one that lets git see everything.
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(top.join(".keep"), "").unwrap();
        pepper_dir(&project, &["snapshots"]).unwrap();
        assert_eq!(
            read_plain(&top.join(".gitignore")).as_deref(),
            Some(GITIGNORE)
        );
        std::fs::write(top.join(".gitignore"), "!*\n").unwrap();
        pepper_dir(&project, &[]).unwrap();
        assert_eq!(
            read_plain(&top.join(".gitignore")).as_deref(),
            Some(GITIGNORE)
        );
        // One that's a link is replaced, not written through.
        std::fs::remove_file(top.join(".gitignore")).unwrap();
        std::fs::write(dir.join("theirs"), "!*\n").unwrap();
        std::os::unix::fs::symlink(dir.join("theirs"), top.join(".gitignore")).unwrap();
        pepper_dir(&project, &[]).unwrap();
        assert!(is_plain(&top.join(".gitignore")));
        assert_eq!(std::fs::read_to_string(dir.join("theirs")).unwrap(), "!*\n");
        // One that can't be written: no .pepper there.
        std::fs::remove_file(top.join(".gitignore")).unwrap();
        std::fs::create_dir(top.join(".gitignore")).unwrap();
        assert!(pepper_dir(&project, &[]).is_err());
        // Nor in a folder others may write in.
        let open = dir.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(pepper_dir(&open, &[]).is_err());
        assert!(!open.join(".pepper").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_folders_stay_put() {
        let dir = scratch("held");
        let (project, elsewhere) = (dir.join("project"), dir.join("elsewhere"));
        std::fs::create_dir_all(project.join(".pepper")).unwrap();
        std::fs::create_dir_all(elsewhere.join("points")).unwrap();
        let top = Dir::open(&project.join(".pepper")).unwrap();
        let points = top.sub("points").unwrap();
        // A project's own events.jsonl is left alone: a new file is made.
        std::fs::write(project.join(".pepper/events.jsonl"), "theirs\n").unwrap();
        let (path, mut events) = new_events_file(&top, "7").unwrap();
        assert_eq!(path, project.join(".pepper/events-7.jsonl"));
        let (again, _) = new_events_file(&top, "7").unwrap();
        assert_eq!(again, project.join(".pepper/events-7-2.jsonl"));
        // Nor a link at the name.
        std::os::unix::fs::symlink(dir.join("made"), project.join(".pepper/events-8.jsonl"))
            .unwrap();
        let (other, _) = new_events_file(&top, "8").unwrap();
        assert_eq!(other, project.join(".pepper/events-8-2.jsonl"));
        assert!(!dir.join("made").exists());

        // .pepper replaced by a link while the viewer runs: what it writes
        // still lands in the folder it held, never the link's.
        let moved = dir.join("moved");
        std::fs::rename(project.join(".pepper"), &moved).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(".pepper")).unwrap();
        events.write_all(b"a line\n").unwrap();
        points.write("p-full.png", b"png").unwrap();
        assert_eq!(
            std::fs::read(moved.join("points/p-full.png")).unwrap(),
            b"png"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("events-7.jsonl")).unwrap(),
            "a line\n"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("events.jsonl")).unwrap(),
            "theirs\n"
        );
        assert!(
            std::fs::read_dir(elsewhere.join("points"))
                .unwrap()
                .next()
                .is_none()
        );

        // A stage's path leads nowhere through the link; its files are
        // moved in through the held folder.
        let (name, stage) = points.stage().unwrap();
        assert!(!points.path().join(&name).exists());
        std::fs::write(moved.join("points").join(&name).join("c.png"), "c").unwrap();
        points.adopt(&stage, "c.png", "c.png").unwrap();
        points.remove_dir(&name).unwrap();
        assert_eq!(
            std::fs::read_to_string(moved.join("points/c.png")).unwrap(),
            "c"
        );
        for bad in ["", "..", "a/b"] {
            assert!(points.write(bad, b"x").is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
