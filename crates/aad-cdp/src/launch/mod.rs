//! Explicit browser startup shared with aad-browser. Recording owns only the
//! process/profile it launches; attaching to an existing page stays separate.
mod profile;
pub use profile::{browser_exe, copy_login_profile, real_user_data};

use crate::{connection::Connection, ws::WebSocket, Client};
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = crate::recording::Result<T>;

pub struct Options {
    pub browser: String,
    pub executable: Option<PathBuf>,
    /// Reusable user-data directory; omitted means a fresh temporary profile.
    pub profile_dir: Option<PathBuf>,
    /// Copy login files from this CLOSED user-data directory into a new profile.
    pub copy_from: Option<PathBuf>,
    pub headless: bool,
}

struct Profile {
    path: PathBuf,
    temporary: bool,
    lock: Option<File>,
}

impl Profile {
    fn prepare(options: &Options) -> Result<Self> {
        let path = options.profile_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("aad-record-profile-{}", uuid::Uuid::new_v4()))
        });
        if let Some(source) = &options.copy_from {
            ensure_closed(source)?;
            copy_login_profile(source, &path)?;
        } else {
            fs::create_dir_all(&path)?;
        }
        let path = path.canonicalize()?;
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(".aad-recording.lock"))
            .map_err(|e| {
                format!("profile already in use by AAD (or stale .aad-recording.lock): {e}")
            })?;
        let profile = Self {
            path,
            temporary: options.profile_dir.is_none(),
            lock: Some(lock),
        };
        ensure_closed(&profile.path)?;
        // A new launch must write a fresh endpoint file. Never attach to a stale
        // port or assume that a pre-existing endpoint belongs to this process.
        let active = profile.path.join("DevToolsActivePort");
        if active.exists() {
            fs::remove_file(active)?;
        }
        Ok(profile)
    }
}

fn ensure_closed(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if fs::symlink_metadata(path.join("SingletonLock")).is_ok() {
        return Err("profile is in use (SingletonLock exists); close that browser before reusing/copying it".into());
    }
    if let Ok(contents) = fs::read_to_string(path.join("DevToolsActivePort")) {
        if let Some(port) = contents.lines().next().and_then(|s| s.parse().ok()) {
            if Client::available(port) {
                return Err("profile has a running CDP browser; attach with --page, or close it before launching".into());
            }
        }
    }
    Ok(())
}

impl Drop for Profile {
    fn drop(&mut self) {
        drop(self.lock.take());
        let _ = fs::remove_file(self.path.join(".aad-recording.lock"));
        if self.temporary {
            for _ in 0..20 {
                if fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!(
                "could not remove temporary browser profile: {}",
                self.path.display()
            );
        }
    }
}

/// Owns a newly started browser. Dropping closes this browser only. Explicit
/// profile directories persist; temporary profiles are removed after shutdown.
pub struct Browser {
    client: Option<Client>,
    child: Child,
    profile: Profile,
    pub port: u16,
    pub page: String,
}

impl Browser {
    pub fn start(options: Options) -> Result<Self> {
        let exe = options
            .executable
            .clone()
            .or_else(|| browser_exe(Some(&options.browser)))
            .ok_or_else(|| {
                format!(
                    "{} executable not found; use --browser-executable",
                    options.browser
                )
            })?;
        let profile = Profile::prepare(&options)?;
        let mut command = Command::new(exe);
        command
            .args([
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--no-startup-window",
            ])
            .arg(format!("--user-data-dir={}", profile.path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if options.headless {
            command.arg("--headless=new");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn()?;
        let mut browser = Self {
            client: None,
            child,
            profile,
            port: 0,
            page: String::new(),
        };
        browser.wait_ready()?;
        let result = browser.client.as_mut().unwrap().command(
            "Target.createTarget",
            json!({"url":"about:blank", "newWindow":true}),
        )?;
        browser.page = result["targetId"]
            .as_str()
            .ok_or("missing newly created page ID")?
            .into();
        Ok(browser)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("browser exited before CDP startup ({status}); close any browser using this profile, or choose a new --profile-dir").into());
            }
            if let Ok(contents) = fs::read_to_string(self.profile.path.join("DevToolsActivePort")) {
                let mut lines = contents.lines();
                if let (Some(port), Some(path)) = (
                    lines.next().and_then(|s| s.parse::<u16>().ok()),
                    lines.next(),
                ) {
                    if path.starts_with("/devtools/browser/") {
                        let endpoint = format!("ws://127.0.0.1:{port}{path}");
                        if let Ok(ws) = WebSocket::connect_until(&endpoint, deadline) {
                            self.client = Some(Client {
                                connection: Connection::new(ws)?,
                                next_id: 0,
                                deadline: None,
                            });
                            self.port = port;
                            return Ok(());
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("browser did not expose CDP; use a non-default working profile (the daily default directory may reject remote debugging)".into())
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile.path
    }

    /// Startup navigation, only for the blank page created by this launch.
    /// Call after installing the recorder so the initial requests are observed.
    pub fn open_url(&self, url: &str) -> Result<()> {
        let target = Client::list_targets(self.port)?
            .into_iter()
            .find(|t| t.id == self.page)
            .ok_or("created page was closed")?;
        if target.url != "about:blank" {
            return Err("created page has already navigated; refusing to replace it".into());
        }
        let mut client = Client::attach(&target)?;
        let result = client.command("Page.navigate", json!({"url":url}))?;
        if let Some(error) = result["errorText"].as_str() {
            return Err(format!("could not open URL: {error}").into());
        }
        Ok(())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(mut client) = self.client.take() {
            client.set_deadline(Instant::now() + Duration::from_secs(2));
            let _ = client.command("Browser.close", json!({}));
        }
        for _ in 0..30 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Kill only this invocation's process tree, never every browser image.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.child.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
