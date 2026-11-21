//! Run on a signed-in Mac with the built AX helper granted Accessibility.
#![cfg(target_os = "macos")]

use aad_macos::browser_picker::FilePicker;
use aad_platform::picker::Kind;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture(Child);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(8)
}

fn report(path: &Path) -> Value {
    let until = deadline();
    loop {
        if let Ok(bytes) = fs::read(path) {
            return serde_json::from_slice(&bytes).unwrap();
        }
        assert!(
            Instant::now() < until,
            "missing AppKit report {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires a macOS GUI session and the signed AX helper with Accessibility permission"]
fn inspect_and_operate_real_appkit_file_panels() {
    let root = WorkDir(
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string()),
    );
    let contents = root.0.join("Fixture.app/Contents");
    let bin = contents.join("MacOS");
    fs::create_dir_all(&bin).unwrap();
    let fixture = bin.join("FilePickerFixture");
    fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>FilePickerFixture</string>
<key>CFBundleIdentifier</key><string>dev.ai-auto-desktop.file-picker-fixture</string>
<key>CFBundlePackageType</key><string>APPL</string></dict></plist>"#,
    )
    .unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/FilePickerFixture.swift");
    assert!(Command::new("xcrun")
        .args(["--sdk", "macosx", "swiftc"])
        .arg(source)
        .arg("-o")
        .arg(&fixture)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(root.0.join("Fixture.app"))
        .status()
        .unwrap()
        .success());

    for mode in ["open", "save", "replace"] {
        let dir = root.0.join(mode);
        fs::create_dir(&dir).unwrap();
        let app = Fixture(Command::new(&fixture).arg(mode).arg(&dir).spawn().unwrap());
        let pid = app.0.id();
        let bounds: [i32; 4] = serde_json::from_value(report(&dir.join("ready"))).unwrap();
        let kind = if mode == "save" {
            Kind::Save
        } else {
            Kind::Open
        };
        let until = deadline();
        let picker = loop {
            match FilePicker::observe(pid, None, bounds, kind, until) {
                Ok(picker) => break picker,
                Err(e) if Instant::now() >= until => panic!("observe {mode}: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(80)),
            }
        };
        assert!(!picker.process_only());
        assert_eq!(picker.id(), 0);
        picker.revalidate(deadline()).unwrap();
        let wrong = [bounds[0] + 10, bounds[1], bounds[2], bounds[3]];
        assert!(FilePicker::observe(pid, None, wrong, kind, deadline()).is_err());
        match mode {
            "save" => {
                assert!(picker.dismiss(deadline()).is_err());
                let occupied = dir.join("kept.txt");
                fs::write(&occupied, b"keep").unwrap();
                assert!(picker.save_new(&occupied, deadline()).is_err());
                let dest = dir.join("selected name.txt");
                picker.save_new(&dest, deadline()).unwrap();
                assert_eq!(
                    report(&dir.join("result"))["selected"],
                    dest.to_str().unwrap()
                );
                assert_eq!(
                    fs::read_to_string(dest).unwrap(),
                    "native fixture wrote this file\n"
                );
                assert_eq!(fs::read(occupied).unwrap(), b"keep");
            }
            "replace" => {
                fs::write(dir.join("replace-now"), b"").unwrap();
                let until = deadline();
                while picker.revalidate(deadline()).is_ok() {
                    assert!(Instant::now() < until, "previous panel was still valid");
                }
                assert!(picker.dismiss(deadline()).is_err());
                assert!(!dir.join("prompt-result").exists());
            }
            _ => {
                picker.dismiss(deadline()).unwrap();
                assert_eq!(report(&dir.join("result"))["cancelled"], true);
            }
        }
        assert!(picker.revalidate(deadline()).is_err());
    }
}
