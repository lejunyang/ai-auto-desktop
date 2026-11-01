//! The Linux part of `aad probe`: read-only checks of what desktop
//! automation needs here. Nothing is asked for, injected or captured.
use aad_platform::probe::{Check, State};
use serde_json::json;
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A system command at a fixed place, not searched for on PATH.
pub fn fixed_command(name: &str) -> Option<PathBuf> {
    let roots = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Ok,
    Nonzero,
    Timeout,
    Error,
    NotRun,
}

impl CommandOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Nonzero => "nonzero",
            Self::Timeout => "timeout",
            Self::Error => "error",
            Self::NotRun => "not_run",
        }
    }
}

pub fn run() -> Vec<Check> {
    vec![atspi(), x11(), wayland(), portal(), libei(), uinput()]
}

fn sanitized(command: &Path, args: &[&str], inherited: &[&str]) -> CommandOutcome {
    let mut command = Command::new(command);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in inherited {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return CommandOutcome::Error,
    };
    let stdout = child.stdout.take().expect("probe stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.take(64 * 1024 + 1).read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().unwrap_or_default();
                return if status.success() && !output.is_empty() && output.len() <= 64 * 1024 {
                    CommandOutcome::Ok
                } else {
                    CommandOutcome::Nonzero
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return CommandOutcome::Timeout;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = reader.join();
                return CommandOutcome::Error;
            }
        }
    }
}

fn atspi() -> Check {
    let address = std::env::var_os("AT_SPI_BUS_ADDRESS").is_some();
    let session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let gdbus = fixed_command("gdbus");
    let outcome = if let (Some(command), true) = (gdbus.as_deref(), session) {
        sanitized(
            command,
            &[
                "call",
                "--session",
                "--dest",
                "org.a11y.Bus",
                "--object-path",
                "/org/a11y/bus",
                "--method",
                "org.a11y.Bus.GetAddress",
            ],
            &["DBUS_SESSION_BUS_ADDRESS"],
        )
    } else {
        CommandOutcome::NotRun
    };
    let (state, summary) = match outcome {
        CommandOutcome::Ok => (
            State::Available,
            "The session AT-SPI bus returned an address.",
        ),
        CommandOutcome::Timeout | CommandOutcome::Error if session => (
            State::Unknown,
            "The AT-SPI bus query could not be completed.",
        ),
        _ if address => (
            State::Degraded,
            "An AT-SPI address is advertised but could not be queried.",
        ),
        _ => (State::Unavailable, "No usable AT-SPI bus is advertised."),
    };
    Check::new(
        "linux.at_spi",
        state,
        summary,
        json!({"address_advertised": address, "session_bus_advertised": session, "gdbus_found": gdbus.is_some(), "query": outcome.as_str()}),
    )
}

fn x11() -> Check {
    let display = std::env::var_os("DISPLAY").is_some();
    let xprop = fixed_command("xprop");
    let outcome = if let (Some(command), true) = (xprop.as_deref(), display) {
        sanitized(
            command,
            &["-root", "_NET_SUPPORTING_WM_CHECK"],
            &["DISPLAY", "XAUTHORITY"],
        )
    } else {
        CommandOutcome::NotRun
    };
    let (state, summary) = match outcome {
        CommandOutcome::Ok => (
            State::Available,
            "The advertised X11 display answered a bounded metadata query.",
        ),
        _ if !display => (
            State::Unavailable,
            "No X11 display is advertised to this process.",
        ),
        CommandOutcome::Timeout | CommandOutcome::Error => {
            (State::Unknown, "The X11 query could not be completed.")
        }
        CommandOutcome::NotRun => (
            State::Degraded,
            "An X11 display is advertised, but no trusted query tool was found.",
        ),
        CommandOutcome::Nonzero => (
            State::Degraded,
            "The advertised X11 display did not answer the query.",
        ),
    };
    Check::new(
        "linux.x11",
        state,
        summary,
        json!({"display_advertised": display, "xprop_found": xprop.is_some(), "query": outcome.as_str()}),
    )
}

fn wayland() -> Check {
    let display = std::env::var_os("WAYLAND_DISPLAY");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let endpoint = display.as_ref().map(Path::new).map(|path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            runtime
                .as_ref()
                .map(Path::new)
                .unwrap_or(Path::new(""))
                .join(path)
        }
    });
    let socket = endpoint
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.file_type().is_socket());
    Check::new(
        "linux.wayland",
        if socket {
            State::Available
        } else if display.is_some() {
            State::Degraded
        } else {
            State::Unavailable
        },
        if socket {
            "The advertised Wayland endpoint is a socket."
        } else {
            "No usable Wayland endpoint was confirmed."
        },
        json!({"display_advertised": display.is_some(), "runtime_dir_advertised": runtime.is_some(), "socket_confirmed": socket}),
    )
}

fn portal() -> Check {
    let session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let gdbus = fixed_command("gdbus");
    let outcome = if let (Some(command), true) = (gdbus.as_deref(), session) {
        sanitized(
            command,
            &[
                "call",
                "--session",
                "--dest",
                "org.freedesktop.portal.Desktop",
                "--object-path",
                "/org/freedesktop/portal/desktop",
                "--method",
                "org.freedesktop.DBus.Properties.Get",
                "org.freedesktop.portal.RemoteDesktop",
                "version",
            ],
            &["DBUS_SESSION_BUS_ADDRESS"],
        )
    } else {
        CommandOutcome::NotRun
    };
    let (state, summary) = match outcome {
        CommandOutcome::Ok => (
            State::Available,
            "The RemoteDesktop portal interface is exposed; authorization was not requested.",
        ),
        _ if !session => (
            State::Unavailable,
            "No D-Bus session address is available for the portal.",
        ),
        _ => (
            State::Unknown,
            "The RemoteDesktop portal interface could not be verified.",
        ),
    };
    Check::new(
        "linux.remote_desktop_portal",
        state,
        summary,
        json!({"gdbus_found": gdbus.is_some(), "session_bus_advertised": session, "query": outcome.as_str(), "permission_requested": false, "session_created": false}),
    )
}

fn libei() -> Check {
    let library = fixed_library("ei");
    let oeffis = fixed_library("oeffis");
    let tools = fixed_command("ei-debug-events").is_some() || fixed_command("ei-demo").is_some();
    Check::new(
        "linux.libei",
        if library {
            State::Available
        } else if tools || oeffis {
            State::Degraded
        } else {
            State::Unavailable
        },
        if library {
            "The libei client library is discoverable; no compositor connection was attempted."
        } else if tools || oeffis {
            "libei tooling is present; no compositor connection was attempted."
        } else {
            "No known libei diagnostic command was found."
        },
        json!({"libei_found": library, "liboeffis_found": oeffis, "tooling_found": tools, "connection_attempted": false}),
    )
}

fn uinput() -> Check {
    let path = ["/dev/uinput", "/dev/input/uinput"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists());
    let character = path
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.file_type().is_char_device());
    let writable = path.is_some_and(|path| {
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .ok()
            .is_some_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
    });
    Check::new(
        "linux.uinput",
        if character && writable {
            State::Available
        } else if path.is_some() {
            State::Degraded
        } else {
            State::Unavailable
        },
        if character && writable {
            "A writable uinput character device is present; it was not opened."
        } else {
            "No writable uinput device is exposed to this process."
        },
        json!({"device_present": path.is_some(), "character_device": character, "writable_mode": writable, "libevdev_found": fixed_library("evdev"), "ydotool_found": fixed_command("ydotool").is_some(), "evemu_device_found": fixed_command("evemu-device").is_some(), "device_opened": false}),
    )
}

fn fixed_library(name: &str) -> bool {
    let prefix = format!("lib{name}.so");
    ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .into_iter()
        .map(Path::new)
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            let path = entry.path();
            let direct = entry.file_name().to_string_lossy().starts_with(&prefix);
            direct
                || (path.is_dir()
                    && std::fs::read_dir(path).ok().is_some_and(|children| {
                        children
                            .filter_map(Result::ok)
                            .any(|child| child.file_name().to_string_lossy().starts_with(&prefix))
                    }))
        })
}
