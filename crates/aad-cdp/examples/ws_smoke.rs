//! Probe just the hand-rolled WebSocket against a live browser.
use aad_cdp::Client;
fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9402);
    println!("version: {:?}", Client::version(port));
    match Client::list_targets(port) {
        Ok(targets) => {
            println!("targets={}", targets.len());
            for t in targets.iter().filter(|t| t.kind == "page") {
                println!("page url={} ws_len={}", t.url, t.webSocketDebuggerUrl.len());
            }
            if let Some(target) = targets.iter().find(|t| t.kind == "page") {
                match Client::attach(target) {
                    Ok(mut client) => {
                        println!("ATTACHED");
                        client.ensure_installed().unwrap();
                        let state = client.state().unwrap();
                        println!(
                            "STATE title={:?} text_len={}",
                            state.title,
                            state.text.len()
                        );
                        println!(
                            "HEAD: {}",
                            state.text.lines().take(2).collect::<Vec<_>>().join(" | ")
                        );
                    }
                    Err(e) => println!("ATTACH ERR {e}"),
                }
            }
        }
        Err(e) => println!("LIST ERR {e}"),
    }
}
