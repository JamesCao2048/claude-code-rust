use include_dir::{Dir, include_dir};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::warn;

static EMBEDDED_CLAUDE_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../.claude");
static EMBEDDED_CLAUDE_MD: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../CLAUDE.md"));

const MARKER_FILENAME: &str = ".lingxi_marker";
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

        let claude_dir = base.join(".claude");
        extract_dir(&EMBEDDED_CLAUDE_DIR, &claude_dir)?;
        fs::write(base.join("CLAUDE.md"), EMBEDDED_CLAUDE_MD)?;
        extract_plugin_layout(&base)?;
        fs::write(base.join(MARKER_FILENAME), id.to_string())?;

        Ok(Self { path: base })
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

fn extract_dir(dir: &Dir<'_>, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for file in dir.files() {
        let file_path = target.join(file.path().file_name().unwrap_or_default());
        fs::write(&file_path, file.contents())?;
    }
    for sub_dir in dir.dirs() {
        let name = sub_dir.path().file_name().unwrap_or_default();
        if name.to_string_lossy() == "images" {
            continue;
        }
        extract_dir(sub_dir, &target.join(name))?;
    }
    Ok(())
}

fn extract_plugin_layout(base: &Path) -> io::Result<()> {
    let plugin_dir = base.join(".claude-plugin");
    fs::create_dir_all(&plugin_dir)?;
    fs::write(plugin_dir.join("plugin.json"), PLUGIN_MANIFEST)?;

    for name in ["commands", "agents", "skills"] {
        if let Some(dir) = EMBEDDED_CLAUDE_DIR.get_dir(name) {
            extract_dir(dir, &base.join(name))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::EmbeddedResourceDir;

    #[test]
    fn extract_creates_local_plugin_layout_for_embedded_claude_resources() {
        let resources = EmbeddedResourceDir::extract().expect("extract embedded resources");
        let root = resources.path();

        assert!(root.join(".claude").join("commands").join("gen.md").is_file());
        assert!(root.join(".claude").join("agents").is_dir());
        assert!(root.join(".claude").join("skills").is_dir());
        assert!(root.join("CLAUDE.md").is_file());
        let claude_md =
            std::fs::read_to_string(root.join("CLAUDE.md")).expect("read extracted CLAUDE.md");
        assert!(claude_md.contains("LINGXI-AscendC"));

        let plugin_manifest = root.join(".claude-plugin").join("plugin.json");
        assert!(plugin_manifest.is_file());
        assert!(root.join("commands").join("gen.md").is_file());
        assert!(root.join("agents").is_dir());
        assert!(root.join("skills").is_dir());
    }
}
