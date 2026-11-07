use aad_control::{
    transport::{self, Endpoint, RemoteClient},
    Policy, SecretBindings, Session,
};
use clap::{Args, Subcommand};
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Args, Default)]
pub struct Grants {
    /// Restrict to these installed providers. Repeat to allow more than one.
    #[arg(long = "provider")]
    providers: Vec<String>,
    /// Restrict to these discovered resource IDs. Repeat to allow more than one.
    #[arg(long = "resource")]
    resources: Vec<String>,
    /// Enable CDP only on this loopback port. Never launches a real browser or copies logins.
    #[arg(long)]
    browser_port: Option<u16>,
    /// Attach Firefox WebDriver BiDi on this loopback port, using its own session.
    #[arg(long, conflicts_with = "browser_port")]
    browser_bidi_port: Option<u16>,
    /// Let desktop_launch start this program: an absolute path or a bare
    /// name on PATH, started directly (never through a shell). Repeat for
    /// more. Shells, script hosts and association launchers are refused.
    #[arg(long = "allow-launch", value_name = "PROGRAM")]
    allow_launch: Vec<String>,
    /// Let clipboard_read return the clipboard's text. Off by default: the
    /// clipboard often holds passwords and private text.
    #[arg(long)]
    allow_clipboard_read: bool,
    /// Let control_global_key press key combinations anchored on no element
    /// (Win+D, Alt+Tab), wherever focus is. Off by default.
    #[arg(long)]
    allow_global_keys: bool,
    /// Let browser uploads read files under this directory. Repeat to allow more than one.
    /// Without it every upload is refused.
    #[arg(long = "allow-upload-dir", value_name = "DIR")]
    upload_dirs: Vec<std::path::PathBuf>,
    /// Let browser_storage show cookie values. Without it names and flags only.
    #[arg(long)]
    allow_cookie_values: bool,
    /// Let browser control open regular files under this directory. Repeatable.
    /// This controls document access; it is not a browser filesystem sandbox.
    #[arg(long = "allow-browser-file-dir", value_name = "DIR")]
    browser_file_dirs: Vec<std::path::PathBuf>,
    /// Allow one internal origin, e.g. chrome://settings. Repeatable; no wildcards.
    #[arg(long = "allow-browser-internal-origin", value_name = "ORIGIN")]
    browser_internal_origins: Vec<String>,
    /// Permit replacing protected fields with server-owned secret bindings.
    /// Also requires --allow-input. Ordinary fill/type/paste stay prohibited.
    #[arg(long)]
    allow_secret_input: bool,
    /// Bind a public name to a server environment variable. Only the name is
    /// exposed to control clients; never put the secret value on the command line.
    #[arg(long = "secret-env", value_name = "NAME=ENV")]
    secret_env: Vec<String>,
    /// Populated once by the synchronous startup path, before any child/thread.
    #[arg(skip)]
    prepared_bindings: Option<SecretBindings>,
}
impl Grants {
    fn policy(&self, allow_input: bool) -> std::io::Result<Policy> {
        self.check(allow_input)?;
        Ok(Policy {
            allow_input,
            allow_secret_input: self.allow_secret_input,
            secret_bindings: if self.secret_env.is_empty() {
                SecretBindings::default()
            } else {
                self.prepared_bindings
                    .clone()
                    .ok_or_else(|| invalid("secret bindings must be prepared during startup"))?
            },
            providers: self.providers.iter().cloned().collect(),
            resources: if self.resources.is_empty() {
                None
            } else {
                Some(self.resources.iter().cloned().collect::<BTreeSet<_>>())
            },
            launch: self.allow_launch.iter().cloned().collect(),
            allow_clipboard_read: self.allow_clipboard_read,
            allow_global_keys: self.allow_global_keys,
            upload_roots: self.upload_dirs.clone(),
            allow_cookie_values: self.allow_cookie_values,
            browser_access: aad_control::browser_access::BrowserAccess::new(
                &self.browser_file_dirs,
                &self.browser_internal_origins,
            )
            .map_err(|error| invalid(error.message))?,
        })
    }
    fn prepare_secrets(&mut self, allow_input: bool) -> std::io::Result<Vec<String>> {
        self.check(allow_input)?;
        // Read every binding before the caller removes any environment key:
        // several public names may intentionally use the same source variable.
        let bindings =
            bindings_from_environment(&self.secret_env, |name| std::env::var(name).ok())?;
        let variables = self
            .secret_env
            .iter()
            .map(|binding| {
                binding
                    .split_once('=')
                    .expect("bindings validated")
                    .1
                    .to_string()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.prepared_bindings = Some(bindings);
        Ok(variables)
    }
    /// Launching and global keys are input: refuse them without the input grant
    /// rather than start a server that would refuse every such request.
    fn check(&self, allow_input: bool) -> std::io::Result<()> {
        if self.browser_bidi_port == Some(0)
            || (self.browser_bidi_port.is_some() && self.browser_port.is_some())
        {
            return Err(invalid(
                "--browser-bidi-port must be nonzero and cannot be combined with --browser-port",
            ));
        }
        if !allow_input
            && (!self.allow_launch.is_empty() || self.allow_global_keys || self.allow_secret_input)
        {
            return Err(invalid(
                "--allow-launch, --allow-global-keys and --allow-secret-input need --allow-input",
            ));
        }
        if !self.allow_secret_input && !self.secret_env.is_empty() {
            return Err(invalid("--secret-env needs --allow-secret-input"));
        }
        Ok(())
    }
    /// Whether any grant was given that only the remote server may hold.
    fn any(&self) -> bool {
        !self.providers.is_empty()
            || !self.resources.is_empty()
            || self.browser_port.is_some()
            || self.browser_bidi_port.is_some()
            || !self.allow_launch.is_empty()
            || self.allow_clipboard_read
            || self.allow_global_keys
            || !self.upload_dirs.is_empty()
            || self.allow_cookie_values
            || !self.browser_file_dirs.is_empty()
            || !self.browser_internal_origins.is_empty()
            || self.allow_secret_input
            || !self.secret_env.is_empty()
    }
}

fn bindings_from_environment(
    bindings: &[String],
    mut read: impl FnMut(&str) -> Option<String>,
) -> std::io::Result<SecretBindings> {
    let mut specifications = Vec::new();
    let mut names = BTreeSet::new();
    for binding in bindings {
        let (name, variable) = binding
            .split_once('=')
            .ok_or_else(|| invalid("secret binding must be NAME=ENV"))?;
        if !aad_control::valid_secret_ref(name)
            || variable.is_empty()
            || variable.len() > 128
            || !variable
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || variable.as_bytes()[0].is_ascii_digit()
        {
            return Err(invalid(
                "invalid secret binding name or environment variable",
            ));
        }
        if !names.insert(name) {
            return Err(invalid("invalid or duplicate secret binding"));
        }
        specifications.push((name, variable));
    }
    let mut pairs = Vec::new();
    for (name, variable) in specifications {
        let value = read(variable).ok_or_else(|| {
            invalid("secret binding environment variable is absent or not Unicode")
        })?;
        pairs.push((name.to_string(), value));
    }
    SecretBindings::new(pairs).map_err(|_| invalid("invalid or duplicate secret binding"))
}

#[derive(Args, Default)]
pub struct McpArgs {
    #[command(flatten)]
    grants: Grants,
    /// Explicit grant for control_act, save_workflow and run_workflow.
    /// Without it the server only observes.
    #[arg(long)]
    allow_input: bool,
    /// Let browser_open start from a copy of an existing browser profile's logins.
    #[arg(long, requires = "allow_input")]
    allow_profile_copy: bool,
    /// Trusted argv as a JSON array, e.g. an existing authenticated ssh -T command.
    /// This connection exposes only remote control tools, no local workflow tools.
    #[arg(long, value_name = "JSON_ARGV")]
    connect: Option<String>,
}

impl McpArgs {
    pub fn prepare_secrets(&mut self) -> std::io::Result<Vec<String>> {
        if self.connect.is_some() {
            if self.allow_input || self.grants.any() {
                return Err(invalid("remote grants must be configured on the remote server; local flags cannot replace them"));
            }
            Ok(Vec::new())
        } else {
            self.grants.prepare_secrets(self.allow_input)
        }
    }
}

#[derive(Subcommand)]
pub enum ControlCommand {
    /// Read-only persistent browser diagnostics over NDJSON.
    /// Send targets, then browser_network with resource_id and limit (1..500).
    Diagnostics {
        #[arg(long)]
        browser_port: u16,
    },
    /// Serve one private NDJSON session on inherited authenticated stdin/stdout.
    /// Use SSH for a remote machine; this command has no network listener.
    Serve(ServeArgs),
    /// Proxy a persistent session through trusted operator-supplied argv.
    /// Missing request session IDs are bound to this connection by the client.
    Connect(ConnectArgs),
}

impl ControlCommand {
    pub fn prepare_secrets(&mut self) -> std::io::Result<Vec<String>> {
        match self {
            Self::Serve(args) => args.grants.prepare_secrets(args.allow_input),
            _ => Ok(Vec::new()),
        }
    }
}

#[derive(Args)]
pub struct ServeArgs {
    #[command(flatten)]
    grants: Grants,
    /// Explicit server-side grant. Remote sessions default to read-only.
    #[arg(long)]
    allow_input: bool,
}

#[derive(Args)]
pub struct ConnectArgs {
    /// JSON argv. The executable is started directly, without a local shell.
    #[arg(long, value_name = "JSON_ARGV")]
    command: String,
}

fn command(raw: &str) -> std::io::Result<Vec<String>> {
    let argv: Vec<String> = serde_json::from_str(raw).map_err(invalid)?;
    if argv.is_empty() || argv.len() > 64 || argv.iter().any(|s| s.len() > 8192 || s.contains('\0'))
    {
        return Err(invalid("command must be a bounded non-empty argv"));
    }
    Ok(argv)
}
fn invalid(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
}

pub fn mcp(args: &McpArgs) -> std::io::Result<()> {
    let server = if let Some(raw) = &args.connect {
        if args.allow_input || args.grants.any() {
            return Err(invalid("remote grants must be configured on the remote server; local flags cannot replace them"));
        }
        let client = RemoteClient::spawn(&command(raw)?).map_err(|e| invalid(e.message))?;
        aad_mcp::Server::with_control(Arc::new(client))
    } else {
        args.grants.check(args.allow_input)?;
        let server = aad_mcp::Server::local(Session::native_with_browsers(
            args.grants.policy(args.allow_input)?,
            args.grants.browser_port,
            args.grants.browser_bidi_port,
        ));
        if args.allow_profile_copy {
            server.allow_profile_copy()
        } else {
            server
        }
    };
    aad_mcp::serve_stdio_with(server)
}

pub fn run(command_args: &ControlCommand) -> std::io::Result<()> {
    let endpoint: Arc<dyn Endpoint> = match command_args {
        ControlCommand::Diagnostics { browser_port } => Arc::new(Session::native_with_browser(
            Default::default(),
            Some(*browser_port),
        )),
        ControlCommand::Serve(args) => {
            args.grants.check(args.allow_input)?;
            Arc::new(Session::native_with_browsers(
                args.grants.policy(args.allow_input)?,
                args.grants.browser_port,
                args.grants.browser_bidi_port,
            ))
        }
        ControlCommand::Connect(args) => {
            Arc::new(RemoteClient::spawn(&command(&args.command)?).map_err(|e| invalid(e.message))?)
        }
    };
    transport::serve(std::io::stdin(), std::io::stdout(), endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn firefox_attachment_is_explicit_and_distinct_from_cdp() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            grants: Grants,
        }
        use clap::Parser;
        let cli = Cli::try_parse_from(["aad", "--browser-bidi-port", "9223"]).unwrap();
        assert_eq!(cli.grants.browser_bidi_port, Some(9223));
        assert!(cli.grants.any());
        assert!(cli.grants.check(false).is_ok());
        assert!(Cli::try_parse_from([
            "aad",
            "--browser-bidi-port",
            "9223",
            "--browser-port",
            "9222"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["aad", "--browser-bidi-port", "0"])
            .unwrap()
            .grants
            .check(false)
            .is_err());
    }
    #[test]
    fn input_only_grants_need_the_input_grant() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            grants: Grants,
        }
        use clap::Parser;
        let launch = Cli::try_parse_from(["aad", "--allow-launch", "notepad.exe"]).unwrap();
        assert!(launch.grants.check(false).is_err());
        assert!(launch.grants.check(true).is_ok());
        assert_eq!(launch.grants.policy(true).unwrap().launch.len(), 1);
        let keys = Cli::try_parse_from(["aad", "--allow-global-keys"]).unwrap();
        assert!(keys.grants.check(false).is_err());
        let clipboard = Cli::try_parse_from(["aad", "--allow-clipboard-read"]).unwrap();
        assert!(clipboard.grants.check(false).is_ok());
        assert!(clipboard.grants.policy(false).unwrap().allow_clipboard_read);
    }
    #[test]
    fn uploads_are_refused_unless_the_operator_names_directories() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            grants: Grants,
        }
        use clap::Parser;
        let none = Cli::try_parse_from(["aad"]).unwrap();
        assert!(none.grants.policy(true).unwrap().upload_roots.is_empty());
        let two = Cli::try_parse_from([
            "aad",
            "--allow-upload-dir",
            "C:/uploads",
            "--allow-upload-dir",
            "D:/more",
        ])
        .unwrap();
        assert_eq!(
            two.grants.policy(true).unwrap().upload_roots,
            vec![
                std::path::PathBuf::from("C:/uploads"),
                std::path::PathBuf::from("D:/more")
            ]
        );
    }
    #[test]
    fn cookie_values_stay_redacted_unless_the_operator_allows_them() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            grants: Grants,
        }
        use clap::Parser;
        assert!(
            !Cli::try_parse_from(["aad"])
                .unwrap()
                .grants
                .policy(false)
                .unwrap()
                .allow_cookie_values
        );
        assert!(
            Cli::try_parse_from(["aad", "--allow-cookie-values"])
                .unwrap()
                .grants
                .policy(false)
                .unwrap()
                .allow_cookie_values
        );
    }
    #[test]
    fn browser_document_grants_are_server_owned_and_validate_at_startup() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            grants: Grants,
        }
        use clap::Parser;
        let granted = Cli::try_parse_from([
            "aad",
            "--allow-browser-internal-origin",
            "chrome://settings",
            "--allow-browser-internal-origin",
            "edge://downloads",
        ])
        .unwrap();
        assert!(
            granted.grants.any(),
            "remote clients must not silently accept local grants"
        );
        let policy = granted.grants.policy(false).unwrap();
        assert!(
            !policy.allow_input,
            "read grants must not imply input permission"
        );
        assert!(policy
            .browser_access
            .check_document("chrome://settings/appearance")
            .is_ok());
        assert!(policy
            .browser_access
            .check_document("chrome://extensions/")
            .is_err());
        let bad =
            Cli::try_parse_from(["aad", "--allow-browser-internal-origin", "chrome://*"]).unwrap();
        assert!(bad.grants.policy(true).is_err());
        let bad = Cli::try_parse_from([
            "aad",
            "--allow-browser-file-dir",
            "/aad-nonexistent-fixture-directory",
        ])
        .unwrap();
        assert!(bad.grants.policy(true).is_err());
    }
    #[test]
    fn transport_commands_are_argv_not_shell_fragments() {
        assert!(command("[]").is_err());
        assert!(command("ssh host aad").is_err());
        assert_eq!(
            command(r#"["ssh","-T","example","aad control serve"]"#).unwrap(),
            vec!["ssh", "-T", "example", "aad control serve"]
        );
        assert!(command(r#"["bad\u0000program"]"#).is_err());
    }

    #[test]
    fn secret_bindings_require_both_grants_and_never_put_values_in_debug_or_errors() {
        let mut grants = Grants {
            secret_env: vec!["login=AAD_TEST_SECRET".into()],
            ..Default::default()
        };
        assert!(grants.check(true).is_err());
        grants.allow_secret_input = true;
        assert!(grants.check(false).is_err());
        assert!(grants.check(true).is_ok());
        assert!(grants.any());
        let bindings = bindings_from_environment(&grants.secret_env, |variable| {
            assert_eq!(variable, "AAD_TEST_SECRET");
            Some("test-only-password".into())
        })
        .unwrap();
        assert_eq!(bindings.names(), ["login"]);
        assert!(!format!("{bindings:?}").contains("test-only-password"));
        assert!(
            grants.policy(true).is_err(),
            "policy must not read environment lazily"
        );
        grants.prepared_bindings = Some(bindings);
        assert_eq!(
            grants.policy(true).unwrap().secret_bindings.names(),
            ["login"]
        );
        for names in [vec!["login=$(echo bad)".into()], vec!["login".into()]] {
            assert!(
                bindings_from_environment(&names, |_| panic!("must validate before reading"))
                    .is_err()
            );
        }
        assert!(bindings_from_environment(
            &["first=AAD_TEST_SECRET".into(), "malformed".into()],
            |_| panic!("validate every specification before reading any value")
        )
        .is_err());
        assert!(bindings_from_environment(&grants.secret_env, |_| None).is_err());
    }
}
