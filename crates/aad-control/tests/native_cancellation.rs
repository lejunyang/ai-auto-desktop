use aad_control::{
    native::{Flavor, NativeBackend},
    Action, Backend, Context, Result,
};
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::{CancellationToken, Provider};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

struct CooperativeProvider {
    manifest: CapabilityManifest,
    started: Mutex<mpsc::Sender<()>>,
    owned: AtomicBool,
    released: AtomicBool,
}

impl Provider for CooperativeProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(&self, _: &str, _: Value, _: Option<Duration>) -> Result<Value> {
        panic!("native adapter must preserve the cancellation-aware override")
    }
    fn invoke_cancellable(
        &self,
        _: &str,
        args: Value,
        _: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        cancellation.check()?;
        assert_eq!(args["target"]["node_id"], "n");
        self.owned.store(true, Ordering::SeqCst);
        self.started.lock().unwrap().send(()).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while !cancellation.is_cancelled() && Instant::now() < limit {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.owned.store(false, Ordering::SeqCst);
        self.released.store(true, Ordering::SeqCst);
        cancellation
            .check()
            .map_err(|error| error.with_effect("unknown"))?;
        panic!("native cancellation never reached the provider")
    }
}

#[test]
fn committed_native_input_and_workflow_wrapper_forward_the_same_cancellation() {
    for workflow in [false, true] {
        let (tx, rx) = mpsc::channel();
        let provider = Arc::new(CooperativeProvider {
            manifest: manifest::parse(&manifest::document(
                "desktop.windows_uia",
                serde_json::Map::from_iter([
                    ("pointer_gesture".into(), json!({"contract_major":1})),
                    ("secret_input".into(), json!({"contract_major":1})),
                ]),
            ))
            .unwrap(),
            started: Mutex::new(tx),
            owned: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        let backend = NativeBackend::new(provider.clone(), Flavor::Windows);
        let context = Arc::new(Context::new(Duration::from_secs(5), true));
        let running = context.clone();
        let worker = std::thread::spawn(move || {
            let node = json!({"node_id":"n","role":"pane","actions":["pointer_gesture"]});
            let snapshot = json!({"snapshot_id":"s","revision":1,"nodes":[node]});
            if workflow {
                let provider = backend.provider();
                let denied = provider
                    .invoke_cancellable(
                        "desktop.windows_uia.secret_input@1",
                        json!({}),
                        None,
                        running.cancellation(),
                    )
                    .unwrap_err();
                assert_eq!(denied.code, "CONTROL.SECRET_WORKFLOW_DENIED");
                provider.invoke_cancellable(
                    "desktop.windows_uia.pointer_gesture@1",
                    json!({"target":{"node_id":"n"}}),
                    Some(running.commit().unwrap()),
                    running.cancellation(),
                )
            } else {
                let action: Action =
                    serde_json::from_value(json!({"type":"click","hold_ms":5000})).unwrap();
                backend.act(&snapshot, &node, &action, &running)
            }
        });
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(provider.owned.load(Ordering::SeqCst));
        context.cancel();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.code, "DRIVER.CANCELLED");
        assert_eq!(error.effect, "unknown");
        assert!(!provider.owned.load(Ordering::SeqCst));
        assert!(provider.released.load(Ordering::SeqCst));
        assert_eq!(context.remaining().unwrap_err().effect, "unknown");
    }
}
