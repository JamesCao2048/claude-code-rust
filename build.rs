use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

const MANIFEST_FILE: &str = "PACKAGED_RESOURCES.txt";

fn main() -> io::Result<()> {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(io::Error::other)?);
    let repo_root =
        crate_dir.parent().ok_or_else(|| io::Error::other("missing parent repository root"))?;
    let manifest_path = repo_root.join(MANIFEST_FILE);
    let out_root =
        PathBuf::from(env::var("OUT_DIR").map_err(io::Error::other)?).join("lingxi_resources");

    println!("cargo:rerun-if-changed={}", manifest_path.display());

    if out_root.exists() {
        fs::remove_dir_all(&out_root)?;
    }
    fs::create_dir_all(&out_root)?;
    fs::copy(&manifest_path, out_root.join(MANIFEST_FILE))?;

    let manifest = fs::read_to_string(&manifest_path)?;
    for entry in parse_manifest(&manifest)? {
        let source = repo_root.join(&entry.source);
        let dest = entry.dest.as_ref().unwrap_or(&entry.source);
        let target = out_root.join(dest);
        validate_resource_path(&entry.source)?;
        validate_resource_path(dest)?;

        match entry.kind {
            ResourceKind::File => {
                println!("cargo:rerun-if-changed={}", source.display());
                copy_file(&source, &target)?;
            }
            ResourceKind::Dir => {
                println!("cargo:rerun-if-changed={}", source.display());
                copy_dir_recursive(&source, &target)?;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ResourceKind {
    File,
    Dir,
}

#[derive(Debug)]
struct ResourceEntry {
    kind: ResourceKind,
    source: PathBuf,
    dest: Option<PathBuf>,
}

fn parse_manifest(content: &str) -> io::Result<Vec<ResourceEntry>> {
    let mut entries = Vec::new();
    for (index, raw_line) in content.lines().enumerate() {
        let line = raw_line.split_once('#').map_or(raw_line, |(value, _)| value).trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let kind = match parts.next() {
            Some("file") => ResourceKind::File,
            Some("dir") => ResourceKind::Dir,
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid resource kind `{other}` on line {}", index + 1),
                ));
            }
            None => continue,
        };
        let source = parts.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing resource path on line {}", index + 1),
            )
        })?;

        let dest = match parts.next() {
            None => None,
            Some("as") => {
                let dest_path = parts.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("missing destination path after `as` on line {}", index + 1),
                    )
                })?;
                Some(PathBuf::from(dest_path))
            }
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected `as <dest>` or end of line, got `{other}` on line {}", index + 1),
                ));
            }
        };

        if parts.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected extra fields on line {}", index + 1),
            ));
        }

        entries.push(ResourceEntry { kind, source: PathBuf::from(source), dest });
    }
    Ok(entries)
}

fn validate_resource_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty resource path"));
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("resource path must be repository-relative: {}", path.display()),
        ));
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("resource file does not exist: {}", source.display()),
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, target)?;
    Ok(())
}

fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("resource directory does not exist: {}", source.display()),
        ));
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if should_skip_resource(&name) {
            continue;
        }
        let source_path = entry.path();
        let target_path = target.join(&name_os);
        if source_path.is_dir() {
            println!("cargo:rerun-if-changed={}", source_path.display());
            copy_dir_recursive(&source_path, &target_path)?;
        } else if source_path.is_file() {
            println!("cargo:rerun-if-changed={}", source_path.display());
            copy_file(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn should_skip_resource(name: &str) -> bool {
    // Hardcoded skip list applied to every directory entry encountered while
    // walking a `dir` manifest entry. Keeps git worktrees, dev-only state, and
    // platform junk out of the embedded resource tree.
    matches!(
        name,
        "worktrees"
            | ".worktrees"
            | ".git"
            | "__pycache__"
            | ".DS_Store"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | "settings.local.json"
            | ".lingxi"
            | "node_modules"
    ) || name.starts_with("._")
        || Path::new(name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pyc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── parse_manifest unit tests ──────────────────────────────────────────

    #[test]
    fn parse_dir_as_dest() {
        let manifest = "dir foo/bar as baz\n";
        let entries = parse_manifest(manifest).expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.kind, ResourceKind::Dir));
        assert_eq!(e.source, PathBuf::from("foo/bar"));
        assert_eq!(e.dest, Some(PathBuf::from("baz")));
    }

    #[test]
    fn parse_file_as_dest() {
        let manifest = "file a/b.md as CLAUDE.md\n";
        let entries = parse_manifest(manifest).expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.kind, ResourceKind::File));
        assert_eq!(e.source, PathBuf::from("a/b.md"));
        assert_eq!(e.dest, Some(PathBuf::from("CLAUDE.md")));
    }

    #[test]
    fn parse_dir_no_dest() {
        let manifest = "dir some/path\n";
        let entries = parse_manifest(manifest).expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.kind, ResourceKind::Dir));
        assert_eq!(e.source, PathBuf::from("some/path"));
        assert!(e.dest.is_none());
    }

    #[test]
    fn parse_rejects_dir_as_missing_dest() {
        // "dir foo as" — `as` keyword present but no destination path
        let manifest = "dir foo as\n";
        let result = parse_manifest(manifest);
        assert!(result.is_err(), "should reject missing destination after `as`");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("missing destination path"),
            "error message should mention missing destination: {msg}"
        );
    }

    #[test]
    fn parse_rejects_unknown_keyword() {
        let manifest = "dir foo bar\n";
        let result = parse_manifest(manifest);
        assert!(result.is_err(), "should reject unexpected token after source");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("expected `as <dest>`"), "got: {msg}");
    }

    #[test]
    fn parse_rejects_extra_fields() {
        let manifest = "dir foo as bar extra\n";
        let result = parse_manifest(manifest);
        assert!(result.is_err(), "should reject extra fields after dest");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unexpected extra fields"), "got: {msg}");
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let manifest = "\n# comment\nfile a.md\n\n# another\n";
        let entries = parse_manifest(manifest).expect("parse should succeed");
        assert_eq!(entries.len(), 1);
    }

    // ── copy_dir_recursive + dir-as-dest integration test ─────────────────

    #[test]
    fn dir_as_dest_recursive_copy() {
        // Fixture: tests/fixtures/dir_as_test/{a, b/c}
        // Verify: copy_dir_recursive copies them under a remapped destination.
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let fixture_src = manifest_dir.join("tests/fixtures/dir_as_test");
        assert!(fixture_src.is_dir(), "fixture dir must exist: {}", fixture_src.display());

        let tmp = std::env::temp_dir().join(format!("lingxi_build_test_{}", std::process::id()));
        let dest_root = tmp.join("dest_name");
        if tmp.exists() {
            fs::remove_dir_all(&tmp).unwrap();
        }

        // Simulate what build.rs main() does for a `dir foo/bar as dest_name` entry.
        copy_dir_recursive(&fixture_src, &dest_root).expect("copy should succeed");

        assert!(dest_root.join("a").is_file(), "file 'a' must exist under dest");
        assert!(dest_root.join("b").is_dir(), "subdir 'b' must exist under dest");
        assert!(dest_root.join("b/c").is_file(), "file 'b/c' must exist under dest");

        // Verify content is faithfully copied.
        assert_eq!(
            fs::read_to_string(dest_root.join("a")).unwrap().trim(),
            "content a"
        );
        assert_eq!(
            fs::read_to_string(dest_root.join("b/c")).unwrap().trim(),
            "content c"
        );

        fs::remove_dir_all(&tmp).unwrap();
    }

    // ── conflict behaviour (last-wins) ────────────────────────────────────

    #[test]
    fn dir_as_same_dest_last_wins() {
        // Two `dir A as X` + `dir B as X` entries: last writer wins.
        // The manifest parser itself does not error on this; the conflict is
        // resolved at copy time (second copy overwrites first). This test
        // locks that "last-wins" behaviour.
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let fixture_src = manifest_dir.join("tests/fixtures/dir_as_test");

        let tmp = std::env::temp_dir().join(format!("lingxi_conflict_test_{}", std::process::id()));
        let dest_root = tmp.join("X");
        if tmp.exists() {
            fs::remove_dir_all(&tmp).unwrap();
        }

        // First copy from fixture (contains file `a` with "content a")
        copy_dir_recursive(&fixture_src, &dest_root).expect("first copy");

        // Second copy: a different source directory that only has `a` with different content.
        let src2 = tmp.join("src2");
        fs::create_dir_all(&src2).unwrap();
        fs::write(src2.join("a"), "content from src2").unwrap();
        copy_dir_recursive(&src2, &dest_root).expect("second copy");

        // The second copy's content should have overwritten the first.
        let content = fs::read_to_string(dest_root.join("a")).unwrap();
        assert_eq!(content.trim(), "content from src2", "last-wins: second copy overwrites first");

        fs::remove_dir_all(&tmp).unwrap();
    }
}
