//! One continuous socket reader; command replies never compete with observers.
use crate::{ws::WebSocket, CdpError};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_EVENTS: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Inbox {
    pending: HashMap<u64, mpsc::SyncSender<Result<Value, CdpError>>>,
    events: VecDeque<(Value, usize)>,
    bytes: usize,
    dropped: u64,
    failure: Option<CdpError>,
    /// Only these event methods are queued; `None` queues every event.
    keep: Option<HashSet<String>>,
    /// These events fail every command waiting for its reply.
    interrupt: HashSet<String>,
}

impl Inbox {
    fn filter(&mut self, keep: Option<&[&str]>, interrupt: &[&str]) {
        self.keep = keep.map(|methods| methods.iter().map(|m| m.to_string()).collect());
        self.interrupt = interrupt.iter().map(|m| m.to_string()).collect();
        if let Some(keep) = &self.keep {
            self.events
                .retain(|(event, _)| event["method"].as_str().is_some_and(|m| keep.contains(m)));
            self.bytes = self.events.iter().map(|(_, size)| size).sum();
        }
    }
    fn arrived(&mut self, value: Value, bytes: usize) {
        let method = value["method"].as_str().unwrap_or_default().to_string();
        if self.interrupt.contains(&method) {
            for (_, reply) in self.pending.drain() {
                let _ = reply.send(Err(CdpError::Interrupted(method.clone())));
            }
        }
        if self.keep.as_ref().is_none_or(|keep| keep.contains(&method)) {
            self.event(value, bytes);
        }
    }
    fn event(&mut self, value: Value, bytes: usize) {
        if bytes > MAX_BYTES {
            self.dropped += 1;
            return;
        }
        while self.events.len() >= MAX_EVENTS || self.bytes + bytes > MAX_BYTES {
            if let Some((_, size)) = self.events.pop_front() {
                self.bytes -= size;
                self.dropped += 1;
            }
        }
        self.bytes += bytes;
        self.events.push_back((value, bytes));
    }
}

/// A drain includes loss and disconnect information, even when no events remain.
#[derive(Debug)]
pub struct EventBatch {
    pub events: Vec<Value>,
    pub dropped: u64,
    pub failure: Option<CdpError>,
}

pub(crate) struct Connection {
    socket: WebSocket,
    inbox: Arc<Mutex<Inbox>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Connection {
    pub fn new(socket: WebSocket) -> Result<Self, CdpError> {
        let mut read = socket.reader()?;
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let shared = inbox.clone();
        let reader = std::thread::spawn(move || {
            let failure = loop {
                // No short idle timeout: abandoning a partially read frame
                // corrupts the stream. Drop shuts the socket down to interrupt.
                let frame = match read.recv_until(Instant::now() + Duration::from_secs(86400)) {
                    Ok(frame) => frame,
                    Err(error) => break error,
                };
                if !frame.is_text() {
                    continue;
                }
                let text = frame.as_text();
                let value: Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(error) => break CdpError::protocol(error.to_string()),
                };
                let mut state = shared.lock().unwrap();
                if let Some(id) = value["id"].as_u64() {
                    if let Some(reply) = state.pending.remove(&id) {
                        let result = match value.get("error") {
                            Some(error) => Err(CdpError::protocol(error.to_string())),
                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = reply.send(result);
                    }
                    // A late reply to a timed-out command is not an event.
                } else if value["method"].is_string() {
                    state.arrived(value, text.len());
                }
            };
            let mut state = shared.lock().unwrap();
            state.failure = Some(failure.clone());
            for (_, reply) in state.pending.drain() {
                let _ = reply.send(Err(failure.clone()));
            }
        });
        Ok(Self {
            socket,
            inbox,
            reader: Some(reader),
        })
    }

    pub fn command(
        &mut self,
        id: u64,
        message: &Value,
        deadline: Instant,
    ) -> Result<Value, CdpError> {
        let (send, recv) = mpsc::sync_channel(1);
        {
            let mut state = self.inbox.lock().unwrap();
            if let Some(error) = &state.failure {
                return Err(error.clone());
            }
            state.pending.insert(id, send);
        }
        let result = (|| {
            self.socket.set_write_deadline(deadline)?;
            self.socket.send_text(&message.to_string())?;
            recv.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| CdpError::protocol("command deadline elapsed"))?
        })();
        self.inbox.lock().unwrap().pending.remove(&id);
        result
    }

    /// Queue only `keep` events (all when `None`), and abandon waiting
    /// commands when an `interrupt` event arrives.
    pub fn filter(&self, keep: Option<&[&str]>, interrupt: &[&str]) {
        self.inbox.lock().unwrap().filter(keep, interrupt);
    }

    pub fn drain(&self) -> EventBatch {
        let mut state = self.inbox.lock().unwrap();
        let events = state.events.drain(..).map(|(value, _)| value).collect();
        state.bytes = 0;
        EventBatch {
            events,
            dropped: std::mem::take(&mut state.dropped),
            failure: state.failure.clone(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.socket.shutdown();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn events_survive_before_responses_and_while_no_command_is_running() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let mut connection = Connection::new(WebSocket::from_stream(stream).unwrap()).unwrap();
        let worker = std::thread::spawn(move || {
            let mut header = [0; 2];
            peer.read_exact(&mut header).unwrap();
            let mut payload = vec![0; (header[1] & 127) as usize + 4];
            peer.read_exact(&mut payload).unwrap();
            for text in [
                r#"{"method":"Network.requestWillBeSent"}"#,
                r#"{"id":1,"result":{"ok":true}}"#,
                r#"{"method":"Network.loadingFinished"}"#,
            ] {
                peer.write_all(&[0x81, text.len() as u8]).unwrap();
                peer.write_all(text.as_bytes()).unwrap();
            }
        });
        let result = connection
            .command(
                1,
                &serde_json::json!({"id":1,"method":"test"}),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(result["ok"], true);
        worker.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while events.len() < 2 && Instant::now() < deadline {
            events.extend(connection.drain().events);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["method"], "Network.requestWillBeSent");
        assert_eq!(events[1]["method"], "Network.loadingFinished");
    }

    #[test]
    fn an_interrupting_event_releases_a_waiting_command_and_filters_keep_the_queue_small() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let mut connection = Connection::new(WebSocket::from_stream(stream).unwrap()).unwrap();
        connection.filter(
            Some(&["Page.javascriptDialogOpening"]),
            &["Page.javascriptDialogOpening"],
        );
        let worker = std::thread::spawn(move || {
            let mut header = [0; 2];
            peer.read_exact(&mut header).unwrap();
            let mut payload = vec![0; (header[1] & 127) as usize + 4];
            peer.read_exact(&mut payload).unwrap();
            for text in [
                r#"{"method":"Runtime.consoleAPICalled"}"#,
                r#"{"method":"Page.javascriptDialogOpening","params":{"type":"alert"}}"#,
            ] {
                peer.write_all(&[0x81, text.len() as u8]).unwrap();
                peer.write_all(text.as_bytes()).unwrap();
            }
            // The reply only comes once the dialog is gone; keep the socket open.
            std::thread::sleep(Duration::from_millis(300));
        });
        let started = Instant::now();
        let failure = connection
            .command(
                1,
                &serde_json::json!({"id":1,"method":"Runtime.evaluate"}),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            failure.interrupted_by(),
            Some("Page.javascriptDialogOpening")
        );
        let events = connection.drain().events;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["params"]["type"], "alert");
        worker.join().unwrap();
    }

    #[test]
    fn overflow_is_bounded_and_accounted_for() {
        let mut state = Inbox::default();
        for _ in 0..MAX_EVENTS + 3 {
            state.event(Value::Null, 1);
        }
        assert_eq!(state.events.len(), MAX_EVENTS);
        assert_eq!(state.dropped, 3);
        state.event(Value::Null, MAX_BYTES + 1);
        assert_eq!(state.dropped, 4);
        assert_eq!(state.bytes, MAX_EVENTS);
    }
}
