//! Read-only Chrome DevTools Protocol (CDP) client.
//!
//! Purpose: for Chromium-based apps (Edge/Chrome browsers, and Electron apps
//! started with a debug port), read the *rendered* page text and user
//! activity directly from the page, bypassing the noisy/anonymous UIA tree.
//! See `docs/research/electron-cdp-observation.md`.
//!
//! Security posture:
//! - only connects to loopback; the ambient observer never opens the browser,
//!   navigates or dispatches input. `aad-control` uses the trusted low-level
//!   command primitive for separately authorized, observed-element actions;
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
    deadline: Option<std::time::Instant>,
}

impl Client {
    /// List debuggable targets over HTTP. Returns an error when no browser is
    /// listening on `port`, so callers can fall back to UIA.
    pub fn list_targets(port: u16) -> Result<Vec<Target>, CdpError> {
        Self::list_targets_until(port, std::time::Instant::now() + Duration::from_secs(3))
    }
    pub fn list_targets_until(
        port: u16,
        deadline: std::time::Instant,
    ) -> Result<Vec<Target>, CdpError> {
        let body = http_get_until(port, "/json/list", deadline)?;
        let targets: Vec<Target> = serde_json::from_str(&body)
            .map_err(|error| CdpError::protocol(format!("could not parse /json/list: {error}")))?;
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
        Self::attach_until(target, std::time::Instant::now() + Duration::from_secs(10))
    }

    pub fn attach_until(target: &Target, deadline: std::time::Instant) -> Result<Self, CdpError> {
        let ws = ws::WebSocket::connect_until(&target.webSocketDebuggerUrl, deadline)?;
        let mut client = Client {
            ws,
            next_id: 0,
            deadline: Some(deadline),
        };
        client.enable_runtime()?;
        client.deadline = None;
        Ok(client)
    }

    /// Deadline for a trusted application's sequence of CDP calls. This is a
    /// library primitive, never a pass-through method exposed to AI clients.
    pub fn set_deadline(&mut self, deadline: std::time::Instant) {
        self.deadline = Some(deadline);
    }

    pub fn command(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CdpError> {
        self.send(method, params)
    }

    fn send(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = serde_json::json!({"id": id, "method": method, "params": params});
        let deadline = self
            .deadline
            .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(8));
        self.ws.set_write_deadline(deadline)?;
        self.ws
            .send_text(&serde_json::to_string(&message).unwrap())?;

        // Read until the matching response, queuing any unrelated frames to the
        // internal buffer (handled by the WebSocket itself).
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
                return Ok(value
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
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
        serde_json::from_value(raw)
            .map_err(|error| CdpError::protocol(format!("bad state: {error}")))
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
    http_get_until(
        port,
        path,
        std::time::Instant::now() + Duration::from_secs(3),
    )
}
fn http_get_until(port: u16, path: &str, deadline: std::time::Instant) -> Result<String, CdpError> {
    let remaining = || {
        deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| CdpError::connect("HTTP deadline elapsed"))
    };
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, remaining()?)
        .map_err(|error| CdpError::connect(format!("port {port}: {error}")))?;
    // Explicitly restore blocking mode after connect_timeout, including Windows.
    stream.set_nonblocking(false).map_err(CdpError::io)?;
    stream
        .set_write_timeout(Some(remaining()?))
        .map_err(CdpError::io)?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(CdpError::io)?;

    // The devtools server is authoritative about the body length; read exactly
    // Content-Length rather than read_to_end, which blocked until timeout when
    // the server held the connection open after sending the response.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut content_length: Option<usize> = None;
    let mut header_end = None;
    loop {
        stream
            .set_read_timeout(Some(remaining()?))
            .map_err(CdpError::io)?;
        let read = stream.read(&mut chunk).map_err(CdpError::io)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            if let Some(headers) = raw
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|split| split + 4)
            {
                header_end = Some(headers);
                let header_text = String::from_utf8_lossy(&raw[..headers]);
                let status_ok = header_text.starts_with("HTTP/1.1 200");
                if !status_ok {
                    return Err(CdpError::connect(format!(
                        "non-200 from /json: {}",
                        header_text.lines().next().unwrap_or("")
                    )));
                }
                for line in header_text.lines() {
                    let line = line.to_ascii_lowercase();
                    if let Some(value) = line.strip_prefix("content-length:") {
                        let value = value
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| CdpError::protocol("invalid content length"))?;
                        if value > 16 * 1024 * 1024 {
                            return Err(CdpError::protocol("http response too large"));
                        }
                        content_length = Some(value);
                    }
                }
            } else if raw.len() > 16384 {
                return Err(CdpError::protocol("http header too large"));
            }
        }
        if let (Some(length), Some(body_start)) = (content_length, header_end) {
            let end = body_start + length;
            if raw.len() >= end {
                raw.truncate(end);
                break;
            }
        }
        if raw.len() > 16 * 1024 * 1024 {
            return Err(CdpError::protocol("http response too large"));
        }
    }

    let start = header_end.ok_or_else(|| CdpError::protocol("incomplete HTTP header"))?;
    if content_length.is_some_and(|length| raw.len() - start != length) {
        return Err(CdpError::protocol("incomplete HTTP body"));
    }
    String::from_utf8(raw[start..].to_vec())
        .map_err(|_| CdpError::protocol("HTTP body is not UTF-8"))
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
  // Capture phase so framework routers (React/Vue) that stopPropagation still
  // get observed; resolve to the nearest clickable ancestor for a label.
  const onClick = (e) => {
    const el = clickable(e.target);
    q.push({
      kind: 'click',
      tag: el.tagName ? el.tagName.toLowerCase() : null,
      label: labelOf(el),
      href: el.href || el.closest && el.closest('a') ? (el.closest('a') || {}).href || null : null,
      value: null,
      type: null,
      url: location.href,
    });
  };
  document.addEventListener('click', onClick, true);
  // contentEditable / rich-text editors (docs, chat composers) do not expose
  // a form .value; read their text content instead.
  const editorValue = (el) => {
    if (el.isContentEditable) return (el.innerText || el.textContent || '');
    return el.value != null ? String(el.value) : (el.textContent || '');
  };
  const onInput = (e) => {
    const el = e.target;
    if (!el || !el.tagName) return;
    const tag = el.tagName.toLowerCase();
    const editable = ['input','textarea','select'].includes(tag) || el.isContentEditable;
    if (!editable) return;
    const isPassword = tag === 'input' && (el.type || '').toLowerCase() === 'password';
    q.push({
      kind: 'input',
      tag: el.isContentEditable ? 'contenteditable' : tag,
      label: labelOf(el),
      href: null,
      value: isPassword ? null : editorValue(el).slice(0, 500),
      type: tag === 'input' ? (el.type || null)
            : tag === 'textarea' ? 'textarea' : 'richtext',
      url: location.href,
    });
  };
  document.addEventListener('input', onInput, true);
  document.addEventListener('change', onInput, true);

  // Some web inputs and IME composition omit per-keystroke 'input' or commit
  // only at the end; read the focused editable on keyup / compositionend.
  const readActive = () => {
    const el = document.activeElement;
    if (el && (el.isContentEditable ||
        ['INPUT','TEXTAREA','SELECT'].includes(el.tagName))) {
      onInput({target: el});
    }
  };
  document.addEventListener('keyup', readActive, true);
  document.addEventListener('compositionend', readActive, true);

  window.__aadCdp = { installed: true, take: () => q.splice(0, q.length) };
  return 1;
})()
"#;

const DRAIN_JS: &str = r#"
(() => {
  if (!window.__aadCdp) return [];
  const raw = window.__aadCdp.take();
  // Coalesce the per-keystroke 'input' storm into one event per field,
  // keeping the latest value; de-duplicate identical click events.
  const byField = new Map();
  const clicks = [];
  const seenClick = new Set();
  for (const e of raw) {
    if (e.kind === 'input') {
      const key = (e.tag||'') + '|' + (e.type||'') + '|' + (e.label||'') + '|' + (e.url||'');
      byField.set(key, e);
    } else if (e.kind === 'click') {
      const key = (e.tag||'') + '|' + (e.label||'') + '|' + (e.href||'');
      if (!seenClick.has(key)) { seenClick.add(key); clicks.push(e); }
    } else {
      clicks.push(e);
    }
  }
  return clicks.concat(Array.from(byField.values()));
})()
"#;

const STATE_JS: &str = r#"
(() => ({
  title: document.title || "",
  url: location.href || "",
  text: (document.body ? document.body.innerText : "").slice(0, 200000),
}))()
"#;

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;
    fn server(response: Vec<u8>, trickle: bool) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            if trickle {
                for byte in response {
                    std::thread::sleep(Duration::from_millis(15));
                    if stream.write_all(&[byte]).is_err() {
                        break;
                    }
                }
            } else {
                let _ = stream.write_all(&response);
            }
        });
        (port, worker)
    }
    #[test]
    fn malicious_content_length_is_rejected_without_integer_overflow() {
        let (port, worker) = server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n[]",
                usize::MAX
            )
            .into_bytes(),
            false,
        );
        assert!(
            http_get_until(port, "/json/list", Instant::now() + Duration::from_secs(1)).is_err()
        );
        worker.join().unwrap();
    }
    #[test]
    fn discovery_uses_the_request_deadline_even_when_headers_trickle() {
        let (port, worker) = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]".to_vec(),
            true,
        );
        let start = Instant::now();
        assert!(Client::list_targets_until(port, start + Duration::from_millis(60)).is_err());
        assert!(start.elapsed() < Duration::from_millis(300));
        worker.join().unwrap();
    }
}
