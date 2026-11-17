//! The general pasteboard in every format, for `aad assist`, through
//! `osascript` (JavaScript for Automation with the AppKit bridge), so no
//! AppKit bindings are needed here.
//!
//! A paste keeps every item and type the pasteboard holds, writes all of a
//! [`Clip`]'s formats at once (public.utf8-plain-text, public.html,
//! public.rtf, public.png, file URLs), marked concealed and transient so
//! clipboard managers that honour nspasteboard.org skip it, has Cmd+V
//! pressed, and writes back what it kept. Unlike Windows and X11 the macOS
//! pasteboard cannot hand data to one process only: the concealed marker is
//! what keeps it out of history.
//!
//! [待验证] Written without a Mac at hand; needs a real macOS (13+) run of
//! paste into TextEdit/Pages (HTML, RTF, picture) and Finder (files), and
//! of copy from them.
use aad_platform::clip::{Clip, Copied, Pasting, Press};
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;

/// Keeps every item of the pasteboard (each type's data in a file of
/// `dir`), or puts back what was kept. Arguments: `save|restore`, `dir`.
const KEEP: &str = r#"ObjC.import('AppKit');
function run(argv) {
  const pb = $.NSPasteboard.generalPasteboard, dir = argv[1], fm = $.NSFileManager.defaultManager;
  if (argv[0] === 'save') {
    const items = pb.pasteboardItems, manifest = [];
    for (let i = 0; i < (items.js || []).length; i++) {
      const item = items.objectAtIndex(i), types = item.types.js, kept = [];
      for (let j = 0; j < types.length; j++) {
        const type = ObjC.unwrap(types[j]), data = item.dataForType(type);
        if (!data || data.isNil()) continue;
        const file = dir + '/' + i + '-' + j;
        data.writeToFileAtomically(file, true);
        kept.push({type, file});
      }
      manifest.push(kept);
    }
    $.NSString.alloc.initWithUTF8String(JSON.stringify(manifest)).writeToFileAtomicallyEncodingError(dir + '/manifest.json', true, 4, null);
    return 'saved';
  }
  const manifest = JSON.parse(ObjC.unwrap($.NSString.stringWithContentsOfFileEncodingError(dir + '/manifest.json', 4, null)));
  pb.clearContents;
  const objects = [];
  for (const kept of manifest) {
    const item = $.NSPasteboardItem.alloc.init;
    for (const k of kept) item.setDataForType($.NSData.dataWithContentsOfFile(k.file), k.type);
    objects.push(item);
  }
  if (objects.length) pb.writeObjects($(objects));
  return 'restored';
}"#;

/// Writes a paste: arguments are a JSON file {text, html, rtf, png (a file),
/// files: [paths]}.
const WRITE: &str = r#"ObjC.import('AppKit');
function run(argv) {
  const pb = $.NSPasteboard.generalPasteboard;
  const p = JSON.parse(ObjC.unwrap($.NSString.stringWithContentsOfFileEncodingError(argv[0], 4, null)));
  const item = $.NSPasteboardItem.alloc.init;
  if (p.text != null) item.setStringForType(p.text, 'public.utf8-plain-text');
  if (p.html != null) item.setStringForType(p.html, 'public.html');
  if (p.rtf != null) item.setDataForType($(p.rtf).dataUsingEncoding(4), 'public.rtf');
  if (p.png != null) item.setDataForType($.NSData.dataWithContentsOfFile(p.png), 'public.png');
  // nspasteboard.org: managers keep neither concealed nor transient content.
  item.setStringForType('', 'org.nspasteboard.ConcealedType');
  item.setStringForType('', 'org.nspasteboard.TransientType');
  const objects = [item];
  for (const path of p.files) objects.push($.NSURL.fileURLWithPath(path));
  pb.clearContents;
  return pb.writeObjects($(objects)) ? 'written' : 'refused';
}"#;

/// Reads the pasteboard into a JSON file: {protected, text, html, png (a
/// file), files}. Arguments: the JSON file, the picture's file.
const READ: &str = r#"ObjC.import('AppKit');
function run(argv) {
  const pb = $.NSPasteboard.generalPasteboard, out = {files: []};
  const types = (pb.types.js || []).map(t => ObjC.unwrap(t));
  if (types.includes('org.nspasteboard.ConcealedType')) out.protected = true;
  else {
    const text = pb.stringForType('public.utf8-plain-text');
    if (text && !text.isNil()) out.text = ObjC.unwrap(text);
    const html = pb.stringForType('public.html');
    if (html && !html.isNil()) out.html = ObjC.unwrap(html);
    let png = pb.dataForType('public.png');
    if (!png || png.isNil()) {
      const tiff = pb.dataForType('public.tiff');
      if (tiff && !tiff.isNil()) png = $.NSBitmapImageRep.imageRepWithData(tiff).representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
    }
    if (png && !png.isNil()) { png.writeToFileAtomically(argv[1], true); out.png = argv[1]; }
    const urls = pb.readObjectsForClassesOptions($([$.NSURL]), $({NSPasteboardURLReadingFileURLsOnlyKey: true}));
    if (urls && !urls.isNil()) for (const url of urls.js) out.files.push(ObjC.unwrap(url.path));
  }
  $.NSString.alloc.initWithUTF8String(JSON.stringify(out)).writeToFileAtomicallyEncodingError(argv[0], true, 4, null);
  return 'read';
}"#;

fn osascript(script: &str, args: &[&Path]) -> Result<String> {
    let output = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .args(args)
        .output()
        .map_err(|e| format!("cannot run osascript: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "osascript: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn scratch() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "aad-pasteboard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("pasteboard scratch: {e}"))?;
    Ok(dir)
}

/// Pastes every format of `clip` into the frontmost application and puts
/// the pasteboard back afterwards.
pub fn paste(clip: Clip, mut press: Press) -> Pasting {
    std::thread::spawn(move || {
        let dir = scratch()?;
        let result = (|| {
            osascript(KEEP, &[Path::new("save"), &dir])?;
            let png = match &clip.image {
                Some(bytes) => {
                    let path = dir.join("paste.png");
                    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                    Some(path)
                }
                None => None,
            };
            let spec = serde_json::json!({"text": clip.text, "html": clip.html, "rtf": clip.rtf,
                "png": png, "files": clip.files});
            let spec_path = dir.join("paste.json");
            std::fs::write(&spec_path, spec.to_string()).map_err(|e| e.to_string())?;
            if osascript(WRITE, &[&spec_path])? != "written" {
                return Err("the pasteboard refused the paste".to_string());
            }
            let pressed = press();
            // The pasteboard cannot tell when the target has read it: give
            // it time, longer for files a Finder copy reads as it starts.
            std::thread::sleep(Duration::from_millis(if clip.has_files() {
                1_500
            } else {
                600
            }));
            let restored = osascript(KEEP, &[Path::new("restore"), &dir]).is_ok();
            pressed?;
            let names: Vec<&str> = [
                ("text", clip.text.is_some()),
                ("html", clip.html.is_some()),
                ("rtf", clip.rtf.is_some()),
                ("image", clip.image.is_some()),
                ("files", clip.has_files()),
            ]
            .into_iter()
            .filter_map(|(n, on)| on.then_some(n))
            .collect();
            Ok(serde_json::json!({"pasted": true, "formats": names, "restored": restored}))
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    })
}

/// What the pasteboard holds: text, HTML, a picture (PNG, or TIFF turned
/// into PNG) and file URLs; nothing when it is marked concealed.
pub fn copied() -> Result<Copied> {
    let dir = scratch()?;
    let out = dir.join("read.json");
    let png = dir.join("read.png");
    let result = osascript(READ, &[&out, &png]).and_then(|_| {
        let read: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out).map_err(|e| format!("pasteboard: {e}"))?)
                .map_err(|e| format!("pasteboard: {e}"))?;
        Ok(Copied {
            protected: read["protected"] == true,
            text: read["text"].as_str().map(str::to_string),
            html: read["html"].as_str().map(str::to_string),
            image: read["png"].as_str().and_then(|p| std::fs::read(p).ok()),
            files: read["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|f| f.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}
