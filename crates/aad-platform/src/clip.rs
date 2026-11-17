//! Clipboard content in every format at once, as `aad assist` passes it
//! between a viewer's machine and this one.
//!
//! A paste ([`Clip`]) puts all its formats on this system's clipboard together
//! and has the paste key pressed, so the program pasted into picks the format
//! it understands -- a word processor the HTML, a chat program the picture, a
//! file manager the files. What was on the clipboard before is put back
//! afterwards. A copy ([`Copied`]) reads the same formats back.
use serde_json::Value;
use std::path::PathBuf;

/// What one paste offers. Formats left out are not offered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Clip {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<String>,
    /// A picture as PNG bytes, offered in the system's bitmap formats too.
    pub image: Option<Vec<u8>>,
    /// Files on this machine, offered as a file list to copy.
    pub files: Vec<PathBuf>,
}

impl Clip {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.html.is_none()
            && self.rtf.is_none()
            && self.image.is_none()
            && self.files.is_empty()
    }

    /// Only files: the program pasted into may read them some time after
    /// the paste key, so their offer is kept longer.
    pub fn has_files(&self) -> bool {
        !self.files.is_empty()
    }
}

/// What the clipboard holds, read without changing it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Copied {
    /// Its source (a password manager) asked readers to leave it alone;
    /// nothing else is read.
    pub protected: bool,
    pub text: Option<String>,
    pub html: Option<String>,
    /// A picture as PNG bytes.
    pub image: Option<Vec<u8>>,
    /// Files and folders copied in a file manager.
    pub files: Vec<PathBuf>,
}

/// Presses the paste key in the program being pasted into. Called by a
/// paste once everything is on the clipboard, from the paste's own thread.
pub type Press = Box<dyn FnMut() -> Result<(), String> + Send>;

/// A paste under way on a thread of its own: puts the clipboard back when
/// done and tells what it offered and whether the target took it.
pub type Pasting = std::thread::JoinHandle<Result<Value, String>>;

/// The fragment of an HTML clipboard document: what lies between the
/// `<!--StartFragment-->` and `<!--EndFragment-->` markers when they are
/// there, else what lies in `<body>`, else all of it.
pub fn html_fragment(html: &str) -> &str {
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    if let (Some(start), Some(end)) = (html.find(START), html.rfind(END)) {
        if start + START.len() <= end {
            return &html[start + START.len()..end];
        }
    }
    let lower = html.to_ascii_lowercase();
    if let Some(open) = lower.find("<body") {
        if let Some(close) = lower[open..].find('>') {
            let from = open + close + 1;
            let to = lower
                .rfind("</body>")
                .filter(|to| *to >= from)
                .unwrap_or(html.len());
            return &html[from..to];
        }
    }
    html
}

/// A file name safe to create on any system: no path separators, no
/// characters Windows refuses, not a reserved device name, not empty.
pub fn safe_file_name(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Trailing dots and spaces vanish on Windows; leading dots hide the file
    // or name a parent.
    clean = clean.trim_end_matches(['.', ' ']).to_string();
    clean = clean.trim_start_matches('.').to_string();
    if clean.is_empty() {
        clean = "file".into();
    }
    let stem = clean
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        clean.insert(0, '_');
    }
    // Most systems allow 255 bytes per name.
    if clean.len() > 200 {
        let ext = clean
            .rfind('.')
            .map(|i| clean[i..].to_string())
            .filter(|e| e.len() < 16)
            .unwrap_or_default();
        let mut cut = 200 - ext.len();
        while !clean.is_char_boundary(cut) {
            cut -= 1;
        }
        clean.truncate(cut);
        clean.push_str(&ext);
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fragment_is_found_by_its_markers_or_the_body() {
        assert_eq!(
            html_fragment(
                "<html><body><!--StartFragment--><b>x</b><!--EndFragment--></body></html>"
            ),
            "<b>x</b>"
        );
        assert_eq!(
            html_fragment("<meta charset=utf-8><BODY class=a><i>y</i></BODY>"),
            "<i>y</i>"
        );
        assert_eq!(html_fragment("<p>z</p>"), "<p>z</p>");
    }

    #[test]
    fn file_names_cannot_escape_or_name_devices() {
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("a\\b:c.txt"), "a_b_c.txt");
        assert_eq!(safe_file_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_file_name("com1"), "_com1");
        assert_eq!(safe_file_name("..."), "file");
        assert_eq!(safe_file_name("report. "), "report");
        assert_eq!(safe_file_name("报告.pdf"), "报告.pdf");
        let long = format!("{}.png", "长".repeat(300));
        let safe = safe_file_name(&long);
        assert!(safe.len() <= 200 && safe.ends_with(".png"), "{safe}");
    }
}
