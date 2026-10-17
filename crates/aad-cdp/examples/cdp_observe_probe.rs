// End-to-end probe for the read-only CDP client.
//
// Starts Edge with a debug port and a SEPARATE user-data-dir, attaches to the
// page, installs the passive listeners, drives a little activity (address
// bar is browser chrome so it is not in-page; this uses page inputs), and
// prints rendered text plus observed click/input events. Usage:
//   cdp_observe_probe [port]

use aad_cdp::{Client, PageState};
use std::process::Command;
use std::time::{Duration, Instant};

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(9350);
    let profile = std::env::temp_dir().join(format!("aad-cdp-probe-{port}"));
    let _ = std::fs::remove_dir_all(&profile);

    let edge = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).exists())
    .expect("install Edge or Chrome to run this probe");

    let mut browser = Command::new(edge)
        .args([
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            // A fresh profile with extensions/sync disabled: they add startup
            // noise and slow the debug endpoint on first launch.
            "--disable-extensions".to_string(),
            "--disable-sync".to_string(),
            "https://example.com".to_string(),
        ])
        .spawn()
        .expect("start the browser");
    eprintln!(
        "spawned browser pid={} port={} profile={}",
        browser.id(),
        port,
        profile.display()
    );

    let outcome = run(port);

    // The launched Edge is often a launcher process whose children keep the
    // browser (and the profile lock) alive; kill the whole profile tree, not
    // just the spawned pid, so repeated runs start clean.
    let _ = browser.kill();
    let _ = browser.wait();
    kill_profile_edge(&profile);

    match outcome {
        Ok(()) => println!("CDP_PROBE_OK"),
        Err(error) => {
            eprintln!("CDP_PROBE_FAILED: {error}");
            std::process::exit(1);
        }
    }
}

/// Best-effort cleanup of every Edge process launched against this throwaway
/// profile. The browser spawns a process tree, so killing only the spawned pid
/// leaves the profile locked for the next run.
#[cfg(windows)]
fn kill_profile_edge(profile: &std::path::Path) {
    let needle = profile.display().to_string().replace('\'', "''");
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe'\" | \
         Where-Object {{ $_.CommandLine -like '*{needle}*' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output();
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(profile);
}

#[cfg(not(windows))]
fn kill_profile_edge(_profile: &std::path::Path) {}

fn run(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    // The debug server may take noticeably longer than the HTTP connect retry
    // budget; first block until the port answers /json/version (which itself
    // retries internally), only then enumerate pages.
    let ready_deadline = Instant::now() + Duration::from_secs(25);
    while Client::version(port).is_err() {
        if Instant::now() > ready_deadline {
            return Err("debug endpoint never answered /json/version".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let target = loop {
        match Client::list_targets(port) {
            Ok(targets) => {
                let pick = targets
                    .iter()
                    .find(|target| target.url.contains("example.com"))
                    .or_else(|| targets.first())
                    .cloned();
                if let Some(target) = pick {
                    break target;
                }
                eprintln!("list ok but no page targets yet ({})", targets.len());
            }
            Err(error) => eprintln!("list_targets: {error}"),
        }
        if Instant::now() > deadline {
            return Err("no page target came up".into());
        }
        std::thread::sleep(Duration::from_millis(400));
    };

    let mut client = Client::attach(&target)?;

    // Give example.com time to render, then read text.
    std::thread::sleep(Duration::from_secs(2));
    let PageState { title, url, text } = client.state()?;
    println!("STATE title={title:?}\n      url={url}");
    println!(
        "TEXT_HEAD: {}",
        text.lines().take(3).collect::<Vec<_>>().join(" | ")
    );
    assert!(
        text.contains("Example Domain"),
        "rendered text must be readable"
    );

    // Install listeners and synthesize in-page activity via evaluation ONLY:
    // we do not drive Input.* (this is an observer, not an automator). The
    // listeners must still see programmatic trusted-less events because they
    // are plain bubbling events.
    client.ensure_installed()?;
    // Synthesize in-page activity through DOM events (NOT CDP Input domain):
    // validates the passive listeners without the product ever injecting input.
    client.eval(SELF_TEST)?;
    std::thread::sleep(Duration::from_millis(300));

    let events = client.drain_activity()?;
    println!("ACTIVITY events={}", events.len());
    for event in &events {
        let aad_cdp::ObservedActivity {
            kind,
            label,
            tag,
            href,
            value,
            input_type,
            ..
        } = event;
        println!(
            "  {kind:<5} tag={tag:<7?} type={input_type:<7?} label={label:<30?} value={value:<20?} href={href:?}"
        );
    }
    let click = events.iter().any(|event| event.kind == "click");
    let input = events.iter().any(|event| event.kind == "input");
    assert!(click, "a click activity must be observed");
    assert!(input, "an input activity must be observed");
    let rich = events
        .iter()
        .find(|event| event.tag.as_deref() == Some("contenteditable"))
        .expect("a contentEditable composer value must be observed");
    assert_eq!(rich.value.as_deref(), Some("hello rich editor"));

    // Password fields must not leak a value.
    let leaked = events
        .iter()
        .any(|event| event.input_type.as_deref() == Some("password") && event.value.is_some());
    assert!(!leaked, "password values must never be reported");

    Ok(())
}

const SELF_TEST: &str = r#"
(() => {
  const form = document.createElement('div');
  const input = document.createElement('input');
  input.type = 'text'; input.setAttribute('aria-label', 'probe field');
  input.value = 'typed via cdp';
  const secret = document.createElement('input');
  secret.type = 'password'; secret.value = 'topsecret';
  const link = document.createElement('a');
  link.href = 'https://example.com/next'; link.innerText = 'Next';
  const button = document.createElement('button'); button.innerText = 'HiBtn';
  // contentEditable rich-text composer (docs/chat style): read via innerText.
  const rich = document.createElement('div');
  rich.contentEditable = 'true';
  rich.setAttribute('aria-label', 'rich composer');
  rich.innerText = 'hello rich editor';
  form.append(input, secret, link, button, rich);
  document.body.append(form);
  input.dispatchEvent(new Event('input', {bubbles:true}));
  secret.dispatchEvent(new Event('input', {bubbles:true}));
  rich.dispatchEvent(new Event('input', {bubbles:true}));
  // HTMLElement.click() dispatches a bubbling click (a plain dispatched
  // MouseEvent on an <a> is not guaranteed to run the capture listener).
  button.click();
  return 1;
})()
"#;
