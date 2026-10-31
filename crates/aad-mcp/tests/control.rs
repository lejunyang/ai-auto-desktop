use aad_control::*;
use aad_mcp::{Request as RpcRequest, Server};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

struct Fake {
    name: &'static str,
    calls: AtomicUsize,
}
impl Backend for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "fixture:one".into(),
                provider: self.name.into(),
                kind: "desktop".into(),
                name: "test".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"nodes":[{"node_id":"n","name":"Save","role":"button"}],"truncated":false}))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(80));
        Ok(json!({"submitted":true}))
    }
}
fn rpc(id: u64, name: &str, args: Value) -> RpcRequest {
    RpcRequest {
        id: Some(json!(id)),
        method: "tools/call".into(),
        params: json!({"name":name,"arguments":args}),
    }
}
fn payload(reply: Value) -> Value {
    assert!(reply.get("error").is_none(), "{reply}");
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn server(name: &'static str, input: bool) -> (Arc<Server>, Arc<Fake>) {
    let fake = Arc::new(Fake {
        name,
        calls: AtomicUsize::new(0),
    });
    let session = Session::new(
        vec![fake.clone()],
        Policy {
            allow_input: input,
            ..Default::default()
        },
    );
    (Arc::new(Server::with_control(Arc::new(session))), fake)
}
fn target(server: &Server) -> Value {
    payload(
        server
            .handle(&rpc(1, "control_targets", json!({})))
            .unwrap(),
    );
    let observed = payload(
        server
            .handle(&rpc(
                2,
                "control_observe",
                json!({"resource_id":"fixture:one"}),
            ))
            .unwrap(),
    );
    observed["data"]["nodes"][0]["target"].clone()
}

#[test]
fn all_platform_providers_route_through_the_same_tools_without_a_uia_driver() {
    for provider in [
        "desktop.windows_uia",
        "desktop.linux_atspi",
        "desktop.macos_ax",
        "browser.cdp",
    ] {
        let (server, fake) = server(provider, true);
        let target = target(&server);
        let reply = payload(
            server
                .handle(&rpc(
                    3,
                    "control_act",
                    json!({"target":target,"action":{"type":"click"}}),
                ))
                .unwrap(),
        );
        assert_eq!(reply["data"]["provider"], provider);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let denied = server
            .handle(&rpc(4, "invoke", json!({"target":"legacy:bypass:n"})))
            .unwrap();
        assert_eq!(denied["error"]["code"], -32602);
    }
}
#[test]
fn grants_unknown_fields_and_coordinates_cannot_be_bypassed() {
    let (server, fake) = server("desktop.linux_atspi", false);
    let target = target(&server);
    let denied = payload(
        server
            .handle(&rpc(
                3,
                "control_act",
                json!({"target":target,"action":{"type":"click"}}),
            ))
            .unwrap(),
    );
    assert_eq!(denied["error"]["code"], "POLICY.DENIED");
    let bad = server
        .handle(&rpc(
            4,
            "control_act",
            json!({"target":target,"action":{"type":"click","x":1,"y":2}}),
        ))
        .unwrap();
    assert_eq!(bad["error"]["code"], -32602);
    let bad = server
        .handle(&rpc(5, "control_capabilities", json!({"allow_input":true})))
        .unwrap();
    assert_eq!(bad["error"]["code"], -32602);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn cancellation_notification_reaches_an_in_flight_control_request() {
    let (server, fake) = server("browser.cdp", true);
    let target = target(&server);
    let running = server.clone();
    let worker = std::thread::spawn(move || {
        payload(
            running
                .handle(&rpc(
                    9,
                    "control_act",
                    json!({"target":target,"action":{"type":"click"}}),
                ))
                .unwrap(),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while fake.calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(server
        .handle(&RpcRequest {
            id: None,
            method: "notifications/cancelled".into(),
            params: json!({"requestId":9,"reason":"test"})
        })
        .is_none());
    let response = worker.join().unwrap();
    assert_eq!(response["effect"], "unknown");
    assert_eq!(response["error"]["code"], "CONTROL.CANCELLED");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn a_remote_connection_advertises_no_local_file_or_legacy_execution_tools() {
    let (server, _) = server("browser.cdp", false);
    let listed = server
        .handle(&RpcRequest {
            id: Some(json!(1)),
            method: "tools/list".into(),
            params: json!({}),
        })
        .unwrap();
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| aad_mcp::control_tools::method(tool["name"].as_str().unwrap()).is_some()));
    assert_eq!(
        server.handle(&rpc(2, "save_workflow", json!({}))).unwrap()["error"]["code"],
        -32602
    );
}
