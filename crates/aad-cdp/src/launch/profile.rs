use std::path::{Path, PathBuf};

#[derive(serde::Serialize)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// Directory IDs are the stable selection keys. Display names may be duplicated.
pub fn list_profiles(root: &Path) -> Result<Vec<ProfileInfo>, String> {
    let state: serde_json::Value = match std::fs::read(root.join("Local State")) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid Local State: {e}"))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => return Err(e.to_string()),
    };
    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_profile_id(&id).is_err() || !entry.path().join("Preferences").is_file() {
            continue;
        }
        let path = selected_profile(root, &id)?;
        let name = state["profile"]["info_cache"][&id]["name"]
            .as_str()
            .unwrap_or(&id)
            .to_string();
        profiles.push(ProfileInfo { id, name, path });
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(profiles)
}

pub fn validate_profile_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains(['/', '\\', ':'])
        || id.chars().any(char::is_control)
    {
        return Err(
            "profile must be a directory ID such as Default or 'Profile 1', not a path".into(),
        );
    }
    Ok(())
}

pub fn select_profile(root: &Path, requested: Option<&str>) -> Result<String, String> {
    if let Some(id) = requested {
        selected_profile(root, id)?;
        return Ok(id.into());
    }
    let profiles = list_profiles(root)?;
    if profiles.len() == 1 {
        return Ok(profiles[0].id.clone());
    }
    if profiles.is_empty() {
        return Err("no existing browser profiles found in the selected source".into());
    }
    let choices = profiles
        .iter()
        .map(|p| format!("{} ({})", p.id, p.name))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "multiple profiles found: {choices}; specify --existing-profile <ID>"
    ))
}

pub fn selected_profile(root: &Path, id: &str) -> Result<PathBuf, String> {
    validate_profile_id(id)?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let selected = root.join(id).canonicalize().map_err(|_| {
        format!("profile '{id}' not found; run browser-record profiles to list directory IDs")
    })?;
    if selected.parent() != Some(root.as_path()) || !selected.join("Preferences").is_file() {
        return Err(format!(
            "'{id}' is not a browser profile inside {}",
            root.display()
        ));
    }
    Ok(selected)
}

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
    copy_selected_profile(real, work, "Default")
}

pub fn copy_selected_profile(real: &Path, work: &Path, id: &str) -> Result<(), String> {
    selected_profile(real, id)?;
    validate_copy_destination(real, work)?;
    let real = real.canonicalize().map_err(|e| e.to_string())?;
    // Atomically claim a NEW directory. Never remove caller-owned directories.
    std::fs::create_dir(work)
        .map_err(|e| format!("working profile must be a new directory: {e}"))?;
    let result = copy_login_files(&real, work, id);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(work);
    }
    result
}

pub fn validate_copy_destination(real: &Path, work: &Path) -> Result<(), String> {
    if work.exists() {
        return Err("copy destination --profile-dir must be a NEW directory".into());
    }
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
    Ok(())
}

fn copy_login_files(real: &Path, work: &Path, id: &str) -> Result<(), String> {
    let mut copied = 0usize;
    for relative in AUTH_FILES {
        // Keep working copies under Default so later --profile-dir reuse needs
        // no source selection and never opens an empty sibling profile.
        let destination = work.join(relative);
        let relative = relative
            .strip_prefix("Default/")
            .map(|tail| PathBuf::from(id).join(tail))
            .unwrap_or_else(|| PathBuf::from(relative));
        let source = real.join(&relative);
        if !source.exists() {
            continue;
        }
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
                "could not copy {}: {e}; close the source browser before copying",
                relative.display()
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
    fn profile_selection_is_explicit_when_ambiguous_and_copies_only_selected_files() {
        let root = std::env::temp_dir().join(format!("aad-profiles-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        std::fs::create_dir_all(source.join("Profile 1")).unwrap();
        std::fs::write(
            source.join("Profile 1/Preferences"),
            br#"{"marker":"selected"}"#,
        )
        .unwrap();
        std::fs::write(source.join("Local State"), br#"{"profile":{"info_cache":{"Default":{"name":"Work"},"Profile 1":{"name":"Work"},"Profile 2":{"name":"Stale"}}}}"#).unwrap();
        assert_eq!(select_profile(&source, None).unwrap(), "Profile 1");
        std::fs::create_dir(source.join("Default")).unwrap();
        std::fs::write(source.join("Default/Preferences"), b"wrong profile").unwrap();
        let listed = list_profiles(&source).unwrap();
        assert_eq!(
            listed.len(),
            2,
            "stale metadata must not count as a usable profile"
        );
        assert_eq!(
            listed[0].name, listed[1].name,
            "display names are not selection IDs"
        );
        assert!(select_profile(&source, None)
            .unwrap_err()
            .contains("multiple profiles"));
        for invalid in [
            "..",
            "../Default",
            "Profile 9",
            "C:\\Default",
            "Default/sub",
            "Work",
        ] {
            assert!(select_profile(&source, Some(invalid)).is_err());
        }
        let copied = root.join("copy");
        copy_selected_profile(&source, &copied, "Profile 1").unwrap();
        assert_eq!(
            std::fs::read(copied.join("Default/Preferences")).unwrap(),
            br#"{"marker":"selected"}"#
        );
        assert!(
            !copied.join("Profile 1").exists(),
            "working copies consistently use Default"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copying_a_profile_never_deletes_existing_or_source_directories() {
        let root = std::env::temp_dir().join(format!("aad-profile-test-{}", uuid::Uuid::new_v4()));
        let real = root.join("real");
        let existing = root.join("existing");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir(&existing).unwrap();
        std::fs::write(real.join("Local State"), b"dummy fixture").unwrap();
        std::fs::create_dir(real.join("Default")).unwrap();
        std::fs::write(real.join("Default/Preferences"), b"{}").unwrap();
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
