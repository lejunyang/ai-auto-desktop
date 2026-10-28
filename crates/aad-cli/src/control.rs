use aad_control::{
    transport::{self, Endpoint, RemoteClient},
    Policy, Session,
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
}
impl Grants {
    fn policy(&self, allow_input: bool) -> Policy {
        Policy {
            allow_input,
            providers: self.providers.iter().cloned().collect(),
            resources: if self.resources.is_empty() {
                None
            } else {
                Some(self.resources.iter().cloned().collect::<BTreeSet<_>>())
            },
        }
    }
}

#[derive(Args, Default)]
pub struct McpArgs {
    #[command(flatten)]
    grants: Grants,
    /// Explicit grant for control_act, save_workflow and run_workflow.
    /// Without it the server only observes.
    #[arg(long)]
    allow_input: bool,
    /// Trusted argv as a JSON array, e.g. an existing authenticated ssh -T command.
    /// This connection exposes only remote control tools, no local workflow tools.
    #[arg(long, value_name = "JSON_ARGV")]
    connect: Option<String>,
}

#[derive(Subcommand)]
pub enum ControlCommand {
    /// Serve one private NDJSON session on inherited authenticated stdin/stdout.
    /// Use SSH for a remote machine; this command has no network listener.
    Serve(ServeArgs),
    /// Proxy a persistent session through trusted operator-supplied argv.
    /// Missing request session IDs are bound to this connection by the client.
    Connect(ConnectArgs),
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
        if args.allow_input
            || !args.grants.providers.is_empty()
            || !args.grants.resources.is_empty()
            || args.grants.browser_port.is_some()
        {
            return Err(invalid("remote grants must be configured on the remote server; local flags cannot replace them"));
        }
        let client = RemoteClient::spawn(&command(raw)?).map_err(|e| invalid(e.message))?;
        aad_mcp::Server::with_control(Arc::new(client))
    } else {
        aad_mcp::Server::with_options(
            args.grants.policy(args.allow_input),
            args.grants.browser_port,
        )
    };
    aad_mcp::serve_stdio_with(server)
}

pub fn run(command_args: &ControlCommand) -> std::io::Result<()> {
    let endpoint: Arc<dyn Endpoint> = match command_args {
        ControlCommand::Serve(args) => Arc::new(Session::native_with_browser(
            args.grants.policy(args.allow_input),
            args.grants.browser_port,
        )),
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
    fn transport_commands_are_argv_not_shell_fragments() {
        assert!(command("[]").is_err());
        assert!(command("ssh host aad").is_err());
        assert_eq!(
            command(r#"["ssh","-T","example","aad control serve"]"#).unwrap(),
            vec!["ssh", "-T", "example", "aad control serve"]
        );
        assert!(command(r#"["bad\u0000program"]"#).is_err());
    }
}
