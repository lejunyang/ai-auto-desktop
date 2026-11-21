use super::{AuthorizationIntent, AuthorizedSession, AuthorizedStream, PortalClient};
use aad_platform::{PixelFormat, Raw};
use std::io::{self, BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

/// Pixels from the one monitor selected in a fresh interactive ScreenCast session.
pub struct MonitorCapture {
    session: AuthorizedSession,
    child: Child,
    reader: Option<JoinHandle<()>>,
    latest: Arc<Mutex<Result<Option<Raw>>>>,
    last: Option<Raw>,
    repeat: bool,
}

impl MonitorCapture {
    pub fn start(max_fps: u32, cancel: &AtomicBool) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let (session, monitor, fd) = PortalClient::connect(deadline, cancel)
            .and_then(|p| p.authorize_monitor(AuthorizationIntent::UserRequested, deadline, cancel))
            .map_err(|e| format!("Wayland ScreenCast authorization: {e}"))?;
        let mut child = pipeline(fd, &monitor, max_fps)?;
        let pipe = child.stdout.take().expect("piped GStreamer stdout");
        let latest = Arc::new(Mutex::new(Ok(None)));
        let frames = latest.clone();
        let reader = thread::spawn(move || {
            let mut input = BufReader::new(pipe);
            loop {
                let frame = read_frame(&mut input).map(Some);
                let failed = frame.is_err();
                *frames.lock().unwrap() = frame;
                if failed {
                    break;
                }
            }
        });
        let mut capture = Self {
            session,
            child,
            reader: Some(reader),
            latest,
            last: None,
            repeat: true,
        };
        // A returned FD or a live gst-launch process does not prove that
        // the selected node produces usable video.
        let first_frame = Instant::now() + Duration::from_secs(15);
        loop {
            capture.check()?;
            if cancel.load(Ordering::SeqCst) {
                return Err("Wayland screen sharing cancelled".into());
            }
            if capture
                .latest
                .lock()
                .unwrap()
                .as_ref()
                .is_ok_and(Option::is_some)
            {
                return Ok(capture);
            }
            if Instant::now() >= first_frame {
                return Err("the selected portal monitor did not provide a video frame".into());
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    /// Also checked while no viewer is consuming frames.
    pub fn check(&mut self) -> Result<()> {
        self.session
            .refresh()
            .map_err(|e| format!("Wayland ScreenCast session: {e}"))?;
        if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "GStreamer stopped reading the authorized monitor ({status})"
            ));
        }
        self.latest
            .lock()
            .unwrap()
            .as_ref()
            .map(|_| ())
            .map_err(Clone::clone)
    }

    pub fn refresh(&mut self) {
        self.repeat = true;
    }

    pub fn capture(&mut self) -> Result<Option<Raw>> {
        self.check()?;
        let next = self
            .latest
            .lock()
            .unwrap()
            .as_mut()
            .map_err(|e| e.clone())?
            .take();
        if let Some(frame) = next {
            let changed = self.last.as_ref().is_none_or(|old| {
                old.width != frame.width || old.height != frame.height || old.pixels != frame.pixels
            });
            if changed {
                self.last = Some(frame);
                self.repeat = true;
            }
        }
        self.session.refresh().map_err(|e| e.to_string())?;
        Ok(if std::mem::take(&mut self.repeat) {
            self.last.clone()
        } else {
            None
        })
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.session.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn pipeline(fd: OwnedFd, monitor: &AuthorizedStream, max_fps: u32) -> Result<Child> {
    let mut child = Command::new("gst-launch-1.0");
    // stdin is the connected Unix socket returned by OpenPipeWireRemote;
    // pipewiresrc uses pw_context_connect_fd for every nonnegative FD.
    child
        // PipeWire consults both variables when creating the stream. They
        // can replace an explicitly requested node, including its serial.
        .env_remove("PIPEWIRE_NODE")
        .env_remove("PIPEWIRE_PROPS")
        .stdin(Stdio::from(fd))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .args(["-q", "pipewiresrc", "fd=0"])
        .arg(format!("path={}", monitor.node_id));
    if let Some(serial) = monitor.pipewire_serial {
        child.arg(format!("target-object={serial}"));
    }
    child
        .args([
            "stream-properties=props,node.dont-fallback=(boolean)true",
            "!",
            "videorate",
            "drop-only=true",
        ])
        .arg(format!("max-rate={}", max_fps.clamp(1, 60)))
        .args([
            "!",
            "videoconvert",
            "!",
            "video/x-raw,format=RGB",
            "!",
            "pnmenc",
            "!",
            "fdsink",
            "fd=1",
            "sync=false",
        ]);
    // If assist is killed while waiting for consent or streaming, its
    // subprocess must not keep the authorized socket open.
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        child.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    child
        .spawn()
        .map_err(|e| format!("could not launch GStreamer for the portal monitor: {e}"))
}

fn read_frame(reader: &mut impl BufRead) -> Result<Raw> {
    use std::io::Read;
    let mut header = String::new();
    for _ in 0..3 {
        let len = reader
            .by_ref()
            .take(64)
            .read_line(&mut header)
            .map_err(|e| e.to_string())?;
        if len == 0 || len >= 64 {
            return Err("missing or invalid GStreamer PPM frame".into());
        }
    }
    let mut fields = header.split_ascii_whitespace();
    let invalid = "invalid GStreamer PPM frame dimensions";
    if fields.next() != Some("P6") {
        return Err("expected a binary RGB monitor frame".into());
    }
    let mut dimension = || {
        fields
            .next()?
            .parse::<u32>()
            .ok()
            .filter(|v| (1..=16384).contains(v))
    };
    let width = dimension().ok_or(invalid)?;
    let height = dimension().ok_or(invalid)?;
    let count = width as usize * height as usize;
    if count > 32 * 1024 * 1024 || fields.next() != Some("255") || fields.next().is_some() {
        return Err(invalid.into());
    }
    let mut rgb = vec![0; count * 3];
    reader
        .read_exact(&mut rgb)
        .map_err(|e| format!("incomplete monitor frame: {e}"))?;
    let mut pixels = Vec::with_capacity(count * 4);
    for pixel in rgb.chunks_exact(3) {
        pixels.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
    Ok(Raw {
        width,
        height,
        format: PixelFormat::Rgba,
        pixels: Arc::new(pixels),
        damage: None,
    })
}
