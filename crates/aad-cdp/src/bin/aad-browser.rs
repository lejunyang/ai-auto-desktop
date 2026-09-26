//! Launch a Chromium browser with a CDP debug port on a DEDICATED profile.
//!
//! This is the supported, safe way to get a debuggable browser for
//! `aad watch --cdp-port`: it never touches the user's everyday browser
//! profile (Chromium refuses the debug port on the default data dir, and
//! attaching to a running instance is impossible). The dedicated profile keeps
//! the user's normal tabs/logins untouched while giving a clean, observable
//! browser window.
//!
//! Usage:
//!   cargo run -p aad-cdp --bin aad-browser -- --port 9222 [url]
//! or after install, the `aad-browser` executable.

use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Args {
    port: u16,
    profile_dir: Option<PathBuf>,
    url: Option<String>,
    detached: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: 9222,
        profile_dir: None,
        url: None,
        detached: false,
    };
    let mut rest = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => args.port = iter.next().and_then(|value| value.parse().ok()).unwrap_or(9222),
            "--profile-dir" => args.profile_dir = iter.next().map(PathBuf::from),
            "--detached" => args.detached = true,
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
        "aad-browser: launch Edge/Chrome with a CDP debug port on a dedicated profile\n\n\
USAGE:\n  aad-browser [--port 9222] [--profile-dir PATH] [--detached] [URL]\n\n\
  --port          CDP remote-debugging port (default 9222)\n\
  --profile-dir   dedicated user-data-dir (default: a temp aad-cdp-profile-<port>)\n\
  --detached      return immediately after launch (default: keep the launcher \
alive until the browser closes)\n\
  URL             first page to open (default about:blank)\n\n\
Your everyday browser/profile is never modified."
    );
}

/// Candidate executables, newest Chromium browsers first.
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

fn main() {
    let args = parse_args();
    let exe = browser_exe().unwrap_or_else(|| {
        eprintln!("no Edge/Chrome/Brave executable found");
        std::process::exit(2);
    });

    let profile_dir = args.profile_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("aad-cdp-profile-{}", args.port))
    });
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");

    let url = args.url.unwrap_or_else(|| "about:blank".to_string());

    // Hardened, quiet, debuggable launch. No extensions/sync/first-run noise;
    // the dedicated data dir is what makes the debug port actually open.
    let mut command = Command::new(&exe);
    command
        .args([
            format!("--remote-debugging-port={}", args.port),
            format!("--user-data-dir={}", profile_dir.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-extensions".to_string(),
            "--disable-sync".to_string(),
            "--new-window".to_string(),
            url,
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null());

    // On Windows detach so closing this launcher does not kill the browser;
    // the process tree still cleans up with the profile.
    #[cfg(windows)]
    if args.detached {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    let child = command.spawn().unwrap_or_else(|error| {
        eprintln!("failed to start {}: {error}", exe.display());
        std::process::exit(1);
    });

    // Wait briefly for the debug endpoint, so callers know it is ready.
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut ready = false;
    while std::time::Instant::now() < ready_deadline {
        if aad_cdp::Client::available(args.port) {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    println!(
        "{{\"browser\":\"{}\",\"port\":{},\"profile_dir\":\"{}\",\"ready\":{},\"pid\":{}}}",
        exe.display(),
        args.port,
        profile_dir.display(),
        ready,
        child.id()
    );

    if !ready {
        eprintln!("warning: debug endpoint did not answer in time");
        std::process::exit(1);
    }
    if !args.detached {
        // Keep the launcher alive; when the browser window closes this exits.
        let _ = child.wait_with_output();
    }
}
