use super::{Recorder, Result};
use crate::{cdp::CdpError, launch::Browser};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, mpsc};
use std::time::{Duration, Instant};

pub(super) struct StartupNavigation {
    reply: mpsc::Receiver<std::result::Result<Value, CdpError>>,
    frame: String,
    request_url: String,
    loader: Option<String>,
    committed: bool,
    deadline: Instant,
}

impl StartupNavigation {
    fn start(recorder: &mut Recorder, browser: &Browser, url: &str) -> Result<Self> {
        if recorder.options.port != browser.port || recorder.pages[0].id != browser.page {
            return Err("the recorder is not attached to the launched page".into());
        }
        let client = &mut recorder.pages[0].client;
        let tree = client.command("Page.getFrameTree", json!({}))?;
        let frame = &tree["frameTree"]["frame"];
        if frame["url"] != "about:blank" {
            return Err("created page has already navigated; refusing to replace it".into());
        }
        let frame = frame["id"].as_str().ok_or("missing main frame")?.to_owned();
        let mut url = url::Url::parse(url)?;
        let reply = client.begin_session_command(
            "",
            "Page.navigate",
            json!({"url":url.as_str(),"frameId":frame}),
        )?;
        url.set_fragment(None);
        Ok(Self {
            reply,
            frame,
            request_url: url.into(),
            loader: None,
            committed: false,
            deadline: Instant::now() + Duration::from_secs(10),
        })
    }

    // Called only for events from the launched page's root CDP session.
    pub(super) fn observe(&mut self, method: &str, p: &Value) {
        match method {
            "Network.requestWillBeSent"
                if self.loader.is_none()
                    && p["type"] == "Document"
                    && p["frameId"] == self.frame
                    && p["request"]["url"] == self.request_url
                    && p["redirectResponse"].is_null() =>
            {
                self.loader = p["loaderId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            // Redirects retain the navigation loader. A different main-frame
            // navigation or Chromium's error page cannot confirm this one.
            "Page.frameNavigated"
                if p["frame"]["parentId"].is_null()
                    && p["frame"]["id"] == self.frame
                    && p["frame"]["unreachableUrl"]
                        .as_str()
                        .unwrap_or("")
                        .is_empty()
                    && self
                        .loader
                        .as_ref()
                        .is_some_and(|id| p["frame"]["loaderId"] == *id) =>
            {
                self.committed = true
            }
            _ => (),
        }
    }

    fn confirmed(&self) -> Result<bool> {
        match self.reply.try_recv() {
            Ok(reply) => {
                let reply = reply?;
                if let Some(error) = reply["errorText"].as_str() {
                    return Err(format!("could not open URL: {error}").into());
                }
                if reply["frameId"] != self.frame || reply["isDownload"] == true {
                    return Err("startup navigation did not open the requested page".into());
                }
                Ok(true)
            }
            Err(mpsc::TryRecvError::Empty) if self.committed => Ok(true),
            Err(mpsc::TryRecvError::Empty) if Instant::now() < self.deadline => Ok(false),
            Err(mpsc::TryRecvError::Empty) => {
                Err("startup navigation confirmation timed out".into())
            }
            Err(mpsc::TryRecvError::Disconnected) => Err("startup navigation disconnected".into()),
        }
    }
}

impl Recorder {
    /// Dispatch once on the installed recorder session, so the event loop can
    /// process network evidence and stop commands while navigation is pending.
    pub fn run_from_blank(
        mut self,
        browser: &Browser,
        url: &str,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        match StartupNavigation::start(&mut self, browser, url) {
            Ok(navigation) => self.navigation = Some(navigation),
            Err(error) => {
                self.finish(Some(error.to_string()))?;
                return Err(error);
            }
        }
        self.run(cancel)
    }

    pub(super) fn check_navigation(&mut self) -> Result<()> {
        if let Some(nav) = &self.navigation {
            if nav.confirmed()? {
                self.navigation = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_reply_requires_the_committed_loader_of_the_requested_document() {
        let (sender, reply) = mpsc::channel();
        let mut nav = StartupNavigation {
            reply,
            frame: "main".into(),
            request_url: "http://example.test/start".into(),
            loader: None,
            committed: false,
            deadline: Instant::now() + Duration::from_secs(10),
        };
        nav.observe("Network.requestWillBeSent", &json!({
            "frameId":"main","type":"Document","request":{"url":nav.request_url},"loaderId":"initial"
        }));
        for frame in [
            json!({"id":"main","loaderId":"other"}),
            json!({"id":"child","parentId":"main","loaderId":"initial"}),
            json!({"id":"main","loaderId":"initial","unreachableUrl":"http://example.test/start"}),
        ] {
            nav.observe("Page.frameNavigated", &json!({"frame":frame}));
            assert!(!nav.confirmed().unwrap());
        }
        nav.observe(
            "Page.frameNavigated",
            &json!({
                "frame":{"id":"main","loaderId":"initial","url":"http://example.test/redirected"}
            }),
        );
        assert!(nav.confirmed().unwrap());
        sender
            .send(Ok(json!({"frameId":"main","errorText":"net::ERR_ABORTED"})))
            .unwrap();
        assert!(nav
            .confirmed()
            .unwrap_err()
            .to_string()
            .contains("net::ERR_ABORTED"));
    }
}
