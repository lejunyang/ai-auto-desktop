//! A persistent, explicitly attached Firefox WebDriver BiDi session.
//!
//! Only the local WebSocket framing/request multiplexer is shared with CDP.
//! No CDP command, page attach, fallback or browser launch happens here.

use aad_cdp::transport::{Connection, EventBatch, TransportError, WebSocket};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, TransportError>;

/// Firefox permits one active BiDi session. Keep its socket alive across reads
/// and actions, and end only the session created by this client.
pub struct Client {
    connection: Connection,
    next_id: u64,
    session_id: String,
    capabilities: Value,
    ended: bool,
}

impl Client {
    /// Connect only to the operator-selected IPv4 loopback port. No arbitrary
    /// WebSocket URL, credentials, proxy, Origin override or remote host exists.
    pub fn connect(port: u16, deadline: Instant) -> Result<Self> {
        if port == 0 {
            return Err(TransportError::BadUrl);
        }
        let socket = WebSocket::connect_until(&format!("ws://127.0.0.1:{port}/session"), deadline)?;
        let mut client = Self {
            connection: Connection::new(socket)?,
            next_id: 1,
            session_id: String::new(),
            capabilities: Value::Null,
            ended: true,
        };
        client.connection.filter(
            Some(&[
                "browsingContext.contextDestroyed",
                "browsingContext.navigationStarted",
                "script.realmDestroyed",
                "browsingContext.userPromptOpened",
            ]),
            &["browsingContext.userPromptOpened"],
        );
        let session = client.command(
            "session.new",
            json!({"capabilities":{"alwaysMatch":{
                "browserName":"firefox", "unhandledPromptBehavior":{"default":"ignore"}
            }}}),
            deadline,
            None,
        )?;
        client.session_id = session["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| TransportError::protocol("BiDi returned no session identity"))?
            .into();
        client.ended = false;
        client.capabilities = session["capabilities"].clone();
        if client.capabilities["browserName"] != "firefox" {
            return Err(TransportError::protocol("BiDi endpoint is not Firefox"));
        }
        client.command(
            "session.subscribe",
            json!({"events":["browsingContext.contextDestroyed","browsingContext.navigationStarted",
                "script.realmDestroyed","browsingContext.userPromptOpened"]}),
            deadline,
            None,
        )?;
        Ok(client)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Browser-provided capabilities, never page JavaScript or a user agent
    /// heuristic. A provider must apply its own policy to use this evidence.
    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Internal protocol primitive; it is not exposed as a public control tool.
    /// An abandoned request's late response is discarded by the multiplexer.
    pub fn command(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        if Instant::now() >= deadline
            || cancel
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst))
        {
            return Err(TransportError::protocol(
                "BiDi request expired or cancelled",
            ));
        }
        match cancel {
            Some(cancel) => self.connection.set_cancellation(cancel),
            None => self.connection.clear_cancellation(),
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| TransportError::protocol("BiDi request identity exhausted"))?;
        self.connection.command(
            id,
            &json!({"id":id,"method":method,"params":params}),
            deadline,
        )
    }

    pub fn events(&self) -> EventBatch {
        self.connection.drain()
    }

    /// End our automation session; this does not close the browser or its tabs.
    pub fn end(&mut self, deadline: Instant) -> Result<()> {
        if !self.ended {
            self.command("session.end", json!({}), deadline, None)?;
            self.ended = true;
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.end(Instant::now() + Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_zero_port_without_network() {
        assert!(matches!(
            Client::connect(0, Instant::now() + Duration::from_secs(1)),
            Err(TransportError::BadUrl)
        ));
    }

    /// Run against an explicitly started private headless Firefox fixture.
    #[test]
    #[ignore = "needs AAD_TEST_BIDI_PORT for an isolated Firefox instance"]
    fn firefox_session_roundtrip_and_competing_session() {
        let port = std::env::var("AAD_TEST_BIDI_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let until = || Instant::now() + Duration::from_secs(5);
        let mut first = Client::connect(port, until()).unwrap();
        assert_eq!(first.capabilities()["browserName"], "firefox");
        assert_eq!(first.capabilities()["moz:headless"], true);
        assert!(Client::connect(port, until()).is_err());
        let tree = first
            .command("browsingContext.getTree", json!({}), until(), None)
            .unwrap();
        let context = tree["contexts"][0]["context"].as_str().unwrap();
        let result = first.command("script.evaluate", json!({"target":{"context":context,"sandbox":"aad-bidi-transport-test"}, "expression":"6 * 7", "awaitPromise":false}), until(), None).unwrap();
        assert_eq!(result["result"]["value"], 42);
        let cancelled = Arc::new(AtomicBool::new(true));
        assert!(first
            .command(
                "browsingContext.getTree",
                json!({}),
                until(),
                Some(cancelled)
            )
            .is_err());
        assert!(first
            .command("browsingContext.getTree", json!({}), until(), None)
            .is_ok());
        assert!(first.end(Instant::now()).is_err());
        first.end(until()).unwrap();
        let second = Client::connect(port, until()).unwrap();
        assert_ne!(first.session_id(), second.session_id());
    }
}
