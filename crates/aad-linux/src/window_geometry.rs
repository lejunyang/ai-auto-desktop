//! Read-only X11 client geometry used to calibrate accessibility coordinates.
use std::os::unix::fs::MetadataExt;
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState, WindowClass};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientGeometry {
    pub id: u32,
    pub title: String,
    /// Exact ICCCM WM_NAME, which KDE can keep without the application suffix.
    pub legacy_title: Option<String>,
    /// Complete COMPOUND_TEXT bytes; decoded only by the bounded Xlib helper.
    pub compound_title: Option<Vec<u8>>,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ClientGeometry {
    pub fn title_matches(&self, name: &str) -> bool {
        !name.is_empty() && (self.title == name || self.legacy_title.as_deref() == Some(name))
    }
}

fn legacy_title(
    kind: u32,
    utf8: u32,
    format: u8,
    remaining: u32,
    value: Vec<u8>,
) -> Option<String> {
    if format != 8 || remaining != 0 || value.contains(&0) {
        return None;
    }
    if kind == utf8 {
        String::from_utf8(value).ok()
    } else if kind == u32::from(AtomEnum::STRING) {
        // ICCCM STRING is ISO-8859-1. Never guess a locale or decode lossy text.
        Some(value.into_iter().map(char::from).collect())
    } else {
        None // COMPOUND_TEXT requires a separate, verified decoder.
    }
}

/// Only visible clients owned by the specified same-user process are returned.
/// X11 round trips may block; callers requiring cancellation must run this read
/// in a bounded worker and discard late results.
pub fn clients(process_id: u32, deadline: Instant) -> Result<Vec<ClientGeometry>, String> {
    let check = || {
        if Instant::now() < deadline {
            Ok(())
        } else {
            Err("window geometry deadline exceeded".to_string())
        }
    };
    check()?;
    let path = format!("/proc/{process_id}");
    let identity = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if process_id == 0 || identity.uid() != unsafe { libc::geteuid() } {
        return Err("window geometry requires a same-user process".into());
    }
    let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| {
        conn.intern_atom(true, name)
            .map_err(|e| e.to_string())?
            .reply()
            .map(|r| r.atom)
            .map_err(|e| e.to_string())
    };
    let list_atom = atom(b"_NET_CLIENT_LIST")?;
    let pid_atom = atom(b"_NET_WM_PID")?;
    let title_atom = atom(b"_NET_WM_NAME")?;
    let utf8 = atom(b"UTF8_STRING")?;
    let compound = atom(b"COMPOUND_TEXT")?;
    if [list_atom, pid_atom, title_atom, utf8].contains(&0) {
        return Err("window metadata atoms are unavailable".into());
    }
    let reply = conn
        .get_property(false, root, list_atom, AtomEnum::WINDOW, 0, 4096)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    if reply.bytes_after != 0 || reply.format != 32 {
        return Err("window list is incomplete".into());
    }
    let ids = reply
        .value32()
        .ok_or("window list is invalid")?
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    for id in ids {
        check()?;
        let pid = conn
            .get_property(false, id, pid_atom, AtomEnum::CARDINAL, 0, 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if pid.bytes_after != 0
            || pid.value32().and_then(|mut values| values.next()) != Some(process_id)
        {
            continue;
        }
        let attrs = conn
            .get_window_attributes(id)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if attrs.map_state != MapState::VIEWABLE
            || attrs.class != WindowClass::INPUT_OUTPUT
            || attrs.override_redirect
        {
            continue;
        }
        let geometry = conn
            .get_geometry(id)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let origin = conn
            .translate_coordinates(id, root, 0, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if !origin.same_screen || geometry.width == 0 || geometry.height == 0 {
            continue;
        }
        let title = conn
            .get_property(false, id, title_atom, utf8, 0, 1024)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if title.bytes_after != 0 || title.format != 8 {
            continue;
        }
        let title = String::from_utf8(title.value).map_err(|_| "window title is not UTF-8")?;
        let legacy = conn
            .get_property(false, id, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let compound_title = (compound != 0
            && legacy.type_ == compound
            && legacy.format == 8
            && legacy.bytes_after == 0
            && !legacy.value.contains(&0))
        .then(|| legacy.value.clone());
        let legacy_title = legacy_title(
            legacy.type_,
            utf8,
            legacy.format,
            legacy.bytes_after,
            legacy.value,
        );
        result.push(ClientGeometry {
            id,
            title,
            legacy_title,
            compound_title,
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: i32::from(geometry.width),
            height: i32::from(geometry.height),
        });
    }
    check()?;
    let current = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if (current.dev(), current.ino(), current.uid())
        != (identity.dev(), identity.ino(), identity.uid())
    {
        return Err("window process identity changed".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_names_require_complete_known_encoding_and_exact_match() {
        let utf8 = 12345;
        assert_eq!(
            legacy_title(AtomEnum::STRING.into(), utf8, 8, 0, vec![0xe9]),
            Some("é".into())
        );
        assert_eq!(
            legacy_title(utf8, utf8, 8, 0, "中文".as_bytes().to_vec()),
            Some("中文".into())
        );
        for (kind, format, remaining, value) in [
            (utf8, 8, 1, b"title".to_vec()),
            (utf8, 8, 0, vec![0xff]),
            (utf8 + 1, 8, 0, b"title".to_vec()),
            (utf8, 32, 0, b"title".to_vec()),
            (utf8, 8, 0, b"title\0".to_vec()),
        ] {
            assert!(legacy_title(kind, utf8, format, remaining, value).is_none());
        }
        let client = ClientGeometry {
            id: 1,
            title: "folder — Dolphin".into(),
            legacy_title: Some("folder".into()),
            compound_title: None,
            x: 0,
            y: 0,
            width: 400,
            height: 300,
        };
        assert!(client.title_matches("folder"));
        assert!(client.title_matches("folder — Dolphin"));
        assert!(!client.title_matches("fold"));
        assert!(!client.title_matches(""));
    }
}
