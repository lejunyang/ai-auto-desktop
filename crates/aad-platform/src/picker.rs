//! The native dialog's observed operation, checked before selecting any file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Open,
    Save,
}

use std::io;
use std::path::{Component, Path, PathBuf};

/// An absolute, as-yet unused filename below an explicitly granted directory.
/// Pass the resolved path to the native chooser, and check it again immediately
/// before accepting the dialog. A dangling link also occupies a filename.
pub struct NewFile {
    root: PathBuf,
    path: PathBuf,
}

impl NewFile {
    pub fn under(root: &Path, file: &Path) -> io::Result<Self> {
        if !file.is_absolute() || file.components().any(|part| part == Component::ParentDir) {
            return Err(invalid("use an absolute destination without '..'"));
        }
        let root = root.canonicalize()?;
        let parent = file
            .parent()
            .ok_or_else(|| invalid("missing parent directory"))?
            .canonicalize()?;
        let name = file
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("missing UTF-8 filename"))?;
        if parent
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
            || name.chars().any(char::is_control)
            || !parent.starts_with(&root)
            || !root.is_dir()
            || !parent.is_dir()
        {
            return Err(invalid(
                "the new file must be in an existing directory under --allow-save-dir",
            ));
        }
        #[cfg(windows)]
        if !windows_dialog_name(name)
            || !matches!(parent.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
        {
            return Err(invalid(
                "choose a regular filename on a local Windows drive",
            ));
        }
        let destination = Self {
            root,
            path: parent.join(name),
        };
        destination.revalidate()?;
        Ok(destination)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn revalidate(&self) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("missing parent directory"))?;
        if !parent.is_dir()
            || parent.canonicalize()? != parent
            || self.root.canonicalize()? != self.root
        {
            return Err(invalid("the destination directory changed"));
        }
        match self.path.symlink_metadata() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the destination already exists",
            )),
        }
    }
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

#[cfg(any(windows, test))]
fn windows_dialog_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    let numbered = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    !name.ends_with([' ', '.'])
        && !name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !numbered.is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_must_still_be_new_and_inside_the_grant() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("grant");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("新文件.txt");
        let file = NewFile::under(&root, &path).unwrap();
        assert!(NewFile::under(&root, &temp.path().join("outside")).is_err());
        assert!(NewFile::under(&root, &root.join("../grant/name")).is_err());
        std::fs::write(&path, b"keep").unwrap();
        assert_eq!(
            file.revalidate().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
        #[cfg(unix)]
        {
            let multiline = root.join("line\nbreak");
            std::fs::create_dir(&multiline).unwrap();
            assert!(NewFile::under(&root, &multiline.join("new.txt")).is_err());
            let link = root.join("dangling");
            std::os::unix::fs::symlink(temp.path().join("missing"), &link).unwrap();
            assert!(NewFile::under(&root, &link).is_err());
        }
    }

    #[test]
    fn windows_chooser_does_not_treat_device_names_or_streams_as_files() {
        for invalid in [
            "NUL.txt",
            "COM¹.csv",
            "LPT1 .log",
            "file:stream",
            "name.",
            "*.csv",
        ] {
            assert!(!windows_dialog_name(invalid));
        }
        assert!(windows_dialog_name("Company 1.csv"));
    }
}
