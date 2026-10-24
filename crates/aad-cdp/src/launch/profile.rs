use std::path::{Path, PathBuf};

pub fn browser_exe(browser: Option<&str>) -> Option<PathBuf> {
    let names: &[&str] = match browser {
        Some("edge") => &["edge"],
        Some("chrome") => &["chrome"],
        Some("brave") => &["brave"],
        None => &["edge", "chrome", "brave"],
        _ => return None,
    };
    names.iter().find_map(|name| find_browser(name))
}

fn find_browser(name: &str) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let app = match name {
            "edge" => "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "chrome" => "Google Chrome.app/Contents/MacOS/Google Chrome",
            _ => "Brave Browser.app/Contents/MacOS/Brave Browser",
        };
        return Some(PathBuf::from("/Applications").join(app)).filter(|p| p.is_file());
    }
    if !cfg!(windows) {
        let commands: &[&str] = match name {
            "edge" => &["microsoft-edge", "microsoft-edge-stable"],
            "chrome" => &[
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ],
            _ => &["brave-browser", "brave-browser-stable"],
        };
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            for command in commands {
                let path = dir.join(command);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        return None;
    }
    let program_files = [
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()),
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
        std::env::var("LOCALAPPDATA").unwrap_or_default(),
    ];
    let relative = match name {
        "edge" => r"Microsoft\Edge\Application\msedge.exe",
        "chrome" => r"Google\Chrome\Application\chrome.exe",
        _ => r"BraveSoftware\Brave-Browser\Application\brave.exe",
    };
    for base in program_files {
        if !base.is_empty() {
            let path = PathBuf::from(&base).join(relative);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

pub fn real_user_data(browser: &str) -> Option<PathBuf> {
    if !cfg!(windows) {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let path = if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
                .join(match browser {
                    "edge" => "Microsoft Edge",
                    "chrome" => "Google/Chrome",
                    "brave" => "BraveSoftware/Brave-Browser",
                    _ => return None,
                })
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join(match browser {
                    "edge" => "microsoft-edge",
                    "chrome" => "google-chrome",
                    "brave" => "BraveSoftware/Brave-Browser",
                    _ => return None,
                })
        };
        return path.is_dir().then_some(path);
    }
    let local = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?);
    let path = match browser.to_ascii_lowercase().as_str() {
        "edge" | "msedge" => local.join("Microsoft").join("Edge").join("User Data"),
        "chrome" | "googlechrome" => local.join("Google").join("Chrome").join("User Data"),
        "brave" => local
            .join("BraveSoftware")
            .join("Brave-Browser")
            .join("User Data"),
        _ => return None,
    };
    path.exists().then_some(path)
}

/// Session/login files only — a few MB, not the multi-GB cache.
const AUTH_FILES: &[&str] = &[
    "Local State",
    "Default/Preferences",
    "Default/Secure Preferences",
    "Default/Network/Cookies",
    "Default/Network/Cookies-journal",
    "Default/Network/Cookies-wal",
    "Default/Cookies",
    "Default/Cookies-journal",
    "Default/Cookies-wal",
    "Default/Login Data",
    "Default/Login Data-journal",
    "Default/Login Data-wal",
    "Default/Web Data",
    "Default/Web Data-journal",
    "Default/Web Data-wal",
];

pub fn copy_login_profile(real: &Path, work: &Path) -> Result<(), String> {
    let real = real.canonicalize().map_err(|e| e.to_string())?;
    let parent = work
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if parent.starts_with(&real) {
        return Err("the working profile must be outside the real profile".into());
    }
    // Atomically claim a NEW directory. Never recursively delete a path supplied
    // by the caller: it may contain unrelated data or be the real profile itself.
    std::fs::create_dir(work)
        .map_err(|e| format!("working profile must be a new directory: {e}"))?;
    let result = copy_login_files(&real, work);
    if result.is_err() {
        // This invocation created the directory, so partial copies are ours.
        let _ = std::fs::remove_dir_all(work);
    }
    result
}

fn copy_login_files(real: &Path, work: &Path) -> Result<(), String> {
    let mut copied = 0usize;
    for relative in AUTH_FILES {
        let source = real.join(relative);
        if !source.exists() {
            continue;
        }
        let destination = work.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // The cookie file can stay locked briefly after window close.
        let mut last_error = None;
        for _ in 0..10 {
            match std::fs::copy(&source, &destination) {
                Ok(_) => {
                    last_error = None;
                    copied += 1;
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
        }
        if let Some(e) = last_error {
            return Err(format!(
                "could not copy {relative}: {e}; close the source browser before copying"
            ));
        }
    }
    if copied == 0 {
        return Err("no login files were copied (is the browser closed?)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copying_a_profile_never_deletes_existing_or_source_directories() {
        let root = std::env::temp_dir().join(format!("aad-profile-test-{}", uuid::Uuid::new_v4()));
        let real = root.join("real");
        let existing = root.join("existing");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir(&existing).unwrap();
        std::fs::write(real.join("Local State"), b"dummy fixture").unwrap();
        std::fs::write(existing.join("important.txt"), b"keep me").unwrap();

        for work in [&real, &existing, &root, &real.join("nested")] {
            assert!(copy_login_profile(&real, work).is_err());
            assert_eq!(
                std::fs::read(real.join("Local State")).unwrap(),
                b"dummy fixture"
            );
            assert_eq!(
                std::fs::read(existing.join("important.txt")).unwrap(),
                b"keep me"
            );
        }

        let copy = root.join("new-copy");
        copy_login_profile(&real, &copy).unwrap();
        assert_eq!(
            std::fs::read(copy.join("Local State")).unwrap(),
            b"dummy fixture"
        );
        let empty = root.join("empty-source");
        std::fs::create_dir(&empty).unwrap();
        let failed = root.join("failed-copy");
        assert!(copy_login_profile(&empty, &failed).is_err());
        assert!(
            !failed.exists(),
            "a failed copy must clean its own directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
