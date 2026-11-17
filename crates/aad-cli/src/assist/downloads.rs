//! What this side's clipboard holds, offered to a viewer copying from it.
//!
//! A `clipboard` message lists it: text and HTML in the message itself, a
//! picture and files by id. The viewer fetches those with
//! `GET /clipboard/<id>?token=..`, on the same port and token as the page:
//! a picture as PNG, a file as an attachment, a folder (or everything at
//! once, `all`) as a zip streamed while it is read. Ids are 128-bit random
//! and only the latest listing's are good: a new listing forgets the last.
use super::transfer::Limits;
use aad_platform::clip::Copied;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One thing a listing offers for download.
enum Item {
    Picture(Arc<Vec<u8>>),
    File(PathBuf),
    /// A folder, or several files and folders, as one zip.
    Zip(String, Vec<PathBuf>),
}

/// The latest listing's downloads, shared with the threads that serve them.
#[derive(Clone, Default)]
pub struct Listing(Arc<Mutex<HashMap<String, Item>>>);

/// Most entries walked to size up a folder: a listing must come quickly.
const MOST_ENTRIES: usize = 20_000;

/// The size of a file, or of everything under a folder; `None` when there
/// is too much to walk.
fn size_of(path: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        seen += 1;
        if seen > MOST_ENTRIES {
            return None;
        }
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if meta.is_dir() {
            pending.extend(std::fs::read_dir(&path).ok()?.flatten().map(|e| e.path()));
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    Some(total)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Text cut to at most `most` bytes, on a character boundary.
fn cut(mut text: String, most: usize) -> (String, bool) {
    if text.len() <= most {
        return (text, false);
    }
    let mut at = most;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text.truncate(at);
    (text, true)
}

impl Listing {
    /// Forgets the last listing and makes the `clipboard` message for this
    /// one. `text`, `characters` and `truncated` mean what they always did.
    pub fn list(&self, copied: Copied, limits: &Limits) -> Value {
        let mut items = HashMap::new();
        if copied.protected {
            *self.0.lock().expect("listing") = items;
            return json!({"type": "clipboard", "protected": true, "text": null, "characters": 0,
                "truncated": false});
        }
        let characters = copied.text.as_ref().map_or(0, |t| t.chars().count());
        let (text, text_cut) = match copied.text {
            Some(text) => {
                let (text, was_cut) = cut(text, limits.text);
                (Some(text), was_cut)
            }
            None => (None, false),
        };
        // HTML too large is left out rather than cut into broken markup.
        let html = copied.html.filter(|html| html.len() <= limits.text);
        let mut message = json!({"type": "clipboard", "text": text, "characters": characters,
            "truncated": text_cut, "html": html});
        if let Some(png) = copied.image {
            let (width, height) = image::io::Reader::new(std::io::Cursor::new(&png))
                .with_guessed_format()
                .ok()
                .and_then(|r| r.into_dimensions().ok())
                .unwrap_or((0, 0));
            let id = new_id();
            message["image"] =
                json!({"id": id, "width": width, "height": height, "size": png.len()});
            items.insert(id, Item::Picture(Arc::new(png)));
        }
        if !copied.files.is_empty() {
            let mut files = Vec::new();
            let mut total = Some(0u64);
            let mut offered = Vec::new();
            for path in &copied.files {
                let dir = path.is_dir();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                let size = size_of(path);
                total = total.zip(size).map(|(a, b)| a + b);
                let too_large = limits.files == 0 || size.is_none_or(|size| size > limits.files);
                let mut entry = json!({"name": name, "size": size, "dir": dir});
                if too_large {
                    entry["tooLarge"] = json!(true);
                } else {
                    let id = new_id();
                    entry["id"] = json!(id);
                    items.insert(
                        id,
                        if dir {
                            Item::Zip(format!("{name}.zip"), vec![path.clone()])
                        } else {
                            Item::File(path.clone())
                        },
                    );
                    offered.push(path.clone());
                }
                files.push(entry);
            }
            message["files"] = json!(files);
            let several = copied.files.len() > 1 || copied.files.iter().any(|p| p.is_dir());
            if several && total.is_some_and(|total| total <= limits.files) && offered.len() > 1 {
                let id = new_id();
                message["all"] = json!(id);
                items.insert(id, Item::Zip("clipboard.zip".into(), offered));
            }
        }
        *self.0.lock().expect("listing") = items;
        message
    }

    /// Answers `GET /clipboard/<id>` on `stream`.
    pub fn serve(&self, id: &str, stream: &mut impl Write) {
        let item = {
            let items = self.0.lock().expect("listing");
            match items.get(id) {
                Some(Item::Picture(png)) => Some(Item::Picture(png.clone())),
                Some(Item::File(path)) => Some(Item::File(path.clone())),
                Some(Item::Zip(name, paths)) => Some(Item::Zip(name.clone(), paths.clone())),
                None => None,
            }
        };
        let result = match item {
            None => stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Some(Item::Picture(png)) => {
                let head = head("image/png", "clipboard.png", Some(png.len() as u64));
                stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(&png))
            }
            Some(Item::File(path)) => match std::fs::File::open(&path) {
                Ok(mut file) => {
                    let size = file.metadata().map(|m| m.len()).ok();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into());
                    let head = head(mime(&name), &name, size);
                    stream
                        .write_all(head.as_bytes())
                        .and_then(|()| std::io::copy(&mut file, stream).map(|_| ()))
                }
                Err(_) => stream.write_all(
                    b"HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            },
            Some(Item::Zip(name, paths)) => {
                let head = head("application/zip", &name, None);
                stream
                    .write_all(head.as_bytes())
                    .and_then(|()| zip(&paths, stream))
            }
        };
        if let Err(error) = result {
            eprintln!("aad assist: a clipboard download stopped: {error}");
        }
        let _ = stream.flush();
    }
}

/// The response head for a download: its type, its name for saving (ASCII
/// for old clients, UTF-8 for the rest) and its length when known; without
/// one the body ends when the connection closes.
fn head(mime: &str, name: &str, length: Option<u64>) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let length = length.map_or(String::new(), |n| format!("Content-Length: {n}\r\n"));
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Disposition: attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}\r\n{length}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
    )
}

/// A file's type by its extension, for the few a browser treats specially.
fn mime(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Writes a zip of `paths` (files and whole folders) as it reads them:
/// entries stored as they are, each followed by its CRC and sizes (so
/// nothing is read twice), names in UTF-8, folders kept as folders.
/// Larger than 4 GB needs Zip64, which this does not write; listings refuse
/// that much anyway.
pub fn zip(paths: &[PathBuf], out: &mut impl Write) -> std::io::Result<()> {
    let mut central = Vec::new();
    let mut offset = 0u64;
    let mut count = 0u16;
    let mut pending: Vec<(PathBuf, String)> = paths
        .iter()
        .rev()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            (p.clone(), name)
        })
        .collect();
    while let Some((path, name)) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path)?;
        let (time, date) = dos_time(meta.modified().ok());
        let dir = meta.is_dir();
        let entry_name = if dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        if dir {
            let mut children: Vec<_> = std::fs::read_dir(&path)?.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for child in children.into_iter().rev() {
                let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
                pending.push((child.path(), child_name));
            }
        } else if !meta.is_file() {
            continue;
        }
        let name_bytes = entry_name.as_bytes();
        // Local header: sizes and CRC follow the data (flag bit 3); UTF-8 (bit 11).
        let flags: u16 = 0x0808;
        let mut local = Vec::with_capacity(30 + name_bytes.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&flags.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&time.to_le_bytes());
        local.extend_from_slice(&date.to_le_bytes());
        local.extend_from_slice(&[0u8; 12]); // crc, sizes: in the descriptor
        local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_bytes);
        out.write_all(&local)?;
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0u64;
        if !dir {
            let mut file = std::fs::File::open(&path)?;
            let mut buffer = vec![0u8; 256 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                crc.update(&buffer[..n]);
                out.write_all(&buffer[..n])?;
                size += n as u64;
            }
        }
        let crc = crc.finalize();
        let size32 = u32::try_from(size)
            .map_err(|_| std::io::Error::other("a file over 4 GB cannot be zipped here"))?;
        let mut descriptor = Vec::with_capacity(16);
        descriptor.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        descriptor.extend_from_slice(&crc.to_le_bytes());
        descriptor.extend_from_slice(&size32.to_le_bytes());
        descriptor.extend_from_slice(&size32.to_le_bytes());
        out.write_all(&descriptor)?;
        let offset32 = u32::try_from(offset)
            .map_err(|_| std::io::Error::other("more than 4 GB cannot be zipped here"))?;
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&20u16.to_le_bytes()); // needed
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size32.to_le_bytes());
        central.extend_from_slice(&size32.to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]); // extra, comment, disk, internal attributes
        central.extend_from_slice(&(if dir { 0x10u32 } else { 0 }).to_le_bytes());
        central.extend_from_slice(&offset32.to_le_bytes());
        central.extend_from_slice(name_bytes);
        offset += (local.len() + descriptor.len()) as u64 + size;
        count = count
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("too many entries to zip"))?;
    }
    let central_at = u32::try_from(offset)
        .map_err(|_| std::io::Error::other("more than 4 GB cannot be zipped here"))?;
    out.write_all(&central)?;
    let mut end = Vec::with_capacity(22);
    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    end.extend_from_slice(&[0u8; 4]); // disks
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&(central.len() as u32).to_le_bytes());
    end.extend_from_slice(&central_at.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    out.write_all(&end)
}

/// A time as MS-DOS keeps it in a zip: (time, date), UTC, from 1980.
fn dos_time(time: Option<std::time::SystemTime>) -> (u16, u16) {
    let Some(seconds) = time
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
    else {
        return (0, 0x21);
    };
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if year < 1980 {
        return (0, 0x21);
    }
    let date = (((year - 1980).min(127) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = ((rest / 3600) as u16) << 11
        | (((rest % 3600) / 60) as u16) << 5
        | ((rest % 60) / 2) as u16;
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aad-dl-{}", new_id()));
        std::fs::create_dir_all(dir.join("sub").join("deeper")).unwrap();
        std::fs::write(dir.join("报告.txt"), "内容").unwrap();
        std::fs::write(dir.join("sub").join("a.bin"), vec![1u8; 70_000]).unwrap();
        std::fs::write(dir.join("sub").join("deeper").join("b.txt"), "b").unwrap();
        dir
    }

    /// Reads a zip back with nothing but its central directory and headers.
    fn unzip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = bytes.len() - 22;
        assert_eq!(&bytes[end..end + 4], &0x0605_4b50u32.to_le_bytes());
        let count = u16::from_le_bytes([bytes[end + 10], bytes[end + 11]]) as usize;
        let mut at = u32::from_le_bytes(bytes[end + 16..end + 20].try_into().unwrap()) as usize;
        let mut entries = Vec::new();
        for _ in 0..count {
            assert_eq!(&bytes[at..at + 4], &0x0201_4b50u32.to_le_bytes());
            let crc = u32::from_le_bytes(bytes[at + 16..at + 20].try_into().unwrap());
            let size = u32::from_le_bytes(bytes[at + 20..at + 24].try_into().unwrap()) as usize;
            let name_len = u16::from_le_bytes([bytes[at + 28], bytes[at + 29]]) as usize;
            let local = u32::from_le_bytes(bytes[at + 42..at + 46].try_into().unwrap()) as usize;
            let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let local_name = u16::from_le_bytes([bytes[local + 26], bytes[local + 27]]) as usize;
            let data_at = local + 30 + local_name;
            let data = bytes[data_at..data_at + size].to_vec();
            assert_eq!(crc32fast::hash(&data), crc, "{name}");
            entries.push((name, data));
            at += 46 + name_len;
        }
        entries
    }

    #[test]
    fn a_folder_zips_whole_and_reads_back() {
        let dir = folder();
        let mut out = Vec::new();
        zip(&[dir.join("报告.txt"), dir.join("sub")], &mut out).unwrap();
        let entries = unzip(&out);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "报告.txt",
                "sub/",
                "sub/a.bin",
                "sub/deeper/",
                "sub/deeper/b.txt"
            ]
        );
        assert_eq!(entries[0].1, "内容".as_bytes());
        assert_eq!(entries[2].1, vec![1u8; 70_000]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_listing_offers_what_fits_and_only_its_own_ids() {
        let dir = folder();
        let listing = Listing::default();
        let copied = Copied {
            text: Some("x".repeat(10)),
            html: Some("<b>x</b>".into()),
            files: vec![dir.join("报告.txt"), dir.join("sub")],
            ..Copied::default()
        };
        let limits = Limits {
            text: 4,
            files: 1024 * 1024,
            count: 100,
        };
        let message = listing.list(copied.clone(), &limits);
        assert_eq!(message["text"], "xxxx");
        assert_eq!(message["truncated"], true);
        assert_eq!(message["characters"], 10);
        // HTML larger than the limit is left out, not cut.
        assert!(message["html"].is_null());
        assert_eq!(message["files"][1]["dir"], true);
        assert_eq!(message["files"][1]["size"], 70_001);
        let file = message["files"][0]["id"].as_str().unwrap().to_string();
        let all = message["all"].as_str().unwrap().to_string();
        let mut response = Vec::new();
        listing.serve(&file, &mut response);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(
            text.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8A.txt"),
            "{text}"
        );
        assert!(text.ends_with("内容"), "{text}");
        let mut response = Vec::new();
        listing.serve(&all, &mut response);
        let body = &response[response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
        assert_eq!(unzip(body).len(), 5);
        // Small limits mark what does not fit, and a new listing forgets the old.
        let message = listing.list(
            copied,
            &Limits {
                files: 10,
                ..limits
            },
        );
        assert_eq!(message["files"][1]["tooLarge"], true);
        assert!(message.get("all").is_none());
        let mut response = Vec::new();
        listing.serve(&file, &mut response);
        assert!(response.starts_with(b"HTTP/1.1 404"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn protected_content_is_not_listed() {
        let message = Listing::default().list(
            Copied {
                protected: true,
                text: Some("secret".into()),
                ..Copied::default()
            },
            &Limits::new(100),
        );
        assert_eq!(message["protected"], true);
        assert!(message["text"].is_null());
    }

    #[test]
    fn dos_times_are_civil_dates() {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        // 2023-11-14 22:13:20 UTC
        let (t, d) = dos_time(Some(time));
        assert_eq!(d >> 9, 2023 - 1980);
        assert_eq!((d >> 5) & 0xf, 11);
        assert_eq!(d & 0x1f, 14);
        assert_eq!(t >> 11, 22);
        assert_eq!((t >> 5) & 0x3f, 13);
    }
}
