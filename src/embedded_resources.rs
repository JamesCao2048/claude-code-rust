use include_dir::{Dir, include_dir};
use std::fs;
use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use tracing::warn;

static EMBEDDED_RESOURCES_DIR: Dir<'_> = include_dir!("$OUT_DIR/lingxi_resources");

const MARKER_FILENAME: &str = ".lingxi_marker";
// Advisory lock file held for the lifetime of an EmbeddedResourceDir.
// cleanup_orphans() probes try_lock on this file to tell live dirs apart
// from stale ones, so a long-running TUI past the 24h age cutoff is not
// deleted underneath itself by a concurrent invocation.
const LOCK_FILENAME: &str = ".lingxi_lock";
const PLUGIN_MANIFEST: &str = r#"{
  "name": "lingxi-ascendc",
  "version": "0.1.0",
  "description": "Lingxi AscendC commands, agents, and skills",
  "author": {
    "name": "Lingxi AscendC"
  }
}
"#;

pub struct EmbeddedResourceDir {
    path: PathBuf,
    // Held for the lifetime of the struct. flock is released on Drop
    // (or when the process exits, even if Drop doesn't run).
    _lock: File,
}

impl EmbeddedResourceDir {
    pub fn extract() -> io::Result<Self> {
        let id = uuid::Uuid::new_v4();
        let base = std::env::temp_dir().join(id.to_string());
        fs::create_dir_all(&base)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
        }

        extract_dir(&EMBEDDED_RESOURCES_DIR, &base)?;
        extract_plugin_layout(&base)?;
        fs::write(base.join(MARKER_FILENAME), id.to_string())?;

        // Take an exclusive advisory lock so cleanup_orphans() from a
        // concurrently-started binary can tell this dir is still in use.
        // The file lives inside the resource dir itself; uuid uniqueness
        // means try_lock should always succeed here — failure means
        // someone else got the same uuid (vanishingly unlikely) or the
        // filesystem doesn't support advisory locks (NFS without lockd).
        let lock_path = base.join(LOCK_FILENAME);
        let lock_file = File::create(&lock_path)?;
        if let Err(e) = lock_file.try_lock() {
            return Err(io::Error::other(format!(
                "failed to acquire resource dir lock at {}: {e:?}",
                lock_path.display(),
            )));
        }

        Ok(Self { path: base, _lock: lock_file })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn cleanup_orphans() {
        let tmp = std::env::temp_dir();
        let Ok(entries) = fs::read_dir(&tmp) else { return };
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let marker = path.join(MARKER_FILENAME);
            if !marker.exists() {
                continue;
            }
            // Skip dirs still held by a live process. A dir with no
            // lock file at all is pre-flock-era state — fall through to
            // the age check. With a lock file present, only proceed
            // when we can take the lock (i.e. no one holds it).
            let lock_path = path.join(LOCK_FILENAME);
            if lock_path.exists() {
                let Ok(probe) = File::open(&lock_path) else { continue };
                match probe.try_lock() {
                    Ok(()) => { /* lock free — proceed to age check */ }
                    Err(TryLockError::WouldBlock) => continue,
                    Err(TryLockError::Error(_)) => continue,
                }
            }
            let Ok(meta) = fs::metadata(&marker) else { continue };
            let Ok(modified) = meta.modified() else { continue };
            if modified < cutoff
                && let Err(e) = fs::remove_dir_all(&path)
            {
                warn!("failed to clean orphan dir {}: {e}", path.display());
            }
        }
    }
}

impl Drop for EmbeddedResourceDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.path) {
            warn!("failed to clean resource dir {}: {e}", self.path.display());
        }
    }
}

/// Metadata for a slash command packaged inside the binary via `include_dir!`.
///
/// `name` is the file stem (e.g. `gen-tilelang`); TUI surfaces prepend the
/// `lingxi-ascendc:` plugin prefix when displaying it to the user.
#[derive(Debug, Clone)]
pub struct EmbeddedCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
}

/// Enumerate the `.claude/commands/*.md` files baked into the binary and
/// return their frontmatter, sorted by command name.
#[must_use]
pub fn list_embedded_commands() -> Vec<EmbeddedCommand> {
    let Some(commands_dir) = EMBEDDED_RESOURCES_DIR.get_dir(".claude/commands") else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    for file in commands_dir.files() {
        if file.path().extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Some(stem) = file.path().file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(content) = file.contents_utf8() else { continue };
        let (description, argument_hint) = parse_command_frontmatter(content);
        commands.push(EmbeddedCommand {
            name: stem.to_owned(),
            description,
            argument_hint,
        });
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn parse_command_frontmatter(content: &str) -> (String, String) {
    let mut iter = content.lines();
    if iter.next().map(str::trim) != Some("---") {
        return (String::new(), String::new());
    }
    let mut description = String::new();
    let mut argument_hint = String::new();
    for line in iter {
        if line.trim() == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("description:") {
            rest.trim().clone_into(&mut description);
        } else if let Some(rest) = line.strip_prefix("argument-hint:") {
            rest.trim().clone_into(&mut argument_hint);
        }
    }
    (description, argument_hint)
}

fn extract_dir(dir: &Dir<'_>, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for file in dir.files() {
        let file_path = target.join(file.path().file_name().unwrap_or_default());
        fs::write(&file_path, file.contents())?;
    }
    for sub_dir in dir.dirs() {
        let name = sub_dir.path().file_name().unwrap_or_default();
        extract_dir(sub_dir, &target.join(name))?;
    }
    Ok(())
}

fn extract_plugin_layout(base: &Path) -> io::Result<()> {
    let plugin_dir = base.join(".claude-plugin");
    fs::create_dir_all(&plugin_dir)?;
    fs::write(plugin_dir.join("plugin.json"), PLUGIN_MANIFEST)?;

    EMBEDDED_RESOURCES_DIR.get_dir(".claude").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "packaged resources missing .claude directory")
    })?;

    for name in ["commands", "agents", "skills"] {
        let resource_path = format!(".claude/{name}");
        if let Some(dir) = EMBEDDED_RESOURCES_DIR.get_dir(&resource_path) {
            extract_dir(dir, &base.join(name))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EmbeddedResourceDir, list_embedded_commands, parse_command_frontmatter};

    #[test]
    fn list_embedded_commands_returns_packaged_slash_commands() {
        let commands = list_embedded_commands();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"gen-tilelang"), "missing gen-tilelang: {names:?}");
        assert!(names.contains(&"gen-ascendc"), "missing gen-ascendc: {names:?}");
        assert!(names.contains(&"verify-env"), "missing verify-env: {names:?}");
        // Sorted output keeps the welcome banner stable across rebuilds.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let gen_til = commands.iter().find(|c| c.name == "gen-tilelang").unwrap();
        assert!(!gen_til.description.is_empty(), "description missing for gen-tilelang");
        assert!(!gen_til.argument_hint.is_empty(), "argument-hint missing for gen-tilelang");
    }

    #[test]
    fn parse_command_frontmatter_extracts_description_and_argument_hint() {
        let body = "---\ndescription: do something\nargument-hint: <arg>\n---\n\nbody";
        let (desc, hint) = parse_command_frontmatter(body);
        assert_eq!(desc, "do something");
        assert_eq!(hint, "<arg>");
    }

    #[test]
    fn parse_command_frontmatter_handles_missing_frontmatter() {
        let (desc, hint) = parse_command_frontmatter("# just markdown");
        assert!(desc.is_empty());
        assert!(hint.is_empty());
    }

    #[test]
    fn parse_command_frontmatter_handles_missing_argument_hint() {
        let body = "---\ndescription: only desc\n---\n";
        let (desc, hint) = parse_command_frontmatter(body);
        assert_eq!(desc, "only desc");
        assert!(hint.is_empty());
    }

    #[test]
    fn extract_creates_local_plugin_layout_for_embedded_claude_resources() {
        let resources = EmbeddedResourceDir::extract().expect("extract embedded resources");
        let root = resources.path();

        // sdk-v1 ships a small set of TUI slash commands (gen-tilelang,
        // gen-ascendc, verify-env) that thin-wrap the CLI subcommands; the
        // canonical entrypoint for batch / CI is still `lingxi-ascendc run`
        // routed via the RESERVED_SUBCOMMANDS dispatcher. The assertion here
        // only checks the resource swap layout (dirs present after extract).
        assert!(root.join(".claude").join("commands").is_dir());
        assert!(root.join(".claude").join("agents").is_dir());
        assert!(root.join(".claude").join("skills").is_dir());
        assert!(root.join("CLAUDE.md").is_file());
        let claude_md =
            std::fs::read_to_string(root.join("CLAUDE.md")).expect("read extracted CLAUDE.md");
        assert!(claude_md.contains("LINGXI-AscendC"));

        let plugin_manifest = root.join(".claude-plugin").join("plugin.json");
        assert!(plugin_manifest.is_file());
        assert!(root.join("commands").is_dir());
        assert!(root.join("agents").is_dir());
        assert!(root.join("skills").is_dir());
        assert!(root.join("PACKAGED_RESOURCES.txt").is_file());
        let manifest =
            std::fs::read_to_string(root.join("PACKAGED_RESOURCES.txt")).expect("read manifest");
        for expected in [
            "as .claude/skills",
            "as .claude/agents",
            "as .claude/commands",
            "as archive_tasks",
            "as scripts",
            "as utils",
            "as templates",
        ] {
            assert!(manifest.contains(expected), "manifest missing {expected}");
        }
    }

    #[test]
    fn extract_includes_manifest_listed_runtime_resources() {
        let resources = EmbeddedResourceDir::extract().expect("extract embedded resources");
        let root = resources.path();

        assert!(root.join("archive_tasks").join("rms_norm").join("model.py").is_file());
        assert!(root.join("scripts").join("lingxi").join("__init__.py").is_file());
        assert!(root.join("scripts").join("lingxi").join("state.py").is_file());
        assert!(root.join("scripts").join("lingxi").join("intake.py").is_file());
        assert!(root.join("scripts").join("lingxi").join("cli.py").is_file());
        assert!(root.join("utils").join("verification_ascendc.py").is_file());
        assert!(root.join("utils").join("verification_tilelang.py").is_file());
    }

    #[test]
    fn agent_skill_resource_references_resolve_under_packaged_root() {
        let resources = EmbeddedResourceDir::extract().expect("extract embedded resources");
        let root = resources.path();

        for relative_path in [
            "archive_tasks/rms_norm/model.py",
            "archive_tasks/matmul_leakyrelu/model.py",
            "utils/build_ascendc.py",
            "utils/verification_ascendc.py",
            "utils/verification_tilelang.py",
        ] {
            assert!(root.join(relative_path).is_file(), "missing packaged path: {relative_path}");
        }
    }
}
