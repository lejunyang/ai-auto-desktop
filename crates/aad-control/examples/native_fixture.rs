//! Only used inside tests/linux's private D-Bus/Xvfb Qt fixture session.
use aad_control::{
    transport::{Endpoint, RemoteClient},
    *,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn data(response: Response) -> Value {
    assert!(response.error.is_none(), "{response:?}");
    response.data.unwrap()
}
fn request(endpoint: &dyn Endpoint, operation: Operation) -> Value {
    data(endpoint.request(&Request::new(Some(endpoint.session_id().into()), operation)))
}
fn main() {
    let pid = std::env::args()
        .nth(1)
        .expect("fixture PID required")
        .parse::<u64>()
        .unwrap();
    let discovery = Session::native(Policy::default());
    let resources = request(&discovery, Operation::Targets {});
    let resource = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["provider"] == "desktop.linux_atspi"
                && r["metadata"]["process_id"] == pid
                && r["metadata"]["toolkit_name"] == "Qt"
        })
        .expect("the private Qt fixture was not discovered");
    let id = resource["resource_id"].as_str().unwrap();
    let argv = vec![
        std::env::var("AAD_CONTROL_CLI").expect("set AAD_CONTROL_CLI to the test aad binary"),
        "control".into(),
        "serve".into(),
        "--allow-input".into(),
        "--provider".into(),
        "desktop.linux_atspi".into(),
        "--resource".into(),
        id.into(),
    ];
    let endpoint = Arc::new(RemoteClient::spawn(&argv).unwrap());
    let resources = request(endpoint.as_ref(), Operation::Targets {});
    assert_eq!(
        resources["resources"].as_array().unwrap().len(),
        1,
        "server resource grant must filter discovery"
    );
    let observe = || {
        request(
            endpoint.as_ref(),
            Operation::Observe {
                resource_id: id.into(),
                max_nodes: 128,
            },
        )
    };
    // AT-SPI acknowledges dispatch before Qt's event loop necessarily updates
    // the accessible tree. Poll actual observations, never the action reply.
    let observe_until = |expected: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let observation = observe();
            if expected(&observation) {
                break observation;
            }
            assert!(
                Instant::now() < deadline,
                "GUI postcondition timed out: {observation}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let first = observe();
    let reset = request(
        endpoint.as_ref(),
        Operation::Find {
            observation_id: first["observation_id"].as_str().unwrap().into(),
            locator: Locator {
                role: Some("push_button".into()),
                name: Some("Reset Qt fixture status".into()),
                ..Default::default()
            },
        },
    );
    request(
        endpoint.as_ref(),
        Operation::Act {
            target: serde_json::from_value(reset["target"].clone()).unwrap(),
            action: Action::Invoke {},
        },
    );
    let first = observe_until(&|observation| {
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Qt fixture status idle")
    });
    let found = request(
        endpoint.as_ref(),
        Operation::Find {
            observation_id: first["observation_id"].as_str().unwrap().into(),
            locator: Locator {
                role: Some("text".into()),
                name: Some("Qt fixture text entry".into()),
                ..Default::default()
            },
        },
    );
    request(
        endpoint.as_ref(),
        Operation::Act {
            target: serde_json::from_value(found["target"].clone()).unwrap(),
            action: Action::Fill {
                value: "Unified native control".into(),
            },
        },
    );
    let after =
        observe_until(&|observation| {
            observation["nodes"].as_array().unwrap().iter().any(|n| {
                n["name"] == "Qt fixture text entry" && n["value"] == "Unified native control"
            })
        });
    let found = request(
        endpoint.as_ref(),
        Operation::Find {
            observation_id: after["observation_id"].as_str().unwrap().into(),
            locator: Locator {
                role: Some("push_button".into()),
                name: Some("Invoke Qt fixture button".into()),
                ..Default::default()
            },
        },
    );
    request(
        endpoint.as_ref(),
        Operation::Act {
            target: serde_json::from_value(found["target"].clone()).unwrap(),
            action: Action::Invoke {},
        },
    );
    observe_until(&|observation| {
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Qt fixture status invoked")
    });
    request(endpoint.as_ref(), Operation::Close {});
    println!("Unified CLI/native/remote-session fixture passed");
}
