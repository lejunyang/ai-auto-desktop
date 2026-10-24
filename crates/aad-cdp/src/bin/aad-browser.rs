//! Standalone startup for legacy watch users, with explicit profile selection.
use aad_cdp::launch::{
    browser_exe, copy_selected_profile, ensure_available, list_profiles, real_user_data,
    select_profile,
};
use clap::Parser;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(about = "Launch a Chromium browser with CDP; occupied profiles require --kill-browser")]
struct Args {
    #[arg(long, default_value = "edge", value_parser = ["edge", "chrome", "brave"])]
    browser: String,
    #[arg(long, default_value_t = 9222)]
    port: u16,
    /// Persistent working user-data directory (must be new when copying).
    #[arg(long)]
    profile_dir: Option<PathBuf>,
    /// Copy the selected profile's login files. Omit ID only when exactly one exists.
    #[arg(long, num_args = 0..=1, value_name = "ID")]
    existing_profile: Option<Option<String>>,
    /// Custom source user-data directory.
    #[arg(long)]
    profile_source: Option<PathBuf>,
    /// List available profile IDs and names without starting a browser.
    #[arg(long)]
    profiles: bool,
    /// Explicitly close the browser occupying the selected user-data directory.
    #[arg(long)]
    kill_browser: bool,
    #[arg(long)]
    detached: bool,
    #[arg(long)]
    keep_copy: bool,
    url: Option<String>,
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source = if args.existing_profile.is_some() || args.profiles {
        Some(
            args.profile_source
                .clone()
                .or_else(|| real_user_data(&args.browser))
                .ok_or("browser profile source not found; use --profile-source")?,
        )
    } else {
        if args.profile_source.is_some() {
            return Err("--profile-source requires --existing-profile or --profiles".into());
        }
        None
    };
    if args.profiles {
        let source = source.unwrap();
        println!(
            "{}",
            json!({"browser":args.browser,"profile_source":source,"profiles":list_profiles(&source)?})
        );
        return Ok(());
    }
    if args.detached && source.is_some() && !args.keep_copy && args.profile_dir.is_none() {
        return Err("detached login copies require --keep-copy or an explicit NEW --profile-dir; use foreground mode for automatic cleanup".into());
    }
    if args.port == 0 {
        return Err("aad-browser requires an explicit nonzero port; browser-record automatically allocates a port".into());
    }
    if aad_cdp::Client::available(args.port) {
        return Err(
            "CDP port is already occupied; choose another port (no browser was closed)".into(),
        );
    }
    let exe = browser_exe(Some(&args.browser)).ok_or("selected browser executable not found")?;
    let work = args.profile_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("aad-cdp-profile-{}", uuid::Uuid::new_v4()))
    });
    if let Some(source) = &source {
        let id = select_profile(
            source,
            args.existing_profile.as_ref().and_then(|id| id.as_deref()),
        )?;
        aad_cdp::launch::validate_copy_destination(source, &work)?;
        ensure_available(source, &args.browser, args.kill_browser)?;
        copy_selected_profile(source, &work, &id)?;
        eprintln!("Using existing profile {id} via a working login copy.");
    } else {
        std::fs::create_dir_all(&work)?;
    }
    ensure_available(&work, &args.browser, args.kill_browser)?;
    let mut command = Command::new(exe);
    command
        .args([
            format!("--remote-debugging-port={}", args.port),
            "--remote-debugging-address=127.0.0.1".into(),
            format!("--user-data-dir={}", work.canonicalize()?.display()),
            "--profile-directory=Default".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--new-window".into(),
            args.url.unwrap_or_else(|| "about:blank".into()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    if args.detached {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0200 | 0x0000_0008);
    }
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            break;
        }
        if aad_cdp::Client::available(args.port) {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!(
        "{}",
        json!({"browser":args.browser,"port":args.port,"profile_dir":work,"ready":ready,"pid":child.id()})
    );
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err("debug endpoint did not start; browser may already use this profile; retry with --kill-browser or choose another profile".into());
    }
    if args.detached {
        drop(child);
    } else {
        let _ = child.wait();
        if args.profile_dir.is_none() && !args.keep_copy {
            let _ = std::fs::remove_dir_all(work);
        }
    }
    Ok(())
}
fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
