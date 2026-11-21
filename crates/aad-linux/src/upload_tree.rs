//! Snapshot an explicitly granted directory without ever following source links.
use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_FILES: usize = 512;
const MAX_DEPTH: usize = 16;

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.contains('\\') && !name.chars().any(char::is_control))
        .ok_or_else(|| refused("upload tree requires plain UTF-8 names"))
}

fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

fn proc_fd(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[derive(Default)]
struct Budget {
    entries: usize,
    files: BTreeMap<PathBuf, u64>,
    bytes: u64,
}

fn copy_children(
    root: &File,
    directory: &File,
    relative: &Path,
    destination: &Path,
    depth: usize,
    budget: &mut Budget,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(refused("upload tree exceeds depth limit"));
    }
    for entry in fs::read_dir(proc_fd(directory))? {
        let filename = entry?.file_name();
        let relative = relative.join(name(Path::new(&filename))?);
        let output = destination.join(&filename);
        budget.entries += 1;
        if budget.entries > MAX_ENTRIES {
            return Err(refused("upload tree exceeds entry limit"));
        }
        let pinned = File::from(openat2(
            root,
            &relative,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )?);
        let metadata = pinned.metadata()?;
        if metadata.is_dir() {
            fs::create_dir(&output)?;
            copy_children(root, &pinned, &relative, &output, depth + 1, budget)?;
        } else if metadata.is_file() && metadata.nlink() == 1 {
            let remaining = MAX_BYTES - budget.bytes;
            if budget.files.len() >= MAX_FILES || metadata.len() > remaining {
                return Err(refused("upload tree exceeds file or byte limit"));
            }
            let mut source = File::open(proc_fd(&pinned))?.take(remaining + 1);
            let size = io::copy(&mut source, &mut File::create_new(output)?)?;
            if size != metadata.len() {
                return Err(refused("upload tree file size changed"));
            }
            budget.bytes += size;
            budget.files.insert(relative, size);
        } else {
            return Err(refused(
                "upload tree accepts only directories and singly linked regular files",
            ));
        }
    }
    Ok(())
}

/// A pinned source root. Only a fresh private snapshot, never the source, is sent to Chromium.
pub struct Grant {
    source: File,
    basename: String,
    stage: PathBuf,
}

impl Grant {
    pub fn new(source: &Path, stage: &Path) -> io::Result<Self> {
        if !absolute(source) || !absolute(stage) || stage.starts_with(source) {
            return Err(refused(
                "use absolute paths and a new staging directory outside the granted tree",
            ));
        }
        let basename = name(source)?.to_owned();
        name(stage)?;
        let uid = rustix::process::geteuid().as_raw();
        let home = std::env::var_os("HOME")
            .and_then(|path| fs::canonicalize(path).ok())
            .filter(|home| {
                stage.starts_with(home) && fs::metadata(home).is_ok_and(|meta| meta.uid() == uid)
            });
        for parent in stage.ancestors().skip(1) {
            let metadata = fs::symlink_metadata(parent)?;
            if !metadata.is_dir()
                || (metadata.uid() != uid
                    && metadata.uid() != 0
                    && !home.as_ref().is_some_and(|home| home.starts_with(parent)))
                || (metadata.mode() & 0o022 != 0
                    && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0))
            {
                return Err(refused(
                    "staging requires a trusted parent without symlinks or shared write access",
                ));
            }
        }
        let source = File::from(openat2(
            CWD,
            source,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )?);
        Ok(Self {
            source,
            basename,
            stage: stage.to_owned(),
        })
    }

    pub fn snapshot(&self) -> io::Result<(PathBuf, BTreeMap<PathBuf, u64>)> {
        fs::DirBuilder::new().mode(0o700).create(&self.stage)?;
        let tree = self.stage.join(&self.basename);
        let result = (|| {
            fs::create_dir(&tree)?;
            let mut budget = Budget::default();
            copy_children(
                &self.source,
                &self.source,
                Path::new(""),
                &tree,
                1,
                &mut budget,
            )?;
            if budget.files.is_empty() {
                return Err(refused("empty upload tree"));
            }
            let manifest = budget
                .files
                .into_iter()
                .map(|(path, size)| (Path::new(&self.basename).join(path), size))
                .collect();
            Ok((tree, manifest))
        })();
        if result.is_err() {
            fs::remove_dir_all(&self.stage)?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("aad-tree-{}", uuid::Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            fs::create_dir(path.join("grant")).unwrap();
            fs::create_dir(path.join("outside")).unwrap();
            fs::write(path.join("outside/private"), b"outside fixture").unwrap();
            Self(path)
        }
        fn grant(&self) -> Grant {
            Grant::new(&self.0.join("grant"), &self.0.join("stage")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replacing_the_granted_root_cannot_redirect_the_snapshot() {
        let fixture = Fixture::new();
        let grant = fixture.grant();
        fs::write(fixture.0.join("grant/ok"), "granted").unwrap();
        fs::rename(fixture.0.join("grant"), fixture.0.join("moved")).unwrap();
        symlink(fixture.0.join("outside"), fixture.0.join("grant")).unwrap();
        assert!(Grant::new(&fixture.0.join("grant"), &fixture.0.join("stage")).is_err());
        let (snapshot, files) = grant.snapshot().unwrap();
        assert_eq!(files, BTreeMap::from([(PathBuf::from("grant/ok"), 7)]));
        assert_eq!(fs::read_to_string(snapshot.join("ok")).unwrap(), "granted");
        assert!(!snapshot.join("private").exists());
        assert!(
            grant.snapshot().is_err(),
            "never reuse an existing staging directory"
        );
    }

    #[test]
    fn unsafe_or_oversized_trees_never_publish_partial_copies() {
        let fixture = Fixture::new();
        let grant = fixture.grant();
        let bad = fixture.0.join("grant/bad");
        for target in ["outside/private", "outside"] {
            symlink(fixture.0.join(target), &bad).unwrap();
            assert!(grant.snapshot().is_err());
            assert!(!fixture.0.join("stage").exists());
            fs::remove_file(&bad).unwrap();
        }
        fs::hard_link(fixture.0.join("outside/private"), &bad).unwrap();
        assert!(grant.snapshot().is_err());
        fs::remove_file(&bad).unwrap();
        File::create(&bad).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(grant.snapshot().is_err());
        assert!(!fixture.0.join("stage").exists());
        fs::remove_file(bad).unwrap();
        assert!(
            grant.snapshot().is_err(),
            "empty selection cannot be confirmed"
        );
        assert!(Grant::new(&fixture.0.join("grant"), &fixture.0.join("grant/stage")).is_err());
    }
}
