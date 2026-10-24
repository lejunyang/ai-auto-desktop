//! CDP integration for `aad watch`.
//!
//! When the foreground application is a Chromium browser exposing a debug
//! port, CDP reads rendered text and in-page click/input activity directly,
//! bypassing the UIA tree's anonymous panes. The observer never opens the
//! browser or injects input; it only attaches to a port the user explicitly
//! enabled with `--remote-debugging-port`.

use aad_cdp::{Client, ObservedActivity, PageState, Target};
use std::collections::HashMap;

/// Tracks one attached CDP browser session across watch loops, keyed by the
/// page target id, plus the last rendered text used for the line diff.
pub struct CdpWatcher {
    clients: HashMap<String, AttachedPage>,
}

struct AttachedPage {
    client: Client,
    /// Lines seen on the previous read, for an incremental text diff.
    previous_lines: Vec<String>,
    previous_page: Option<(String, String)>,
}

/// One CDP observation result for a poll: foreground page metadata, new
/// rendered lines, removed lines, and in-page activity since the last poll.
pub struct CdpObservation {
    pub title: String,
    pub url: String,
    pub activity: Vec<ObservedActivity>,
    pub added_lines: Vec<String>,
    pub removed_lines: Vec<String>,
    pub dropped_events: u64,
    pub truncated: bool,
    pub page_changed: bool,
}

impl CdpWatcher {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Whether a debug endpoint answers on this port.
    pub fn available(port: u16) -> bool {
        Client::available(port)
    }

    /// Observe the foreground page of a debug-enabled browser.
    ///
    /// `title_hint` is the foreground window title from UIA, used only to pick
    /// the right tab when several pages are open. Returns None when no page
    /// target is attachable, so the caller falls back to UIA.
    pub fn observe(&mut self, port: u16, title_hint: &str) -> Option<CdpObservation> {
        let targets = Client::list_targets(port).ok()?;
        let target = pick_page(&targets, title_hint)?;
        let target_id = target.id.clone();

        // Attach lazily and reuse the WebSocket; re-pick if a stored client
        // died (target navigated/reloaded changes target ids anyway).
        if !self.clients.contains_key(&target_id) {
            let client = Client::attach(target).ok()?;
            self.clients.insert(
                target_id.clone(),
                AttachedPage {
                    client,
                    previous_lines: Vec::new(),
                    previous_page: None,
                },
            );
            // Drop sessions for targets that no longer exist, to bound memory
            // over a long watch.
            let live: std::collections::HashSet<String> =
                targets.iter().map(|target| target.id.clone()).collect();
            self.clients.retain(|id, _| live.contains(id));
        }

        let attached = self.clients.get_mut(&target_id)?;
        if let Err(error) = attached.client.ensure_installed() {
            eprintln!("watch: CDP gap for {target_id}: {error}; reconnecting on next poll");
            self.clients.remove(&target_id);
            return None;
        }
        let PageState {
            title,
            url,
            text,
            truncated,
        } = attached.client.state().ok()?;
        let report = match attached.client.drain_activity_report() {
            Ok(report) => report,
            Err(error) => {
                eprintln!("watch: CDP activity gap: {error}");
                return None;
            }
        };
        let page_changed = attached.previous_page.as_ref() != Some(&(title.clone(), url.clone()));
        attached.previous_page = Some((title.clone(), url.clone()));

        let current_lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();

        // Set-based line diff: new lines not present before, and prior lines
        // no longer present. Order-insensitive on purpose: rendered pages
        // reflow, and a line identity is its text.
        let before: std::collections::HashSet<&str> =
            attached.previous_lines.iter().map(String::as_str).collect();
        let now: std::collections::HashSet<&str> =
            current_lines.iter().map(String::as_str).collect();
        let added_lines: Vec<String> = current_lines
            .iter()
            .filter(|line| !before.contains(line.as_str()))
            .cloned()
            .collect();
        let removed_lines: Vec<String> = attached
            .previous_lines
            .iter()
            .filter(|line| !now.contains(line.as_str()))
            .cloned()
            .collect();
        attached.previous_lines = current_lines;

        Some(CdpObservation {
            title,
            url,
            activity: report.events,
            added_lines,
            removed_lines,
            dropped_events: report.dropped,
            truncated,
            page_changed,
        })
    }
}

impl Default for CdpWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Choose the foreground page target.
///
/// Prefer a page whose title/url matches the UIA foreground window title
/// (browsers put the page title in the tab/window title); otherwise take the
/// only a single eligible page is safe as a fallback. Ambiguity is not focus.
fn pick_page<'a>(targets: &'a [Target], title_hint: &str) -> Option<&'a Target> {
    let hint = title_hint.trim();
    if !hint.is_empty() {
        // Window title is usually "<page title> - <browser name>"; match when
        // the hint starts with or contains the page title.
        let matches: Vec<_> = targets
            .iter()
            .filter(|target| target.kind == "page")
            .filter(|target| !target.title.is_empty() && hint.starts_with(&target.title))
            .collect();
        if matches.len() == 1 {
            return Some(matches[0]);
        }
        if matches.len() > 1 {
            return None;
        }
    }
    let pages: Vec<_> = targets
        .iter()
        .filter(|target| target.kind == "page" && !target.url.starts_with("edge://"))
        .filter(|target| !target.url.starts_with("chrome://"))
        .collect();
    if pages.len() == 1 {
        Some(pages[0])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, kind: &str, title: &str, url: &str) -> Target {
        Target {
            kind: kind.into(),
            title: title.into(),
            url: url.into(),
            webSocketDebuggerUrl: String::new(),
            id: id.into(),
        }
    }

    #[test]
    fn matches_window_title_prefix_and_skips_internal_pages() {
        let targets = vec![
            target("ext", "background_page", "", "chrome-extension://x"),
            target("settings", "page", "设置", "edge://settings/"),
            target("news", "page", "今日新闻 - 某站", "https://news.example/a"),
        ];
        // Window title is the page title plus browser suffix.
        let picked = pick_page(&targets, "今日新闻 - 某站 - 个人 - Microsoft Edge").unwrap();
        assert_eq!(picked.id, "news");
    }

    #[test]
    fn falls_back_to_first_real_page_without_a_hint() {
        let targets = vec![
            target("bg", "background_page", "", "chrome-extension://x"),
            target("internal", "page", "edge", "edge://newtab/"),
            target("web", "page", "Example", "https://example.com/"),
        ];
        assert_eq!(pick_page(&targets, "").unwrap().id, "web");
    }

    #[test]
    fn ambiguous_pages_are_not_attributed_to_the_first_tab() {
        let targets = vec![
            target("a", "page", "Login", "https://a.example"),
            target("b", "page", "Login", "https://b.example"),
        ];
        assert!(pick_page(&targets, "Login - Chrome").is_none());
        assert!(pick_page(&targets, "").is_none());
    }
}
