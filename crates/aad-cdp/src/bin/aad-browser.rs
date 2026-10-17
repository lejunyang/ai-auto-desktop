//! Launch a Chromium browser with a CDP debug port.
//!
//! Chromium refuses `--remote-debugging-port` on the browser's DEFAULT
//! user-data-dir (a security measure) and cannot attach to a running instance.
//! Modes:
//! - dedicated (default): isolated temp profile, clean but logged out;
//! - `--copy-login <browser>`: close that browser, copy ONLY the
//!   credential-bearing files (Local State, Cookies, Login Data, Web Data,
//!   Preferences; a few MB) from the real profile into a non-default working
//!   dir, then launch there with the port. Same Windows user => the DPAPI
//!   cookie key still decrypts, so site logins carry over without pointing the
//!   port at or modifying the real multi-GB profile. A temp login copy is
//!   removed after the session unless `--keep-copy` is set.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Args {
    port: u16,
    profile_dir: Option<PathBuf>,
    url: Option<String>,
    detached: bool,
    copy_login: Option<String>,
    keep_copy: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: 9222,
        profile_dir: None,
        url: None,
        detached: false,
        copy_login: None,
        keep_copy: false,
    };
    let mut rest = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => args.port = iter.next().and_then(|v| v.parse().ok()).unwrap_or(9222),
            "--profile-dir" => args.profile_dir = iter.next().map(PathBuf::from),
            "--detached" => args.detached = true,
            "--copy-login" => args.copy_login = iter.next(),
            "--keep-copy" => args.keep_copy = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => rest.push(other.to_string()),
        }
    }
    args.url = rest.into_iter().next();
    args
}

fn print_help() {
    println!(
        "aad-browser: launch Edge/Chrome with a CDP debug port\n\n\
USAGE:\n  aad-browser [--port 9222] [URL]\n  aad-browser --copy-login edge [--keep-copy] [--port 9222] [URL]\n\n\
  --port         CDP remote-debugging port (default 9222)\n\
  --profile-dir  explicit working user-data-dir (default: a temp dir)\n\
  --copy-login BROWSER  carry logins in by copying the real profile's\n                 credential files (edge|chrome|brave); closes the browser;\n\
                 the real profile is never modified\n\
  --keep-copy    keep the copied working profile after exit (otherwise the\n                 temp copy, which contains credentials, is deleted)\n\
  --detached     return once the endpoint is ready\n\
  URL            first page to open (default about:blank)"
    );
}

fn browser_exe() -> Option<PathBuf> {
    let program_files = [
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()),
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
    ];
    let candidates = [
        r"Microsoft\Edge\Application\msedge.exe",
        r"Google\Chrome\Application\chrome.exe",
        r"BraveSoftware\Brave-Browser\Application\brave.exe",
    ];
    for base in program_files {
        for relative in candidates {
            let path = PathBuf::from(&base).join(relative);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

fn real_user_data(browser: &str) -> Option<PathBuf> {
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
    "Default/Cookies",
    "Default/Cookies-journal",
    "Default/Login Data",
    "Default/Login Data-journal",
    "Default/Web Data",
    "Default/Web Data-journal",
];

fn copy_login_profile(real: &Path, work: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
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
            eprintln!("warning: could not copy {relative}: {e}");
        }
    }
    if copied == 0 {
        return Err("no login files were copied (is the browser closed?)".into());
    }
    Ok(())
}

#[cfg(windows)]
fn close_browser(exe: &Path) {
    let image = exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("msedge.exe");
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", image, "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for _ in 0..30 {
        let still = Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {image}")])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(image))
            .unwrap_or(false);
        if !still {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

#[cfg(not(windows))]
fn close_browser(exe: &Path) {
    if let Some(image) = exe.file_name().and_then(|n| n.to_str()) {
        let _ = Command::new("pkill").arg("-x").arg(image).status();
    }
}

fn main() {
    let args = parse_args();
    let exe = browser_exe().unwrap_or_else(|| {
        eprintln!("no Edge/Chrome/Brave executable found");
        std::process::exit(2);
    });

    let mut temp_copy: Option<PathBuf> = None;
    let profile_dir = match (&args.copy_login, &args.profile_dir) {
        (Some(browser), explicit) => {
            let real = real_user_data(browser).unwrap_or_else(|| {
                eprintln!("unknown browser or no installed profile for '{browser}'");
                std::process::exit(2);
            });
            println!(
                "{{\"stage\":\"closing_browser\",\"browser\":\"{}\"}}",
                browser
            );
            close_browser(&exe);
            let work = explicit.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("aad-cdp-login-{}", args.port))
            });
            if let Err(error) = copy_login_profile(&real, &work) {
                eprintln!("login copy failed: {error}");
                std::process::exit(1);
            }
            if args.profile_dir.is_none() {
                temp_copy = Some(work.clone());
            }
            work
        }
        (None, Some(explicit)) => explicit.clone(),
        (None, None) => std::env::temp_dir().join(format!("aad-cdp-profile-{}", args.port)),
    };
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");

    let url = args.url.unwrap_or_else(|| "about:blank".to_string());

    let mut command = Command::new(&exe);
    command
        .args([
            format!("--remote-debugging-port={}", args.port),
            format!("--user-data-dir={}", profile_dir.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--new-window".to_string(),
            url,
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null());

    #[cfg(windows)]
    if args.detached {
        use std::os::windows::process::CommandExt;
        const FLAGS: u32 = 0x0000_0200 | 0x0000_0008;
        command.creation_flags(FLAGS);
    }

    let child = command.spawn().unwrap_or_else(|error| {
        eprintln!("failed to start {}: {error}", exe.display());
        std::process::exit(1);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if aad_cdp::Client::available(args.port) {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    let mode = if args.copy_login.is_some() {
        "login-copy"
    } else {
        "dedicated"
    };
    println!(
        "{{\"browser\":\"{}\",\"port\":{},\"profile_dir\":\"{}\",\"mode\":\"{}\",\"ready\":{},\"pid\":{}}}",
        exe.display(),
        args.port,
        profile_dir.display(),
        mode,
        ready,
        child.id()
    );

    if !ready {
        eprintln!("warning: debug endpoint did not answer in time");
        std::process::exit(1);
    }
    if !args.detached {
        let _ = child.wait_with_output();
    }

    // Do not leave a credential-bearing temp copy on disk.
    if let Some(copy) = temp_copy {
        if !args.keep_copy {
            close_browser(&exe);
            let _ = std::fs::remove_dir_all(copy);
        }
    }
}
