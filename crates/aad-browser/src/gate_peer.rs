//! Local rendezvous for independently started CLI, MCP and recording processes.
//! Only the two IPC handshake nonces and a port go in the private discovery file.
//! The owner receives gate credentials after mutual authentication.
use super::{Gate, Local};
use crate::request_gate::ConfigSource;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
const OFFLINE: &str = "request gate owner unavailable";
static SHARED: AtomicBool = AtomicBool::new(false);
pub fn enable_shared_owner() {
    SHARED.store(true, Ordering::Relaxed);
}
pub(super) fn enabled() -> bool {
    SHARED.load(Ordering::Relaxed)
}
#[derive(Serialize, Deserialize)]
struct Address {
    port: u16,
    client: String,
    server: String,
}
fn send(w: &mut impl Write, v: &impl Serialize) -> io::Result<()> {
    let data = serde_json::to_vec(v)?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(&data)?;
    w.flush()
}
fn read<T: serde::de::DeserializeOwned>(r: &mut impl Read, limit: usize) -> io::Result<T> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut data = vec![0; len];
    r.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}
fn directory(create: bool) -> io::Result<PathBuf> {
    // Both roots belong to the interactive OS user. On Windows inherit the
    // profile directory ACL; on Unix verify private mode before reading a nonce.
    let base = std::env::var_os(if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "HOME"
    })
    .ok_or(io::ErrorKind::NotFound)?;
    let dir = PathBuf::from(base).join(".aad-request-gates");
    if create {
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700).create(&dir)
        };
        #[cfg(not(unix))]
        let created = builder.create(&dir);
        match created {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(dir)
}
fn membership(port: u16) -> io::Result<fs::File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory(true)?.join(format!("{port}.members")))
}

// A caller can keep a future owner alive without starting one or attaching CDP.
pub(super) fn participate(port: u16) -> Result<fs::File, &'static str> {
    let file = membership(port).map_err(|_| OFFLINE)?;
    fs2::FileExt::lock_shared(&file).map_err(|_| OFFLINE)?;
    Ok(file)
}

pub(super) struct Peer(Mutex<TcpStream>);
impl Peer {
    pub fn existing(port: u16) -> Result<Option<Self>, &'static str> {
        let dir = match directory(false) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(OFFLINE),
        };
        let address = match fs::read(dir.join(format!("{port}.json"))) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| OFFLINE)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(OFFLINE),
        };
        let mut stream = match authenticate(&address) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => return Ok(None),
            Err(_) => return Err(OFFLINE),
        };
        send(&mut stream, &Value::Null).map_err(|_| OFFLINE)?;
        if !read::<bool>(&mut stream, 16).map_err(|_| OFFLINE)? {
            return Ok(None);
        }
        Ok(Some(Self(Mutex::new(stream))))
    }
    pub fn join(port: u16, gate: &Gate) -> Result<Self, &'static str> {
        Self::connect(port, gate).map_err(|_| OFFLINE)
    }
    fn connect(port: u16, gate: &Gate) -> io::Result<Self> {
        let dir = directory(true)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{port}.lock")))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let path = dir.join(format!("{port}.json"));
        let existing = fs::read(&path)
            .ok()
            .and_then(|v| serde_json::from_slice::<Address>(&v).ok());
        let mut stream = match existing.as_ref().and_then(|a| authenticate(a).ok()) {
            Some(s) => s,
            None => {
                let mut command = Command::new(std::env::current_exe()?);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x08000000);
                }
                let mut child = command
                    .arg("--aad-request-gate-owner")
                    .arg(port.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                let output = child.stdout.take().unwrap();
                let mut diagnostics = child.stderr.take().unwrap();
                thread::spawn(move || io::copy(&mut diagnostics, &mut io::stderr()));
                thread::spawn(move || child.wait());
                let address: Address = read(&mut BufReader::new(output), 512)?;
                let stream = authenticate(&address)?;
                fs::write(&path, serde_json::to_vec(&address)?)?;
                stream
            }
        };
        send(&mut stream, &gate.credentials())?;
        if !read::<bool>(&mut stream, 16)? {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        Ok(Self(Mutex::new(stream)))
    }
    pub fn call(&self, request: &Value) -> Value {
        let mut stream = self.0.lock().unwrap();
        send(&mut *stream, request)
            .and_then(|_| read(&mut *stream, 8 * 1024 * 1024))
            .unwrap_or(Value::Null)
    }
}
fn authenticate(address: &Address) -> io::Result<TcpStream> {
    let mut s = TcpStream::connect_timeout(
        &([127, 0, 0, 1], address.port).into(),
        Duration::from_secs(1),
    )?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    send(&mut s, &address.client)?;
    if read::<String>(&mut s, 128)? != address.server {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(s)
}
struct State {
    local: Local,
    credentials: (String, String, ConfigSource),
}
fn serve_peer(
    mut s: TcpStream,
    port: u16,
    address: &Address,
    state: &Mutex<Option<State>>,
) -> io::Result<()> {
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    if read::<String>(&mut s, 128)? != address.client {
        return Ok(());
    }
    send(&mut s, &address.server)?;
    let credentials: Option<(String, String, ConfigSource)> = read(&mut s, 64 * 1024)?;
    {
        let mut slot = state.lock().unwrap();
        if let Some(credentials) = credentials {
            let gate = Gate::configured(&credentials.0, credentials.1.clone(), credentials.2)
                .map_err(io::Error::other)?;
            if let Some(owner) = slot.as_mut() {
                if !owner.local.healthy() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                if owner.credentials != credentials {
                    owner.local.update(gate).map_err(io::Error::other)?;
                    owner.credentials = credentials;
                }
            } else {
                *slot = Some(State {
                    local: Local::start(port, gate).map_err(io::Error::other)?,
                    credentials,
                });
            }
        } else if slot.is_none() {
            return send(&mut s, &false);
        }
    }
    send(&mut s, &true)?;
    s.set_read_timeout(None)?;
    loop {
        let request: Value = read(&mut s, 4096)?;
        let reply = {
            let slot = state.lock().unwrap();
            let owner = &slot.as_ref().unwrap().local;
            match request[0].as_str() {
                Some("healthy") => json!(owner.healthy()),
                Some("cursor") => json!(owner.history.lock().unwrap().next),
                Some("window") => json!(owner.window(
                    request[1].as_str().unwrap_or_default(),
                    request[2].as_u64().unwrap_or_default(),
                    request[3].as_u64().unwrap_or_default()
                )),
                _ => return Err(io::ErrorKind::InvalidData.into()),
            }
        };
        send(&mut s, &reply)?;
    }
}
pub fn serve_owner(port: u16) -> io::Result<()> {
    let members = membership(port)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = Arc::new(Address {
        port: listener.local_addr()?.port(),
        client: uuid::Uuid::new_v4().to_string(),
        server: uuid::Uuid::new_v4().to_string(),
    });
    send(&mut io::stdout().lock(), &*address)?;
    listener.set_nonblocking(true)?;
    let peers = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(Mutex::new(None));
    let start = Instant::now();
    let mut used = false;
    loop {
        match listener.accept() {
            Ok((stream, _)) if peers.load(Ordering::Acquire) < 64 => {
                used = true;
                let peers = peers.clone();
                peers.fetch_add(1, Ordering::AcqRel);
                let state = state.clone();
                let address = address.clone();
                thread::spawn(move || {
                    let _ = serve_peer(stream, port, &address, &state);
                    peers.fetch_sub(1, Ordering::Release);
                });
            }
            Err(e) if e.kind() != io::ErrorKind::WouldBlock => return Err(e),
            _ => {}
        }
        if peers.load(Ordering::Acquire) == 0 && (used || start.elapsed() > Duration::from_secs(5))
        {
            // Joining callers take their shared membership before owner discovery.
            // Hold the exclusive lock through cleanup so the next owner starts cleanly.
            match fs2::FileExt::try_lock_exclusive(&members) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => return Err(e),
            }
            drop(listener);
            drop(state.lock().unwrap().take());
            let path = directory(false)?.join(format!("{port}.json"));
            if fs::read(&path)
                .ok()
                .and_then(|v| serde_json::from_slice::<Address>(&v).ok())
                .is_some_and(|a| a.server == address.server)
            {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
