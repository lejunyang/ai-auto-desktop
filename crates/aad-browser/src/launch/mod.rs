//! Explicit browser startup shared with aad-browser. Recording owns only the
//! process/profile it launches; attaching to an existing page stays separate.
mod lease;
mod occupancy;
mod profile;
pub use occupancy::ensure_available;
pub use profile::{
    browser_exe, copy_login_profile, copy_selected_profile, list_profiles, real_user_data,
    select_profile, selected_profile, validate_copy_destination,
};

use crate::{cdp::Client, connection::Connection, ws::WebSocket};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::time::{Duration, Instant};

type Result<T> = crate::recording::Result<T>;

pub trait Authority {
    fn cancellation(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }
    fn check(&self) -> Result<()> {
        Ok(())
    }
    fn navigation(&self) -> Result<()> {
        self.check()
    }
    fn effect(&self, action: &mut dyn FnMut() -> Result<()>) -> Result<()>;
}
struct Unrestricted;
impl Authority for Unrestricted {
    fn effect(&self, action: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        action()
    }
}

pub struct Options {
    pub browser: String,
    pub executable: Option<PathBuf>,
    /// Reusable user-data directory; omitted means a fresh temporary profile.
    pub profile_dir: Option<PathBuf>,
    /// Copy login files from this CLOSED user-data directory into a new profile.
    pub copy_from: Option<PathBuf>,
    pub profile_id: String,
    pub kill_browser: bool,
    pub headless: bool,
}

struct Profile {
    path: PathBuf,
    temporary: bool,
    lock: Arc<Mutex<Option<lease::Lease>>>,
}

impl Profile {
    fn prepare(options: &Options, authority: &dyn Authority) -> Result<Self> {
        let path = options.profile_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("aad-record-profile-{}", uuid::Uuid::new_v4()))
        });
        if let Some(source) = &options.copy_from {
            selected_profile(source, &options.profile_id)?;
            profile::validate_copy_destination(source, &path)?;
            occupancy::ensure_available_guarded(
                source,
                &options.browser,
                options.kill_browser,
                authority,
            )?;
            // Keep the source unavailable to other AAD launches while copying.
            let _source = lease::Lease::acquire(source)?;
            copy_selected_profile(source, &path, &options.profile_id)?;
        } else {
            fs::create_dir_all(&path)?;
        }
        // Chromium on Windows silently skips persisting cookies when its
        // user-data-dir is a verbatim `\\?\` path, so keep the plain form.
        let path = dunce::canonicalize(path)?;
        let profile = Self {
            path,
            temporary: options.profile_dir.is_none(),
            lock: Arc::new(Mutex::new(None)),
        };
        occupancy::ensure_available_guarded(
            &profile.path,
            &options.browser,
            options.kill_browser,
            authority,
        )?;
        *profile.lock.lock().unwrap() = Some(lease::Lease::acquire(&profile.path)?);
        // A new launch must write a fresh endpoint file. Never attach to a stale
        // port or assume that a pre-existing endpoint belongs to this process.
        let active = profile.path.join("DevToolsActivePort");
        if active.exists() {
            fs::remove_file(active)?;
        }
        Ok(profile)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        self.lock.lock().unwrap().take();
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
    child: Arc<Mutex<Child>>,
    watcher: Option<Sender<()>>,
    profile: Profile,
    pub port: u16,
    pub page: String,
}

impl Browser {
    pub fn process_id(&self) -> u32 {
        self.child.lock().unwrap().id()
    }

    pub fn start(options: Options) -> Result<Self> {
        Self::start_guarded(options, &Unrestricted)
    }
    pub fn start_guarded(options: Options, authority: &dyn Authority) -> Result<Self> {
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
        let profile = Profile::prepare(&options, authority)?;
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
            .arg("--profile-directory=Default")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if options.headless {
            // Allows trusted Browser.getBrowserCommandLine verification, so
            // attached control sessions can share this instance's input lease
            // without unnecessarily owning the physical desktop.
            command.args(["--headless=new", "--enable-automation"]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = None;
        authority.effect(&mut || {
            child = Some(command.spawn()?);
            Ok(())
        })?;
        let child = child.unwrap();
        let mut browser = Self {
            client: None,
            child: Arc::new(Mutex::new(child)),
            watcher: None,
            profile,
            port: 0,
            page: String::new(),
        };
        browser.wait_ready(authority)?;
        authority.check()?;
        if let Some(cancel) = authority.cancellation() {
            browser.client.as_mut().unwrap().set_cancellation(cancel);
        }
        let result = browser.client.as_mut().unwrap().command(
            "Target.createTarget",
            json!({"url":"about:blank", "newWindow":true}),
        )?;
        browser.page = result["targetId"]
            .as_str()
            .ok_or("missing newly created page ID")?
            .into();
        authority.check()?;
        browser.watcher = Some(lease::watch_child(&browser.child, &browser.profile.lock));
        Ok(browser)
    }

    fn wait_ready(&mut self, authority: &dyn Authority) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            authority.check()?;
            if let Some(status) = self.child.lock().unwrap().try_wait()? {
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
                        if let Ok(ws) = WebSocket::connect_cancellable(
                            &endpoint,
                            deadline,
                            authority.cancellation().as_deref(),
                        ) {
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
        self.open_url_guarded(url, &Unrestricted)
    }
    pub fn open_url_guarded(&self, url: &str, authority: &dyn Authority) -> Result<()> {
        authority.check()?;
        let cancel = authority.cancellation();
        let deadline = Instant::now() + Duration::from_secs(10);
        let target = Client::list_targets_cancellable(self.port, deadline, cancel.as_deref())?
            .into_iter()
            .find(|t| t.id == self.page)
            .ok_or("created page was closed")?;
        if target.url != "about:blank" {
            return Err("created page has already navigated; refusing to replace it".into());
        }
        let mut client = Client::attach_cancellable(&target, deadline, cancel)?;
        if let Some(cancel) = authority.cancellation() {
            client.set_cancellation(cancel);
        }
        authority.navigation()?;
        let result = client.command("Page.navigate", json!({"url":url}))?;
        if let Some(error) = result["errorText"].as_str() {
            return Err(format!("could not open URL: {error}").into());
        }
        Ok(())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(mut client) = self.client.take() {
            client.set_deadline(Instant::now() + Duration::from_secs(2));
            let _ = client.command("Browser.close", json!({}));
        }
        for _ in 0..30 {
            if self
                .child
                .lock()
                .unwrap()
                .try_wait()
                .ok()
                .flatten()
                .is_some()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Kill only this invocation's process tree, never every browser image.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.process_id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.process_id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
