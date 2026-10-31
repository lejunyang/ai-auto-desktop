//! Starting an operator-allowlisted program, never through a shell.
//!
//! The server operator names the programs (`--allow-launch`); a client can
//! only pick one of them and pass arguments. The executable is started
//! directly with its arguments as an argv -- no shell, no `start`, no file or
//! URL association -- and programs that are themselves shells, script hosts or
//! association launchers are refused even when allowlisted, because their
//! arguments would bring all of that back.
use crate::{error, Result};
use std::path::{Path, PathBuf};

pub const MAX_ARGS: usize = 32;
pub const MAX_ARG_CHARS: usize = 1024;
pub const MAX_PROGRAM_CHARS: usize = 260;

/// Programs whose arguments are commands, scripts or things to open by
/// association. Compared by file name without extension, case-insensitively.
const INTERPRETERS: &[&str] = &[
    // Windows
    "cmd",
    "command",
    "powershell",
    "powershell_ise",
    "pwsh",
    "wscript",
    "cscript",
    "mshta",
    "rundll32",
    "regsvr32",
    "explorer",
    "wsl",
    "bash",
    "conhost",
    "wt",
    "start",
    "msiexec",
    "control",
    "runas",
    // Unix
    "sh",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "csh",
    "tcsh",
    "env",
    "xdg-open",
    "gio",
    "open",
    "sudo",
    "doas",
    "pkexec",
];

/// Whether the client's `program` is one the operator allowlisted.
/// Exact, case-insensitive: the client names the entry, it does not match it.
pub fn allowed(allowlist: &std::collections::BTreeSet<String>, program: &str) -> bool {
    allowlist
        .iter()
        .any(|entry| entry.eq_ignore_ascii_case(program))
}

/// Check the client's arguments without interpreting them.
pub fn validate_args(program: &str, args: &[String]) -> Result<()> {
    if program.is_empty()
        || program.chars().count() > MAX_PROGRAM_CHARS
        || program.chars().any(char::is_control)
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "program must name an allowlisted executable",
        ));
    }
    if args.len() > MAX_ARGS
        || args
            .iter()
            .any(|arg| arg.chars().count() > MAX_ARG_CHARS || arg.contains('\0'))
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            format!("at most {MAX_ARGS} arguments of at most {MAX_ARG_CHARS} characters, no NUL"),
        ));
    }
    Ok(())
}

fn refused(message: impl Into<String>) -> aad_core::AutomationError {
    error("POLICY.DENIED", message)
}

/// The executable an allowlisted name stands for: an absolute path as given,
/// or a bare name found on `PATH` (with `.exe` added on Windows).
pub fn resolve(program: &str) -> Result<PathBuf> {
    let given = Path::new(program);
    let path = if given.is_absolute() {
        given.to_path_buf()
    } else if given.components().count() == 1 {
        search_path(program).ok_or_else(|| {
            error(
                "CONTROL.LAUNCH_FAILED",
                format!("{program} was not found on PATH"),
            )
        })?
    } else {
        return Err(refused(
            "a relative path is ambiguous; allowlist an absolute path or a bare program name",
        ));
    };
    let metadata = std::fs::metadata(&path).map_err(|_| {
        error(
            "CONTROL.LAUNCH_FAILED",
            format!("{} does not exist", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(error(
            "CONTROL.LAUNCH_FAILED",
            format!("{} is not a file", path.display()),
        ));
    }
    check_executable(&path)?;
    Ok(path)
}

/// Refuse anything that is not a directly executable program, and programs
/// that would interpret their arguments as commands or open them by
/// association.
pub fn check_executable(path: &Path) -> Result<()> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if INTERPRETERS.contains(&stem.as_str()) {
        return Err(refused(format!(
            "{} is a shell, script host or association launcher and is never launched",
            path.display()
        )));
    }
    #[cfg(windows)]
    {
        // Only a PE executable starts without an interpreter: .bat/.cmd go
        // through cmd.exe, and .lnk, .msc, .url and documents through the
        // shell's file associations.
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("exe") {
            return Err(refused(format!(
                "{} is not an .exe; only executables are launched, never by association",
                path.display()
            )));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0);
        if mode & 0o111 == 0 {
            return Err(refused(format!("{} is not executable", path.display())));
        }
    }
    Ok(())
}

fn search_path(name: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = if cfg!(windows) && Path::new(name).extension().is_none() {
        vec![format!("{name}.exe")]
    } else {
        vec![name.to_string()]
    };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|directory| {
            candidates
                .iter()
                .map(|candidate| directory.join(candidate))
                .find(|path| path.is_file())
        })
    })
}

/// Start the program with its arguments as an argv, detached from this
/// server's standard streams (which may carry the control protocol itself).
/// Returns the process id; a background thread reaps the child.
pub fn spawn(path: &Path, args: &[String]) -> Result<u32> {
    let mut command = std::process::Command::new(path);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A console program gets a console of its own instead of this one.
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }
    let mut child = command.spawn().map_err(|cause| {
        error(
            "CONTROL.LAUNCH_FAILED",
            format!("{} could not be started: {cause}", path.display()),
        )
    })?;
    let id = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_allowlist_entry_is_allowed() {
        let list = ["notepad.exe".to_string(), r"C:\Tools\app.exe".to_string()].into();
        assert!(allowed(&list, "NOTEPAD.EXE"));
        assert!(allowed(&list, r"c:\tools\APP.exe"));
        assert!(!allowed(&list, "notepad"));
        assert!(!allowed(&list, r"C:\Tools\app.exe "));
        assert!(!allowed(&Default::default(), "notepad.exe"));
    }

    #[test]
    fn shells_and_association_launchers_are_never_launched() {
        for program in [
            "cmd.exe",
            "PowerShell.exe",
            "pwsh",
            "explorer.exe",
            "rundll32.exe",
            "/bin/sh",
            "xdg-open",
        ] {
            let error = check_executable(Path::new(program)).unwrap_err();
            assert_eq!(error.code, "POLICY.DENIED", "{program}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_executables_are_launched_on_windows() {
        for program in [
            r"C:\x\run.bat",
            r"C:\x\run.CMD",
            r"C:\x\a.lnk",
            r"C:\x\doc.txt",
        ] {
            assert_eq!(
                check_executable(Path::new(program)).unwrap_err().code,
                "POLICY.DENIED",
                "{program}"
            );
        }
        assert!(check_executable(Path::new(r"C:\x\tool.exe")).is_ok());
        // A bare name resolves on PATH, as an .exe.
        let found = resolve("whoami").expect("whoami.exe is on PATH");
        assert!(found.is_absolute());
        assert!(resolve(r"sub\tool.exe").is_err());
    }

    #[test]
    fn arguments_are_bounded_but_not_interpreted() {
        assert!(validate_args("tool", &["--flag".into(), "a b; c | d".into()]).is_ok());
        assert!(validate_args("tool", &vec!["x".into(); MAX_ARGS + 1]).is_err());
        assert!(validate_args("tool", &["a\0b".into()]).is_err());
        assert!(validate_args("", &[]).is_err());
    }
}
