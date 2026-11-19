//! Clipboard content passed between a viewer and this side, beyond typed text.
//!
//! A paste: the viewer sends `{"type":"paste","id":..,"formats":{"text/plain":..,
//! "text/html":..},"files":[{"name":..,"type":..,"size":..}]}`, then each file's
//! bytes in binary frames (see [`chunk`]). Files are written to this run's
//! scratch folder; once every byte is in, the paste is handed to the host,
//! which puts every format on the clipboard at once and presses the paste key.
//! The server answers with `{"type":"paste","id":..,"ack":<bytes>}` every
//! [`ACK_EVERY`] bytes, so the viewer keeps at most a few megabytes in flight
//! and its input is never stuck behind a large file, and with
//! `{"type":"paste","id":..,"state":"received|pasted|failed|cancelled"}`.
//! `{"type":"paste","id":..,"cancel":true}` stops an upload and deletes what
//! came of it.
//!
//! A `failed` answer has a `message` in English and may have a `code` for
//! the viewer to say in its own language: `contended` (another program kept
//! taking the clipboard), `lost` (another program took it before the program
//! in front read it), `not_asked` (the program in front did not ask for it).
//! When the clipboard did not work and the paste was plain text, the text is
//! typed instead: `pasted` then has `"typed":true` and the `code` of why.
//! Where the platform says more (X11: who asked for the clipboard, who took
//! it away), the answer carries it in `detail`, for telling failures apart.
use aad_platform::clip::{safe_file_name, Clip};
use serde_json::{json, Value};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What a paste or a copy may carry.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of text and HTML together.
    pub text: usize,
    /// Bytes of files together; 0 passes no files.
    pub files: u64,
    /// Files in one paste.
    pub count: usize,
}

impl Limits {
    pub fn new(max_mb: u64) -> Self {
        Self {
            text: 8 * 1024 * 1024,
            files: max_mb.saturating_mul(1024 * 1024),
            count: 100,
        }
    }

    pub fn describe(&self) -> Value {
        json!({"text": self.text, "files": self.files, "count": self.count})
    }
}

/// Bytes between acknowledgements of an upload.
pub const ACK_EVERY: u64 = 4 * 1024 * 1024;
/// The most bytes one binary frame of an upload carries.
pub const CHUNK: usize = 256 * 1024;
/// A picture is also offered as a bitmap only up to this size: decoding a
/// larger one would hold a lot of memory for a format few want.
const MAX_BITMAP_FILE: u64 = 64 * 1024 * 1024;

/// This run's scratch folder, `<temp>/aad-assist-<pid>`, deleted when the
/// run ends. A lock file beside it, held while the run lives, tells a later
/// run that a folder left behind by one that crashed can go.
pub struct Scratch {
    root: PathBuf,
    lock: Option<(File, PathBuf)>,
}

impl Scratch {
    pub fn create() -> Result<Self, String> {
        let temp = std::env::temp_dir();
        sweep(&temp);
        Self::named(&temp, &format!("aad-assist-{}", std::process::id()))
    }

    fn named(temp: &Path, name: &str) -> Result<Self, String> {
        let root = temp.join(name);
        let lock_path = temp.join(format!("{name}.lock"));
        let lock = File::create(&lock_path).map_err(|e| format!("scratch folder: {e}"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|e| format!("scratch folder is in use: {e}"))?;
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("paste")).map_err(|e| format!("scratch folder: {e}"))?;
        Ok(Self {
            root,
            lock: Some((lock, lock_path)),
        })
    }

    /// A new, empty folder for one paste.
    pub fn paste_dir(&self, id: &str) -> Result<PathBuf, String> {
        let dir = self.root.join("paste").join(id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot store the paste: {e}"))?;
        Ok(dir)
    }

    /// Deletes pastes older than `age`: a program pasted into has long
    /// copied what it wanted by then.
    pub fn forget_older(&self, age: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(self.root.join("paste")) else {
            return;
        };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|elapsed| elapsed > age);
            if old {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        if let Some((lock, path)) = self.lock.take() {
            drop(lock);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Removes the scratch folders of runs that are gone: their lock file can
/// be locked again.
fn sweep(temp: &Path) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name
            .strip_prefix("aad-assist-")
            .and_then(|rest| rest.strip_suffix(".lock"))
        else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(file) = File::options().read(true).write(true).open(entry.path()) else {
            continue;
        };
        if fs2::FileExt::try_lock_exclusive(&file).is_ok() {
            let _ = std::fs::remove_dir_all(temp.join(format!("aad-assist-{stem}")));
            drop(file);
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One file of an upload.
struct Incoming {
    path: PathBuf,
    size: u64,
    written: u64,
    file: Option<File>,
    mime: String,
}

/// A paste whose files are still arriving.
struct Upload {
    id: String,
    dir: PathBuf,
    clip: Clip,
    files: Vec<Incoming>,
    received: u64,
    acked: u64,
}

impl Upload {
    fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    fn complete(&self) -> bool {
        self.files.iter().all(|f| f.written == f.size)
    }

    /// The clipboard content, once every file is in: the files as a file
    /// list, and a single picture also as a bitmap.
    fn finish(mut self) -> Ready {
        for file in &mut self.files {
            file.file = None;
        }
        let mut clip = std::mem::take(&mut self.clip);
        clip.files = self.files.iter().map(|f| f.path.clone()).collect();
        if let [only] = self.files.as_slice() {
            if only.mime.starts_with("image/") && only.size <= MAX_BITMAP_FILE {
                clip.image = picture(&only.path);
            }
        }
        // The files stay for the paste: the program pasted into may still
        // be copying them after it.
        self.dir = PathBuf::new();
        let ready = Ready {
            id: std::mem::take(&mut self.id),
            clip,
        };
        // Nothing to delete now: the files are for the paste.
        self.files.clear();
        ready
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        for file in &mut self.files {
            file.file = None;
        }
        if !self.dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// A picture file as PNG bytes, converted when it is another format the
/// image library reads; `None` when it is not a picture after all.
fn picture(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(bytes);
    }
    let image = image::load_from_memory(&bytes).ok()?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// A paste with everything in, for the host to put on the clipboard.
pub struct Ready {
    pub id: String,
    pub clip: Clip,
}

/// The pastes of one viewer, on its reading thread.
#[derive(Default)]
pub struct Uploads {
    current: Option<Upload>,
}

/// What an incoming message or frame led to.
pub struct Outcome {
    /// Messages for the viewer.
    pub replies: Vec<Value>,
    pub ready: Option<Ready>,
}

impl Outcome {
    /// A paste refused as a whole.
    pub fn refused(message: &Value, why: &str) -> Self {
        if message["cancel"] == true {
            return Self {
                replies: Vec::new(),
                ready: None,
            };
        }
        Self::reply(failed(message["id"].as_str().unwrap_or_default(), why))
    }

    fn reply(message: Value) -> Self {
        Self {
            replies: vec![message],
            ready: None,
        }
    }
}

pub fn failed(id: &str, message: impl Into<String>) -> Value {
    json!({"type": "paste", "id": id, "state": "failed", "message": message.into()})
}

/// [`failed`] with a `code` a viewer can say in its own language.
pub fn failed_because(id: &str, code: &str, message: impl Into<String>) -> Value {
    let mut answer = failed(id, message);
    answer["code"] = code.into();
    answer
}

/// A paste id: what the viewer chose, if it is short and plain.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The string formats a paste carries, by the names the clipboard knows.
fn formats(formats: &Value, limits: &Limits) -> Result<Clip, String> {
    let mut clip = Clip::default();
    let Some(formats) = formats.as_object() else {
        return Ok(clip);
    };
    let total: usize = formats
        .values()
        .filter_map(Value::as_str)
        .map(str::len)
        .sum();
    if total > limits.text {
        return Err(format!(
            "the text is larger than {} MB",
            limits.text / (1024 * 1024)
        ));
    }
    let text = |name: &str| {
        formats
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    clip.text = text("text/plain");
    clip.html = text("text/html");
    clip.rtf = text("text/rtf").or_else(|| text("application/rtf"));
    Ok(clip)
}

impl Uploads {
    /// A `paste` message: a new paste or the cancelling of one.
    pub fn message(&mut self, message: &Value, limits: &Limits, scratch: &Scratch) -> Outcome {
        let id = message["id"].as_str().unwrap_or_default().to_string();
        if !valid_id(&id) {
            return Outcome::reply(failed(&id, "a paste needs an id of letters and digits"));
        }
        if message["cancel"] == true {
            if self.current.as_ref().is_some_and(|u| u.id == id) {
                self.current = None;
                return Outcome::reply(json!({"type": "paste", "id": id, "state": "cancelled"}));
            }
            return Outcome {
                replies: Vec::new(),
                ready: None,
            };
        }
        // A new paste replaces one still arriving.
        self.current = None;
        let mut clip = match formats(&message["formats"], limits) {
            Ok(clip) => clip,
            Err(error) => return Outcome::reply(failed(&id, error)),
        };
        let files = message["files"].as_array().cloned().unwrap_or_default();
        if files.len() > limits.count {
            return Outcome::reply(failed(
                &id,
                format!("at most {} files can be pasted at once", limits.count),
            ));
        }
        let sizes: Vec<u64> = files
            .iter()
            .map(|f| f["size"].as_u64().unwrap_or(0))
            .collect();
        let total: u64 = sizes.iter().sum();
        if !files.is_empty() && total > limits.files {
            return Outcome::reply(failed(
                &id,
                if limits.files == 0 {
                    "this server takes no files (--clipboard-max-mb 0)".to_string()
                } else {
                    format!(
                        "the files are larger than {} MB together",
                        limits.files / (1024 * 1024)
                    )
                },
            ));
        }
        if files.is_empty() && clip.is_empty() {
            return Outcome::reply(failed(&id, "nothing to paste"));
        }
        if files.is_empty() {
            clip.files.clear();
            return Outcome {
                replies: vec![json!({"type": "paste", "id": id, "state": "received"})],
                ready: Some(Ready { id, clip }),
            };
        }
        let dir = match scratch.paste_dir(&id) {
            Ok(dir) => dir,
            Err(error) => return Outcome::reply(failed(&id, error)),
        };
        let mut upload = Upload {
            id: id.clone(),
            dir: dir.clone(),
            clip,
            files: Vec::new(),
            received: 0,
            acked: 0,
        };
        let mut taken: Vec<String> = Vec::new();
        for (file, size) in files.iter().zip(sizes) {
            let name = unique(
                safe_file_name(file["name"].as_str().unwrap_or("file")),
                &mut taken,
            );
            let path = dir.join(&name);
            let handle = match File::create(&path) {
                Ok(handle) => handle,
                Err(error) => {
                    return Outcome::reply(failed(&id, format!("cannot store {name}: {error}")))
                }
            };
            upload.files.push(Incoming {
                path,
                size,
                written: 0,
                file: Some(handle),
                mime: file["type"].as_str().unwrap_or_default().to_string(),
            });
        }
        if upload.complete() {
            return Outcome {
                replies: vec![json!({"type": "paste", "id": id, "state": "received"})],
                ready: Some(upload.finish()),
            };
        }
        self.current = Some(upload);
        Outcome {
            replies: Vec::new(),
            ready: None,
        }
    }

    /// A binary frame: a piece of a file.
    pub fn chunk(&mut self, frame: &[u8]) -> Outcome {
        let none = || Outcome {
            replies: Vec::new(),
            ready: None,
        };
        let Some(piece) = chunk(frame) else {
            return none();
        };
        let Some(upload) = self.current.as_mut().filter(|u| u.id == piece.id) else {
            // A piece of a cancelled or replaced paste.
            return none();
        };
        let id = upload.id.clone();
        let Some(file) = upload.files.get_mut(piece.index) else {
            self.current = None;
            return Outcome::reply(failed(&id, "a piece of a file that is not in the paste"));
        };
        if piece.offset != file.written || file.written + piece.data.len() as u64 > file.size {
            self.current = None;
            return Outcome::reply(failed(&id, "the file's pieces came out of order"));
        }
        let written = file
            .file
            .as_mut()
            .map(|handle| handle.write_all(piece.data));
        if let Some(Err(error)) = written {
            self.current = None;
            return Outcome::reply(failed(&id, format!("cannot store the file: {error}")));
        }
        file.written += piece.data.len() as u64;
        upload.received += piece.data.len() as u64;
        let mut replies = Vec::new();
        if upload.received - upload.acked >= ACK_EVERY {
            upload.acked = upload.received;
            replies.push(json!({"type": "paste", "id": id, "ack": upload.received}));
        }
        if upload.complete() {
            let upload = self.current.take().expect("the upload is current");
            replies.push(json!({"type": "paste", "id": id, "state": "received",
                "bytes": upload.total()}));
            return Outcome {
                replies,
                ready: Some(upload.finish()),
            };
        }
        Outcome {
            replies,
            ready: None,
        }
    }
}

/// A name not yet taken in this paste: `a.png`, `a (2).png`, ...
fn unique(name: String, taken: &mut Vec<String>) -> String {
    let lower = |s: &str| s.to_lowercase();
    let mut candidate = name.clone();
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name.as_str(), ""),
    };
    let mut n = 2;
    while taken.iter().any(|t| lower(t) == lower(&candidate)) {
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    taken.push(candidate.clone());
    candidate
}

/// One piece of an uploaded file.
pub struct Piece<'a> {
    pub id: &'a str,
    pub index: usize,
    pub offset: u64,
    pub data: &'a [u8],
}

/// The magic that starts an upload frame.
pub const MAGIC: &[u8; 4] = b"AADP";

/// Reads an upload frame: `AADP`, version u8 (1), 3 reserved bytes, the
/// paste id's length u16 and the id, the file's index u16, the offset u64,
/// then the bytes (at most [`CHUNK`]); big-endian.
pub fn chunk(frame: &[u8]) -> Option<Piece<'_>> {
    if frame.len() < 10 || &frame[..4] != MAGIC || frame[4] != 1 {
        return None;
    }
    let id_len = usize::from(u16::from_be_bytes([frame[8], frame[9]]));
    let id = std::str::from_utf8(frame.get(10..10 + id_len)?).ok()?;
    let at = 10 + id_len;
    let index = usize::from(u16::from_be_bytes(frame.get(at..at + 2)?.try_into().ok()?));
    let offset = u64::from_be_bytes(frame.get(at + 2..at + 10)?.try_into().ok()?);
    let data = frame.get(at + 10..)?;
    (data.len() <= CHUNK).then_some(Piece {
        id,
        index,
        offset,
        data,
    })
}

#[cfg(test)]
pub fn frame(id: &str, index: u16, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut frame = MAGIC.to_vec();
    frame.extend_from_slice(&[1, 0, 0, 0]);
    frame.extend_from_slice(&(id.len() as u16).to_be_bytes());
    frame.extend_from_slice(id.as_bytes());
    frame.extend_from_slice(&index.to_be_bytes());
    frame.extend_from_slice(&offset.to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Scratch {
        let name = format!("aad-assist-test-{}", uuid::Uuid::new_v4().simple());
        Scratch::named(&std::env::temp_dir(), &name).unwrap()
    }

    #[test]
    fn a_scratch_folder_goes_with_its_run_and_a_dead_runs_is_swept() {
        let temp =
            std::env::temp_dir().join(format!("aad-sweep-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&temp).unwrap();
        // A run that crashed: its folder and an unlocked lock file.
        std::fs::create_dir_all(temp.join("aad-assist-999999").join("paste")).unwrap();
        std::fs::write(temp.join("aad-assist-999999.lock"), b"").unwrap();
        // A live one.
        let live = Scratch::named(&temp, "aad-assist-1").unwrap();
        sweep(&temp);
        assert!(!temp.join("aad-assist-999999").exists());
        assert!(temp.join("aad-assist-1").exists());
        drop(live);
        assert!(!temp.join("aad-assist-1").exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn text_alone_is_ready_at_once() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "a1", "formats": {"text/plain": "hi\tthere\n", "text/html": "<b>hi</b>"}}),
            &Limits::new(100),
            &scratch,
        );
        let ready = outcome.ready.unwrap();
        assert_eq!(ready.clip.text.as_deref(), Some("hi\tthere\n"));
        assert_eq!(ready.clip.html.as_deref(), Some("<b>hi</b>"));
        assert!(ready.clip.files.is_empty());
        assert_eq!(outcome.replies[0]["state"], "received");
    }

    #[test]
    fn files_arrive_in_pieces_with_acknowledgements_and_unique_names() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        let big = vec![7u8; (ACK_EVERY as usize) + 10];
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "f1", "files": [
                {"name": "a.bin", "type": "", "size": big.len()},
                {"name": "A.bin", "type": "", "size": 3},
                {"name": "empty", "type": "", "size": 0}]}),
            &Limits::new(100),
            &scratch,
        );
        assert!(outcome.ready.is_none() && outcome.replies.is_empty());
        let mut acks = 0;
        let mut ready = None;
        for (at, piece) in big.chunks(CHUNK).enumerate() {
            let outcome = uploads.chunk(&frame("f1", 0, (at * CHUNK) as u64, piece));
            acks += outcome
                .replies
                .iter()
                .filter(|r| r.get("ack").is_some())
                .count();
            assert!(outcome.ready.is_none());
        }
        let outcome = uploads.chunk(&frame("f1", 1, 0, b"xyz"));
        ready = ready.or(outcome.ready);
        let ready = ready.expect("complete after the last piece");
        assert_eq!(acks, 1);
        let names: Vec<String> = ready
            .clip
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.bin", "A (2).bin", "empty"]);
        assert_eq!(std::fs::read(&ready.clip.files[0]).unwrap(), big);
        assert_eq!(std::fs::read(&ready.clip.files[1]).unwrap(), b"xyz");
        assert!(ready.clip.image.is_none());
    }

    #[test]
    fn a_cancelled_upload_leaves_nothing_behind() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        uploads.message(
            &json!({"type": "paste", "id": "c1", "files": [{"name": "a", "size": 10}]}),
            &Limits::new(100),
            &scratch,
        );
        uploads.chunk(&frame("c1", 0, 0, b"12345"));
        let dir = scratch.root.join("paste").join("c1");
        assert!(dir.exists());
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "c1", "cancel": true}),
            &Limits::new(100),
            &scratch,
        );
        assert_eq!(outcome.replies[0]["state"], "cancelled");
        assert!(!dir.exists());
        // Late pieces are ignored.
        assert!(uploads.chunk(&frame("c1", 0, 5, b"67890")).ready.is_none());
    }

    #[test]
    fn limits_refuse_before_anything_is_stored() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "l1", "files": [{"name": "a", "size": 2 * 1024 * 1024}]}),
            &Limits::new(1),
            &scratch,
        );
        assert_eq!(outcome.replies[0]["state"], "failed");
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "l2", "files": [{"name": "a", "size": 1}]}),
            &Limits::new(0),
            &scratch,
        );
        assert!(outcome.replies[0]["message"]
            .as_str()
            .unwrap()
            .contains("no files"));
        assert!(!scratch.root.join("paste").join("l1").exists());
        let outcome = uploads.message(
            &json!({"type": "paste", "id": "../x", "formats": {"text/plain": "a"}}),
            &Limits::new(1),
            &scratch,
        );
        assert_eq!(outcome.replies[0]["state"], "failed");
    }

    #[test]
    fn out_of_order_pieces_fail_the_paste() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        uploads.message(
            &json!({"type": "paste", "id": "o1", "files": [{"name": "a", "size": 10}]}),
            &Limits::new(100),
            &scratch,
        );
        let outcome = uploads.chunk(&frame("o1", 0, 5, b"12345"));
        assert_eq!(outcome.replies[0]["state"], "failed");
    }

    #[test]
    fn a_single_picture_is_offered_as_a_bitmap_too() {
        let scratch = scratch();
        let mut uploads = Uploads::default();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 2))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        uploads.message(
            &json!({"type": "paste", "id": "p1", "files": [{"name": "shot.png", "type": "image/png", "size": png.len()}]}),
            &Limits::new(100),
            &scratch,
        );
        let ready = uploads.chunk(&frame("p1", 0, 0, &png)).ready.unwrap();
        assert_eq!(ready.clip.image.as_deref(), Some(png.as_slice()));
        assert_eq!(ready.clip.files.len(), 1);
    }

    #[test]
    fn frames_that_are_not_uploads_are_not_read_as_ones() {
        assert!(chunk(b"AADX\x01\0\0\0\0\0").is_none());
        let frame = frame("id", 3, 9, b"data");
        let piece = chunk(&frame).unwrap();
        assert_eq!(
            (piece.id, piece.index, piece.offset, piece.data),
            ("id", 3, 9, &b"data"[..])
        );
    }
}
