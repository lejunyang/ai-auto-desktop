//! Read-only Chrome DevTools Protocol (CDP) client.
//!
//! Purpose: for Chromium-based apps (Edge/Chrome browsers, and Electron apps
//! started with a debug port), read the *rendered* page text and user
//! activity directly from the page, bypassing the noisy/anonymous UIA tree.
//! See `docs/research/electron-cdp-observation.md`.
//!
//! Security posture:
//! - only connects to `127.0.0.1:<port>`; never opens the browser, never
//!   navigates or dispatches input (no `Input.*`, no `Page.navigate`);
//! - the browser must already be running with `--remote-debugging-port`;
//! - the injected script only attaches passive listeners and queues events;
//!   it is removed by a page reload/navigation and re-installed by
//!   [`Client::ensure_installed`].

use serde::Serialize;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

mod ws;

pub use ws::CdpError;

/// A page/frame target exposed at `/json/list`. The websocket field keeps the
/// devtools protocol's camelCase name; it is an external contract.
#[allow(non_snake_case)]
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Target {
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub url: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    pub webSocketDebuggerUrl: String,
    #[serde(default)]
    pub id: String,
}

/// One observed in-page activity event, produced by the injected listeners.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ObservedActivity {
    /// `click` or `input`.
    pub kind: String,
    /// Best-effort human label for the element (aria-label, text, value, id).
    pub label: Option<String>,
    /// Tag name, e.g. `a`, `input`, `button`.
    pub tag: Option<String>,
    /// Link href for anchors.
    pub href: Option<String>,
    /// Current value of input/textarea/select.
    pub value: Option<String>,
    /// Input type (`text`, `search`, `password`, ...).
    #[serde(rename = "type")]
    pub input_type: Option<String>,
    /// URL of the page when it happened.
    pub url: String,
}

/// A point-in-time reading of a tab.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PageState {
    pub title: String,
    pub url: String,
    /// Rendered visible text (`document.body.innerText`).
    pub text: String,
}

/// A connected CDP page session.
pub struct Client {
    ws: ws::WebSocket,
    next_id: u64,
}

impl Client {
    /// List debuggable targets over HTTP. Returns an error when no browser is
    /// listening on `port`, so callers can fall back to UIA.
    pub fn list_targets(port: u16) -> Result<Vec<Target>, CdpError> {
        let body = http_get(port, "/json/list")?;
        let targets: Vec<Target> = serde_json::from_str(&body).map_err(|error| {
            CdpError::protocol(format!("could not parse /json/list: {error}"))
        })?;
        Ok(targets
            .into_iter()
            .filter(|target| target.kind == "page" && !target.webSocketDebuggerUrl.is_empty())
            .collect())
    }

    /// Whether a CDP endpoint answers on this port (cheap probe).
    pub fn available(port: u16) -> bool {
        http_get(port, "/json/version").is_ok()
    }

    /// Browser identity from `/json/version`; used to wait for a cold start.
    pub fn version(port: u16) -> Result<String, CdpError> {
        let body = http_get(port, "/json/version")?;
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| CdpError::protocol(error.to_string()))?;
        Ok(value
            .get("Browser")
            .and_then(|browser| browser.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Attach to one page target over its WebSocket URL.
    pub fn attach(target: &Target) -> Result<Self, CdpError> {
        let ws = ws::WebSocket::connect_url(&target.webSocketDebuggerUrl)?;
        let mut client = Client { ws, next_id: 0 };
        client.enable_runtime()?;
        Ok(client)
    }

    fn send(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = serde_json::json!({"id": id, "method": method, "params": params});
        self.ws
            .send_text(&serde_json::to_string(&message).unwrap())?;

        // Read until the matching response, queuing any unrelated frames to the
        // internal buffer (handled by the WebSocket itself).
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let frame = self.ws.recv_until(deadline)?;
            if !frame.is_text() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(&frame.as_text()) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(CdpError::protocol(error.to_string()));
                }
                return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
            }
            // An event while waiting on a response: ignore here; the poll loop
            // reads events explicitly after draining responses.
        }
    }

    fn enable_runtime(&mut self) -> Result<(), CdpError> {
        self.send("Runtime.enable", serde_json::json!({}))?;
        Ok(())
    }

    /// Install the passive observation listeners into the current page if they
    /// are not already present. Idempotent; re-run after every navigation.
    pub fn ensure_installed(&mut self) -> Result<(), CdpError> {
        self.eval(INSTALL_JS)?;
        Ok(())
    }

    /// Read and clear the queue of activity events collected on the page.
    pub fn drain_activity(&mut self) -> Result<Vec<ObservedActivity>, CdpError> {
        // The injected code keeps a global array; return it and reset.
        let result = self.eval(DRAIN_JS)?;
        let value = result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let activities: Vec<ObservedActivity> = serde_json::from_value(value).unwrap_or_default();
        Ok(activities)
    }

    /// Current title/url/rendered text.
    pub fn state(&mut self) -> Result<PageState, CdpError> {
        let result = self.eval(STATE_JS)?;
        let raw = result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(raw).map_err(|error| CdpError::protocol(format!("bad state: {error}")))
    }

    /// Evaluate an expression and return its value (returnByValue).
    ///
    /// Read-only in intent; the ambient observer never calls `Input.*` or
    /// `Page.navigate`. Public so the CLI watch loop can also run the
    /// install/drain/state scripts.
    pub fn eval(&mut self, expression: &str) -> Result<serde_json::Value, CdpError> {
        self.send(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
    }
}

fn http_get(port: u16, path: &str) -> Result<String, CdpError> {
    // A plain blocking connect. An earlier connect_timeout form left the
    // socket nonblocking on Windows, which made the subsequent HTTP read fail
    // (measured: plain TcpStream::connect succeeds where the timeout form
    // timed out). Cold-start waiting is the caller's job (Client::version).
    let mut stream =
        TcpStream::connect(format!("127.0.0.1:{port}")).map_err(|error| {
            CdpError::connect(format!("port {port}: {error}"))
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|error| CdpError::io(error))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(CdpError::io)?;

    // The devtools server is authoritative about the body length; read exactly
    // Content-Length rather than read_to_end, which blocked until timeout when
    // the server held the connection open after sending the response.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut content_length: Option<usize> = None;
    loop {
        let read = stream.read(&mut chunk).map_err(CdpError::io)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if content_length.is_none() {
            if let Some(headers) = raw
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|split| split + 4)
            {
                let header_text = String::from_utf8_lossy(&raw[..headers]);
                let status_ok = header_text.starts_with("HTTP/1.1 200");
                if !status_ok {
                    return Err(CdpError::connect(format!(
                        "non-200 from /json: {}",
                        header_text.lines().next().unwrap_or("")
                    )));
                }
                content_length = header_text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    });
            }
        }
        if let Some(length) = content_length {
            if let Some(headers_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_start = headers_end + 4;
                if raw.len() >= body_start + length {
                    raw.truncate(body_start + length);
                    break;
                }
            }
        }
        if raw.len() > 16 * 1024 * 1024 {
            return Err(CdpError::protocol("http response too large"));
        }
    }

    let text = String::from_utf8_lossy(&raw);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
        .to_string();
    Ok(body)
}

/// Script installed into every observed page.
///
/// Passive, namespaced under `window.__aadCdp`, no input/navigation. Captures
/// clicks (resolved to the nearest clickable ancestor) and input/change
/// values. Password fields contribute an event with no value. A per-element
/// micro-coalescence keeps typing from flooding the queue.
const INSTALL_JS: &str = r#"
(() => {
  if (window.__aadCdp && window.__aadCdp.installed) return 1;
  const q = [];
  const labelOf = (el) => {
    if (!el || el === document) return null;
    const a = el.getAttribute && (el.getAttribute('aria-label') || el.getAttribute('title'));
    if (a) return a;
    const t = (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ');
    if (t) return t.slice(0, 120);
    if (el.id) return '#' + el.id;
    return null;
  };
  const clickable = (el) => {
    let n = el;
    for (let i = 0; i < 4 && n; i++) {
      const tag = (n.tagName || '').toLowerCase();
      if (['a','button','[role=button]'].includes(tag) || n.getAttribute('role') === 'button' ||
          n.onclick || n.tabIndex >= 0) return n;
      n = n.parentElement;
    }
    return el;
  };
  document.addEventListener('click', (e) => {
    const el = clickable(e.target);
    q.push({
      kind: 'click',
      tag: el.tagName ? el.tagName.toLowerCase() : null,
      label: labelOf(el),
      href: el.href || null,
      value: null,
      type: null,
      url: location.href,
    });
  }, true);
  const onInput = (e) => {
    const el = e.target;
    if (!el || !el.tagName) return;
    const tag = el.tagName.toLowerCase();
    if (!['input','textarea','select'].includes(tag)) return;
    const isPassword = (el.type || '').toLowerCase() === 'password';
    q.push({
      kind: 'input',
      tag,
      label: labelOf(el),
      href: null,
      value: isPassword ? null : (el.value != null ? String(el.value).slice(0, 500) : null),
      type: el.type || (tag === 'textarea' ? 'textarea' : null),
      url: location.href,
    });
  };
  document.addEventListener('input', onInput, true);
  document.addEventListener('change', onInput, true);
  window.__aadCdp = { installed: true, take: () => q.splice(0, q.length) };
  return 1;
})()
"#;

const DRAIN_JS: &str = r#"
(() => {
  if (!window.__aadCdp) return [];
  return window.__aadCdp.take();
})()
"#;

const STATE_JS: &str = r#"
(() => ({
  title: document.title || "",
  url: location.href || "",
  text: (document.body ? document.body.innerText : "").slice(0, 200000),
}))()
"#;
