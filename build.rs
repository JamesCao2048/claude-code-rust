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
        let source = repo_root.join(&entry.path);
        let target = out_root.join(&entry.path);
        validate_resource_path(&entry.path)?;

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
    path: PathBuf,
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
        let path = parts.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing resource path on line {}", index + 1),
            )
        })?;
        if parts.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected extra fields on line {}", index + 1),
            ));
        }

        entries.push(ResourceEntry { kind, path: PathBuf::from(path) });
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
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
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
