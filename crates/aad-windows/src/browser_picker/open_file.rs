use super::{ordinary_path, remaining, FilePicker, Kind, Result};
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Instant;
use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    FILE_SHARE_READ,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_LINK};
use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};

fn identity(file: &File) -> Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )?;
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

// A filesystem file may still be a shortcut or a browsable Shell folder (e.g. a ZIP).
fn regular_shell_item(ordinary: &str, directory: bool) -> Result<()> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(ordinary), None)?;
        let attrs = item.GetAttributes(SFGAO_FILESYSTEM | SFGAO_FOLDER | SFGAO_LINK)?;
        let expected = if directory {
            SFGAO_FILESYSTEM | SFGAO_FOLDER
        } else {
            SFGAO_FILESYSTEM
        };
        if attrs != expected {
            return Err("unexpected Shell item type for the selected path".into());
        }
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let same = name.as_wide().iter().copied().eq(ordinary.encode_utf16());
        CoTaskMemFree(Some(name.0.cast()));
        if !same {
            return Err("the Shell resolved the filename to a different path".into());
        }
    }
    Ok(())
}

impl FilePicker {
    pub fn choose_directory(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Directory {
            return Err("expected an observed directory dialog".into());
        }
        remaining(deadline)?;
        let canonical = path.canonicalize()?;
        let name = ordinary_path(path, &canonical)?;
        let open = || {
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                .open(&canonical)
        };
        let held = open()?;
        if !held.metadata()?.is_dir() {
            return Err("expected an existing directory".into());
        }
        let original = identity(&held)?;
        let verify = || -> Result<()> {
            remaining(deadline)?;
            if path.canonicalize()? != canonical || identity(&open()?)? != original {
                return Err("the granted directory changed".into());
            }
            regular_shell_item(name, true)
        };
        verify()?;
        self.choose_path(name, deadline, verify)
    }
    /// Submit one already-granted local file through an observed native Open dialog.
    pub fn open_file(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("native file selection requires an observed Open dialog".into());
        }
        remaining(deadline)?;
        let canonical = path.canonicalize()?;
        let name = ordinary_path(path, &canonical)?;
        // The Common Item Dialog can follow shortcuts even if the file itself
        // exists and is within the grant. Refuse Internet shortcuts as well.
        if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| {
                ["lnk", "url", "website", "pif"]
                    .iter()
                    .any(|s| ext.eq_ignore_ascii_case(s))
            })
        {
            return Err("shell shortcuts cannot be submitted as native files".into());
        }
        // Keep this file open without write/delete sharing through modal closure.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&canonical)?;
        if !held.metadata()?.is_file() {
            return Err("expected an existing regular file".into());
        }
        let original = identity(&held)?;
        let verify = || -> Result<()> {
            remaining(deadline)?;
            if path.canonicalize()? != canonical || identity(&File::open(name)?)? != original {
                return Err("the granted file changed before selection".into());
            }
            regular_shell_item(name, false)
        };
        verify()?;
        self.choose_path(name, deadline, verify)
    }
}
