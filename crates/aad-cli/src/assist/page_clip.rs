//! The clipboard of a shared browser page, over CDP.
//!
//! A paste is a real one where it can be: the page is allowed the clipboard,
//! everything is written to it with `navigator.clipboard.write` (text, HTML
//! and a picture), and the paste command is sent with Ctrl+V, so the page's
//! editor handles it as a person's paste. Measured in headless Chrome: the
//! page's paste event then holds text/plain, text/html and the picture as a
//! file, and a contenteditable inserts the HTML. What the clipboard held
//! before is written back afterwards (in headless Chrome it is the browser's
//! own; in a browser on a desktop it is that desktop's).
//!
//! Files other than one picture cannot go through that clipboard: they are
//! handed to the page in a synthetic `paste` event carrying them as `File`s,
//! and when nothing handles it in an editable element the text or HTML is
//! inserted instead. A focused `<input type=file>` simply gets the files.
use aad_platform::clip::{Clip, Copied};
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, String>;

/// Most bytes of files one paste carries into a page: they go as base64
/// through the debugging protocol.
pub const MAX_PAGE_FILES: u64 = 20 * 1024 * 1024;

/// What has the focus, through same-origin frames: `file`, `editable` or
/// `other`.
const FOCUS: &str = "(() => { let d = document, a = d.activeElement;
  try { while (a && a.contentDocument) { d = a.contentDocument; a = d.activeElement; } } catch {}
  if (a && a.tagName === 'INPUT' && a.type === 'file') return 'file';
  if (a && (a.isContentEditable || /^(INPUT|TEXTAREA)$/.test(a.tagName))) return 'editable';
  return 'other'; })()";

/// The focused element itself, as an object for DOM.setFileInputFiles.
const FOCUSED: &str = "(() => { let d = document, a = d.activeElement;
  try { while (a && a.contentDocument) { d = a.contentDocument; a = d.activeElement; } } catch {}
  return a; })()";

/// Keeps what the clipboard holds, then writes the paste to it. Its
/// argument: {text, html, png (base64)}.
const WRITE: &str = "async (p) => {
  const blob = (b64, type) => new Blob([Uint8Array.from(atob(b64), c => c.charCodeAt(0))], {type});
  try {
    const kept = [];
    for (const item of await navigator.clipboard.read()) {
      const parts = {};
      for (const type of item.types) if (/^(text\\/plain|text\\/html|image\\/png)$/.test(type)) parts[type] = await item.getType(type);
      if (Object.keys(parts).length) kept.push(parts);
    }
    window.__aadKept = kept;
  } catch { window.__aadKept = null; }
  const parts = {};
  if (p.text != null) parts['text/plain'] = new Blob([p.text], {type: 'text/plain'});
  if (p.html != null) parts['text/html'] = new Blob([p.html], {type: 'text/html'});
  if (p.png != null) parts['image/png'] = blob(p.png, 'image/png');
  await navigator.clipboard.write([new ClipboardItem(parts)]);
  return true; }";

/// Puts back what [`WRITE`] kept.
const RESTORE: &str = "async () => { const kept = window.__aadKept; delete window.__aadKept;
  if (!kept || !kept.length) return false;
  await navigator.clipboard.write(kept.map(parts => new ClipboardItem(parts))); return true; }";

/// A paste event carrying the paste, files as `File`s, sent to the focused
/// element; when nothing handled it there and it is editable, the HTML or
/// text is inserted. Its argument: {text, html, rtf, files: [{name, type, b64}]}.
const SYNTHETIC: &str = "(p) => { let d = document, a = d.activeElement;
  try { while (a && a.contentDocument) { d = a.contentDocument; a = d.activeElement; } } catch {}
  const target = a || d.body;
  const data = new DataTransfer();
  if (p.text != null) data.setData('text/plain', p.text);
  if (p.html != null) data.setData('text/html', p.html);
  if (p.rtf != null) data.setData('text/rtf', p.rtf);
  for (const f of p.files) data.items.add(new File([Uint8Array.from(atob(f.b64), c => c.charCodeAt(0))], f.name, {type: f.type}));
  const event = new ClipboardEvent('paste', {clipboardData: data, bubbles: true, cancelable: true, composed: true});
  const handled = !target.dispatchEvent(event);
  let inserted = false;
  if (!handled && target && (target.isContentEditable || /^(INPUT|TEXTAREA)$/.test(target.tagName))) {
    if (p.html != null && target.isContentEditable) inserted = d.execCommand('insertHTML', false, p.html);
    else if (p.text != null) inserted = d.execCommand('insertText', false, p.text);
  }
  return {handled, inserted}; }";

/// Reads the clipboard after the page copied: {text, html, png (base64)}.
const READ: &str = "async () => {
  const out = {};
  for (const item of await navigator.clipboard.read()) {
    for (const type of item.types) {
      const blob = await item.getType(type);
      if (type === 'text/plain' && out.text == null) out.text = await blob.text();
      else if (type === 'text/html' && out.html == null) out.html = await blob.text();
      else if (type === 'image/png' && out.png == null) {
        const bytes = new Uint8Array(await blob.arrayBuffer());
        let s = ''; for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
        out.png = btoa(s);
      }
    }
  }
  return out; }";

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The value an evaluation returned, or what it threw.
fn value(answer: Value) -> Result<Value> {
    if let Some(details) = answer.get("exceptionDetails") {
        let text = details["exception"]["description"]
            .as_str()
            .or(details["text"].as_str())
            .unwrap_or("the page threw");
        return Err(text.lines().next().unwrap_or(text).to_string());
    }
    Ok(answer["result"]["value"].clone())
}

/// A function of the page called with one JSON argument, awaited.
fn call_function(
    call: &mut dyn FnMut(&str, Value) -> Result<Value>,
    function: &str,
    argument: &Value,
) -> Result<Value> {
    value(call(
        "Runtime.evaluate",
        json!({"expression": format!("({function})({argument})"), "awaitPromise": true,
            "returnByValue": true, "userGesture": true}),
    )?)
}

/// The MIME type of a pasted file, by its extension.
fn file_type(name: &str) -> &'static str {
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
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Presses V with `modifiers` (CDP bits: the platform's paste or copy
/// shortcut) and runs `command` as the browser would for that shortcut.
fn shortcut(
    call: &mut dyn FnMut(&str, Value) -> Result<Value>,
    modifiers: u32,
    key: &str,
    command: &str,
) -> Result<()> {
    let code = format!("Key{}", key.to_ascii_uppercase());
    let vk = u32::from(key.to_ascii_uppercase().as_bytes()[0]);
    call(
        "Input.dispatchKeyEvent",
        json!({"type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk,
            "modifiers": modifiers, "commands": [command]}),
    )?;
    call(
        "Input.dispatchKeyEvent",
        json!({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk,
            "modifiers": modifiers}),
    )?;
    Ok(())
}

/// The CDP modifier bit of the paste and copy shortcuts.
const SHORTCUT: u32 = if cfg!(target_os = "macos") { 4 } else { 2 };

/// Pastes `clip` into the page. `held` are the CDP modifiers the viewer
/// holds; the shortcut's own is added.
pub fn paste(
    call: &mut dyn FnMut(&str, Value) -> Result<Value>,
    held: u32,
    clip: &Clip,
) -> Result<Value> {
    let total: u64 = clip
        .files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if total > MAX_PAGE_FILES {
        return Err(format!(
            "a browser page takes at most {} MB of files per paste",
            MAX_PAGE_FILES / (1024 * 1024)
        ));
    }
    let focus = value(call(
        "Runtime.evaluate",
        json!({"expression": FOCUS, "returnByValue": true}),
    )?)?;
    if focus == "file" && !clip.files.is_empty() {
        let element = call(
            "Runtime.evaluate",
            json!({"expression": FOCUSED, "returnByValue": false}),
        )?;
        let object = element["result"]["objectId"]
            .as_str()
            .ok_or("the file field could not be reached")?;
        let paths: Vec<String> = clip
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        call(
            "DOM.setFileInputFiles",
            json!({"files": paths, "objectId": object}),
        )?;
        return Ok(json!({"pasted": true, "formats": ["files"], "via": "file field"}));
    }
    let mut formats = Vec::new();
    for (name, present) in [
        ("text", clip.text.is_some()),
        ("html", clip.html.is_some()),
        ("rtf", clip.rtf.is_some()),
        ("image", clip.image.is_some()),
        ("files", !clip.files.is_empty()),
    ] {
        if present {
            formats.push(name);
        }
    }
    // One picture (also listed as its file) goes as the clipboard's picture.
    let only_picture = clip.files.is_empty() || (clip.files.len() == 1 && clip.image.is_some());
    if only_picture {
        let _ = call(
            "Browser.grantPermissions",
            json!({"permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"]}),
        );
        let _ = call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        );
        let written = call_function(
            call,
            WRITE,
            &json!({"text": clip.text, "html": clip.html, "png": clip.image.as_deref().map(b64)}),
        );
        if written.is_ok() {
            let pressed = shortcut(call, held | SHORTCUT, "v", "paste");
            let _ = call_function(call, RESTORE, &json!(null));
            pressed?;
            return Ok(json!({"pasted": true, "formats": formats, "via": "clipboard"}));
        }
    }
    let mut files = Vec::new();
    for path in &clip.files {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read the pasted file: {e}"))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        files.push(json!({"type": file_type(&name), "name": name, "b64": b64(&bytes)}));
    }
    let result = call_function(
        call,
        SYNTHETIC,
        &json!({"text": clip.text, "html": clip.html, "rtf": clip.rtf, "files": files}),
    )?;
    let taken = result["handled"] == true || result["inserted"] == true;
    Ok(json!({"pasted": taken, "formats": formats, "via": "paste event"}))
}

/// What the page's clipboard holds after the page copies what is selected
/// (the copy command, as Ctrl+C would). `None` when the clipboard cannot be
/// read here.
pub fn copied(call: &mut dyn FnMut(&str, Value) -> Result<Value>, held: u32) -> Option<Copied> {
    let _ = call(
        "Browser.grantPermissions",
        json!({"permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"]}),
    );
    let _ = call(
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    );
    shortcut(call, held | SHORTCUT, "c", "copy").ok()?;
    let read = call_function(call, READ, &json!(null)).ok()?;
    let png = read["png"].as_str().and_then(|s| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(s).ok()
    });
    Some(Copied {
        protected: false,
        text: read["text"].as_str().map(str::to_string),
        html: read["html"].as_str().map(str::to_string),
        image: png,
        files: Vec::new(),
    })
}
