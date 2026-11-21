use crate::{error, Result, Session};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedFile {
    pub filename: String,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
}

pub(super) fn safe_filename(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 255
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|ch| ch.is_control() || r#"\/:*?"<>|"#.contains(ch))
        && Path::new(name).file_name().and_then(|part| part.to_str()) == Some(name)
}

impl ExpectedFile {
    pub fn validate(&self) -> Result<()> {
        if !safe_filename(&self.filename)
            || self
                .bytes
                .is_some_and(|bytes| !(1..=MAX_BYTES).contains(&bytes))
            || self.sha256.as_ref().is_some_and(|hash| {
                hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err(error("BROWSER.EXPORT_INVALID", "expected.filename must be a plain filename; optional bytes must be 1..256 MiB and sha256 exactly 64 hex digits"));
        }
        Ok(())
    }
}

pub struct DownloadCursor {
    directory: PathBuf,
    sequence: u64,
    lost: u64,
}

impl Session {
    pub fn download_cursor(&self, timeout: Duration) -> Result<DownloadCursor> {
        let list = self.browser_downloads(timeout)?;
        if list["tracking"] != true {
            return Err(error(
                "BROWSER.EXPORT_UNTRACKED",
                "download verification requires a session-launched Chromium browser",
            ));
        }
        Ok(DownloadCursor {
            directory: PathBuf::from(list["directory"].as_str().ok_or_else(|| {
                error("BROWSER.EXPORT_UNTRACKED", "no private download directory")
            })?),
            sequence: list["sequence"].as_u64().unwrap(),
            lost: list["lost"].as_u64().unwrap(),
        })
    }

    pub fn poll_export(
        &self,
        cursor: &DownloadCursor,
        frame: &str,
        expected: &ExpectedFile,
        timeout: Duration,
        check: impl Fn() -> Result<()>,
    ) -> Result<Option<Value>> {
        expected.validate()?;
        let list = self.browser_downloads(timeout)?;
        let Some(item) = cursor.candidate(&list, frame, expected)? else {
            return Ok(None);
        };
        let path = item["path"]
            .as_str()
            .filter(|_| item["path_reported"] == true)
            .ok_or_else(|| {
                error(
                    "BROWSER.EXPORT_UNPROVEN_PATH",
                    "Chromium did not report the completed file path; not using a filename guess",
                )
            })?;
        let (bytes, sha256, path) =
            verify_file(&cursor.directory, Path::new(path), expected, item, check)?;
        Ok(Some(
            json!({"guid":item["guid"], "frame_id":frame, "filename":expected.filename, "path":path, "bytes":bytes, "sha256":sha256,
            "expected_bytes_checked":expected.bytes.is_some(),"expected_sha256_checked":expected.sha256.is_some()}),
        ))
    }
}

impl DownloadCursor {
    fn candidate<'a>(
        &self,
        list: &'a Value,
        frame: &str,
        expected: &ExpectedFile,
    ) -> Result<Option<&'a Value>> {
        let items = list["downloads"]
            .as_array()
            .ok_or_else(|| error("BROWSER.EXPORT_UNTRACKED", "missing download history"))?;
        if list["tracking"] != true
            || list["directory"] != json!(self.directory)
            || list["lost"] != self.lost
            || list["sequence"]
                .as_u64()
                .and_then(|seq| seq.checked_sub(self.sequence))
                .is_none_or(|count| count > items.len() as u64)
        {
            return Err(error(
                "BROWSER.EXPORT_HISTORY_LOST",
                "download connection or history changed; association not provable",
            ));
        }
        let mut fresh = items.iter().filter(|item| {
            item["sequence"]
                .as_u64()
                .is_some_and(|seq| seq > self.sequence)
                && item["frame_id"] == frame
        });
        let Some(item) = fresh.next() else {
            return Ok(None);
        };
        if fresh.next().is_some() {
            return Err(error(
                "BROWSER.EXPORT_AMBIGUOUS",
                "multiple downloads began in the task frame after the checkpoint",
            ));
        }
        if item["suggested_filename"] != expected.filename {
            return Err(error(
                "BROWSER.EXPORT_MISMATCH",
                "download suggested filename differs from expected.filename",
            ));
        }
        match item["state"].as_str() {
            Some("in_progress") => Ok(None),
            Some("completed") => Ok(Some(item)),
            _ => Err(error(
                "BROWSER.EXPORT_INCOMPLETE",
                "download did not complete",
            )),
        }
    }
}

fn verify_file(
    directory: &Path,
    path: &Path,
    expected: &ExpectedFile,
    item: &Value,
    check: impl Fn() -> Result<()>,
) -> Result<(u64, String, PathBuf)> {
    let unsafe_path = || {
        error(
            "BROWSER.EXPORT_UNSAFE_PATH",
            "expected a regular file directly in the private session download directory",
        )
    };
    let mismatch = || {
        error(
            "BROWSER.EXPORT_MISMATCH",
            "download size or digest does not match the expected file or Chromium completion",
        )
    };
    if !directory.symlink_metadata().is_ok_and(|meta| meta.is_dir())
        || path.parent().and_then(|p| p.canonicalize().ok()).as_deref() != Some(directory)
        || !path
            .file_name()
            .and_then(|part| part.to_str())
            .is_some_and(safe_filename)
        || !path.symlink_metadata().is_ok_and(|meta| meta.is_file())
    {
        return Err(unsafe_path());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, 0x00200000);
    let mut file = options.open(path).map_err(|_| unsafe_path())?;
    let meta = file.metadata().map_err(|_| unsafe_path())?;
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
        return Err(unsafe_path());
    }
    #[cfg(windows)]
    if std::os::windows::fs::MetadataExt::file_attributes(&meta) & 0x400 != 0 {
        return Err(unsafe_path());
    }
    if !meta.is_file() {
        return Err(unsafe_path());
    }
    let size = meta.len();
    if !(1..=MAX_BYTES).contains(&size)
        || expected.bytes.is_some_and(|want| want != size)
        || item["received_bytes"].as_f64() != Some(size as f64)
        || item["total_bytes"]
            .as_f64()
            .is_some_and(|total| total != size as f64)
    {
        return Err(mismatch());
    }
    let output_error = || {
        error(
            "BROWSER.EXPORT_SAVE_FAILED",
            "could not preserve a verified copy in the private session directory",
        )
    };
    let artifact_dir = directory.join(format!("verified-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&artifact_dir).map_err(|_| output_error())?;
    let suffix = Path::new(&expected.filename)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| s.len() <= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let artifact = artifact_dir.join(format!("export.{suffix}"));
    let result = (|| {
        let mut output = std::fs::File::create_new(&artifact).map_err(|_| output_error())?;
        let mut hash = Sha256::new();
        let mut read = 0;
        let mut buf = [0u8; 32768];
        loop {
            check()?;
            let count = file.read(&mut buf).map_err(|_| unsafe_path())?;
            if count == 0 {
                break;
            }
            read += count as u64;
            if read > size {
                return Err(mismatch());
            }
            hash.update(&buf[..count]);
            output
                .write_all(&buf[..count])
                .map_err(|_| output_error())?;
        }
        let digest = format!("{:x}", hash.finalize());
        if read != size
            || expected
                .sha256
                .as_ref()
                .is_some_and(|want| !digest.eq_ignore_ascii_case(want))
        {
            return Err(mismatch());
        }
        output.sync_all().map_err(|_| output_error())?;
        Ok((size, digest, artifact.clone()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(artifact);
        let _ = std::fs::remove_dir(artifact_dir);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> ExpectedFile {
        ExpectedFile {
            filename: "bill.csv".into(),
            bytes: Some(3),
            sha256: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()),
        }
    }

    #[test]
    fn fresh_download_must_be_unique_in_the_same_frame_with_intact_history() {
        let cursor = DownloadCursor {
            directory: "owned".into(),
            sequence: 5,
            lost: 1,
        };
        let mut list = json!({"directory":"owned","tracking":true,"sequence":7,"lost":1,"downloads":[
            {"frame_id":"main","sequence":4,"suggested_filename":"bill.csv","state":"completed"},
            {"frame_id":"other","sequence":6,"suggested_filename":"bill.csv","state":"completed"},
            {"frame_id":"main","sequence":7,"suggested_filename":"bill.csv","state":"in_progress"}]});
        assert!(cursor
            .candidate(&list, "main", &expected())
            .unwrap()
            .is_none());
        list["downloads"][2]["state"] = json!("completed");
        assert_eq!(
            cursor
                .candidate(&list, "main", &expected())
                .unwrap()
                .unwrap()["sequence"],
            7
        );
        list["downloads"][2]["state"] = json!("canceled");
        assert_eq!(
            cursor
                .candidate(&list, "main", &expected())
                .unwrap_err()
                .code,
            "BROWSER.EXPORT_INCOMPLETE"
        );
        list["downloads"][1]["frame_id"] = json!("main");
        assert_eq!(
            cursor
                .candidate(&list, "main", &expected())
                .unwrap_err()
                .code,
            "BROWSER.EXPORT_AMBIGUOUS"
        );
        list["sequence"] = json!(10);
        assert_eq!(
            cursor
                .candidate(&list, "main", &expected())
                .unwrap_err()
                .code,
            "BROWSER.EXPORT_HISTORY_LOST"
        );
    }

    #[test]
    fn verify_the_completed_file_not_a_path_or_content_supplied_by_the_page() {
        use std::fs;
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&dir).unwrap();
        let path = dir.join("bill (1).csv");
        fs::write(&path, "abc").unwrap();
        let item = json!({"received_bytes":3.0,"total_bytes":3.0});
        let verify = |file: &Path| verify_file(&dir, file, &expected(), &item, || Ok(()));
        let (bytes, _, artifact) = verify(&path).unwrap();
        assert_eq!(bytes, 3);
        fs::write(&path, "bad").unwrap();
        assert_eq!(fs::read(artifact).unwrap(), b"abc");
        assert_eq!(verify(&path).unwrap_err().code, "BROWSER.EXPORT_MISMATCH");
        assert!(verify(&dir).is_err());
        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(
                verify(&link).unwrap_err().code,
                "BROWSER.EXPORT_UNSAFE_PATH"
            );
        }
        fs::remove_dir_all(dir).unwrap();
        for bad in ["", ".", "..", "../bill", "a/b", "a\\b", "C:ads", "bill."] {
            assert!(!safe_filename(bad));
        }
    }
}
