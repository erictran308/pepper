//! `install-skill`: the guide as an agent skill (agentskills.io's format),
//! for agents that pick skills by their description rather than being told
//! to run `pepper-agent guide`. The repository keeps it as
//! `skills/pepper/SKILL.md`: a front matter, then guide.md as it is (a test
//! keeps the two the same).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::files;
use crate::json::Value;
use crate::text;

pub const SKILL: &str = include_str!("../skills/pepper/SKILL.md");
const NAME: &str = "pepper";

/// The value of a key in a SKILL.md's front matter.
fn front(skill: &str, key: &str) -> Option<String> {
    let rest = skill.strip_prefix("---\n")?;
    let (head, _) = rest.split_once("\n---\n")?;
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

/// What follows the front matter.
#[cfg(test)]
fn body(skill: &str) -> Option<&str> {
    let rest = skill.strip_prefix("---\n")?;
    let (_, body) = rest.split_once("\n---\n")?;
    Some(body.strip_prefix('\n').unwrap_or(body))
}

/// `~/x` as the home folder's `x`; a relative path from here.
fn folder(given: Option<&str>) -> Result<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::new("FAILED", "HOME isn't set"))
    };
    let dir = match given {
        None => home()?.join(".claude").join("skills"),
        Some("~") => home()?,
        Some(d) => match d.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None => {
                std::path::absolute(d).map_err(|e| Error::new("BAD_ARGS", format!("--dir: {e}")))?
            }
        },
    };
    Ok(dir)
}

pub fn install(dir: Option<&str>, force: bool) -> Result<Value> {
    let dir = folder(dir)?;
    let target = dir.join(NAME).join("SKILL.md");
    let shown = text::inert(&target.display().to_string(), 300);
    let before = std::fs::read_to_string(&target).ok();
    if before.as_deref() == Some(SKILL) {
        return Ok(answer(&target, false));
    }
    if let Some(before) = &before
        && front(before, "name").as_deref() != Some(NAME)
        && !force
    {
        return Err(
            Error::new("BAD_ARGS", format!("{shown} holds another skill")).hint(
                "Choose another --dir, or pass --force to replace it (only if the user's fine with that).",
            ),
        );
    }
    let fail = |e: std::io::Error| Error::new("FAILED", format!("{shown}: {e}"));
    std::fs::create_dir_all(target.parent().unwrap_or(Path::new("."))).map_err(fail)?;
    files::write_private(&target, SKILL.as_bytes()).map_err(fail)?;
    Ok(answer(&target, true))
}

fn answer(target: &Path, changed: bool) -> Value {
    Value::obj()
        .with("installed", target.display().to_string())
        .with("skill", NAME)
        .with("changed", changed)
        .with(
            "next",
            "Agents that read that folder load it when a task matches its description; one already running may need a new session to see it.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill is the guide under a front matter. After changing
    /// guide.md: `PEPPER_AGENT_BLESS=1 cargo test skill` writes it again.
    #[test]
    fn skill_is_the_guide() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/pepper/SKILL.md");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let guide = crate::GUIDE;
        if body(&on_disk) != Some(guide) && std::env::var_os("PEPPER_AGENT_BLESS").is_some() {
            let head = on_disk.len() - body(&on_disk).map_or(0, str::len);
            std::fs::write(&path, format!("{}{guide}", &on_disk[..head])).unwrap();
            return;
        }
        assert!(
            body(SKILL) == Some(guide),
            "skills/pepper/SKILL.md isn't guide.md: PEPPER_AGENT_BLESS=1 cargo test skill"
        );
        // agentskills.io: the name is its folder's, the description says
        // when to use it, in 1024 characters at most.
        assert_eq!(front(SKILL, "name").as_deref(), Some(NAME));
        assert_eq!(path.parent().unwrap().file_name().unwrap(), NAME);
        let description = front(SKILL, "description").unwrap();
        assert!((1..=1024).contains(&description.chars().count()));
        for word in [
            "video",
            "edit",
            "preview",
            "screenshot",
            "render",
            "stream",
            "demo",
            "compare",
        ] {
            assert!(description.contains(word), "{word}");
        }
        assert!(front(SKILL, "compatibility").unwrap().chars().count() <= 500);
    }

    #[test]
    fn installs_and_updates() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-skill-{}", std::process::id()));
        let given = dir.display().to_string();
        let first = install(Some(&given), false).unwrap();
        assert_eq!(first.get_bool("changed"), Some(true));
        let target = dir.join("pepper/SKILL.md");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SKILL);
        // Again: nothing to do. An older one of ours: replaced.
        assert_eq!(
            install(Some(&given), false).unwrap().get_bool("changed"),
            Some(false)
        );
        std::fs::write(&target, "---\nname: pepper\ndescription: old\n---\nold\n").unwrap();
        assert_eq!(
            install(Some(&given), false).unwrap().get_bool("changed"),
            Some(true)
        );
        // Someone else's: only with --force.
        std::fs::write(&target, "---\nname: other\ndescription: x\n---\n").unwrap();
        assert_eq!(install(Some(&given), false).unwrap_err().code, "BAD_ARGS");
        assert!(install(Some(&given), true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            folder(Some("~/.codex/skills")).unwrap(),
            PathBuf::from(std::env::var("HOME").unwrap()).join(".codex/skills")
        );
    }
}
