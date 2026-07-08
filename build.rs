use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const MANIFEST_FILE: &str = "PACKAGED_RESOURCES.txt";

fn main() -> io::Result<()> {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(io::Error::other)?);
    let repo_root =
        crate_dir.parent().ok_or_else(|| io::Error::other("missing parent repository root"))?;
    let manifest_path = repo_root.join(MANIFEST_FILE);
    let out_root =
        PathBuf::from(env::var("OUT_DIR").map_err(io::Error::other)?).join("lingxi_resources");

    emit_build_metadata(repo_root, &crate_dir);

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

        // Emit before the optional-skip check so a later `git submodule
        // update --init` that brings the source into existence re-triggers
        // packaging.
        println!("cargo:rerun-if-changed={}", source.display());

        // Optional entries (`file?` / `dir?`) whose source is absent are
        // skipped with a build warning instead of failing. Used for the
        // Z-Search submodule so contributors without access to it can still
        // build — they get a binary without the deep-optimize backend.
        if skip_missing_optional(entry.optional, &source) {
            println!(
                "cargo:warning=optional resource missing, skipping (build will lack it): {}",
                source.display()
            );
            continue;
        }

        match entry.kind {
            ResourceKind::File => copy_file(&source, &target)?,
            ResourceKind::Dir => copy_dir_recursive(&source, &target)?,
        }
    }

    Ok(())
}

/// An optional resource whose source is missing is skipped (see main loop);
/// a required resource never is (`copy_file` / `copy_dir_recursive` then fail
/// loudly). Extracted so the skip decision is unit-testable.
fn skip_missing_optional(optional: bool, source: &Path) -> bool {
    optional && !source.exists()
}

#[derive(Debug, Clone, Copy)]
enum ResourceKind {
    File,
    Dir,
}

#[derive(Debug)]
struct ResourceEntry {
    kind: ResourceKind,
    // `file?` / `dir?`: skip with a warning when the source is missing instead
    // of failing the build (used for the optional Z-Search submodule subset).
    optional: bool,
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
        let (kind, optional) = match parts.next() {
            Some("file") => (ResourceKind::File, false),
            Some("file?") => (ResourceKind::File, true),
            Some("dir") => (ResourceKind::Dir, false),
            Some("dir?") => (ResourceKind::Dir, true),
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

        entries.push(ResourceEntry { kind, optional, source: PathBuf::from(source), dest });
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

// Emit LINGXI_BUILD_SHA + LINGXI_BUILD_DIRTY as rustc env vars so the binary
// can print which commit it was built from. Falls back to "unknown" when git
// isn't available (e.g. tarball builds) — the binary still prints a version,
// just without provenance.
fn emit_build_metadata(repo_root: &Path, submodule_dir: &Path) {
    let parent_sha = git_short_sha(repo_root).unwrap_or_else(|| "unknown".to_owned());
    let submodule_sha = git_short_sha(submodule_dir).unwrap_or_else(|| "unknown".to_owned());
    let dirty_suffix = if git_is_dirty(repo_root) || git_is_dirty(submodule_dir) {
        "-dirty"
    } else {
        ""
    };
    let crate_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let version_string =
        format!("{crate_version} ({parent_sha}+sub:{submodule_sha}{dirty_suffix})");
    println!("cargo:rustc-env=LINGXI_BUILD_SHA={parent_sha}");
    println!("cargo:rustc-env=LINGXI_BUILD_SUBMODULE_SHA={submodule_sha}");
    println!("cargo:rustc-env=LINGXI_BUILD_DIRTY={}", if dirty_suffix.is_empty() { "0" } else { "1" });
    println!("cargo:rustc-env=LINGXI_VERSION_STRING={version_string}");
    // Trigger rebuild when HEAD moves. Submodule HEAD lives under parent's
    // .git/modules so the parent HEAD path covers most cases; explicitly
    // watch the submodule's gitdir pointer file too.
    println!("cargo:rerun-if-changed={}/.git/HEAD", repo_root.display());
    println!("cargo:rerun-if-changed={}/.git", submodule_dir.display());
}

fn git_short_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn git_is_dirty(dir: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .ok()
        .is_some_and(|out| out.status.success() && !out.stdout.is_empty())
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

    // ── optional resource (`file?` / `dir?`) parsing + skip semantics ─────

    #[test]
    fn parse_plain_kinds_are_required() {
        let entries = parse_manifest("file a.md\ndir some/path\n").expect("parse should succeed");
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].optional, "plain `file` must be required");
        assert!(!entries[1].optional, "plain `dir` must be required");
    }

    #[test]
    fn parse_optional_file() {
        let entries = parse_manifest("file? a/b.json as c/d.json\n").expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.kind, ResourceKind::File));
        assert!(e.optional, "`file?` must be optional");
        assert_eq!(e.source, PathBuf::from("a/b.json"));
        assert_eq!(e.dest, Some(PathBuf::from("c/d.json")));
    }

    #[test]
    fn parse_optional_dir() {
        let entries = parse_manifest("dir? foo/bar as baz\n").expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.kind, ResourceKind::Dir));
        assert!(e.optional, "`dir?` must be optional");
        assert_eq!(e.source, PathBuf::from("foo/bar"));
        assert_eq!(e.dest, Some(PathBuf::from("baz")));
    }

    #[test]
    fn skip_missing_optional_semantics() {
        let missing = std::env::temp_dir().join("lingxi_definitely_missing_resource_xyz");
        assert!(!missing.exists(), "precondition: fixture path must not exist");
        // Optional + missing => skip; required + missing => do NOT skip (caller
        // then fails loudly). Present sources are never skipped.
        assert!(skip_missing_optional(true, &missing), "optional missing => skip");
        assert!(!skip_missing_optional(false, &missing), "required missing => must not skip");
        let present = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        assert!(!skip_missing_optional(true, &present), "optional present => must not skip");
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
