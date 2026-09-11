use aad_core::compile_descriptor;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::{run, ArtifactStore, Provider, ProviderRegistry, RunOptions, RunStatus};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n1\t1\t0\t0\t0\t0\t0\t0\t200\t80\t-1\t\n5\t1\t1\t1\t1\t1\t10\t20\t40\t12\t96.0\tInvoice\n5\t1\t1\t1\t1\t2\t55\t20\t50\t12\t84.0\tA-42\n";

struct Fixture {
    root: PathBuf,
    engine: PathBuf,
    image: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "aad-ocr-integration-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let engine = root.join(if cfg!(windows) { "fake.cmd" } else { "fake" });
        #[cfg(unix)]
        {
            std::fs::write(
                &engine,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tesseract 5.4.1'; exit 0; fi\nprintf '%b' '{}'\n",
                    TSV.replace('\n', "\\n")
                        .replace('\t', "\\t")
                        .replace('\'', "'\\''")
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let image = root.join("input.png");
        image::DynamicImage::new_rgb8(200, 80)
            .save_with_format(&image, image::ImageFormat::Png)
            .unwrap();
        Self {
            root,
            engine,
            image,
        }
    }
}

#[cfg(target_os = "linux")]
fn invoke_fixture(
    tsv: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, aad_core::AutomationError> {
    let root = std::env::temp_dir().join(format!(
        "aad-ocr-integration-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let engine = root.join("fake");
    std::fs::write(
        &engine,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tesseract 5.4.1'; exit 0; fi\nprintf '%b' '{}'\n",
            tsv.replace('\n', "\\n")
                .replace('\t', "\\t")
                .replace('\'', "'\\''")
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o700)).unwrap();
    let image = root.join("input.png");
    image::DynamicImage::new_rgb8(200, 80)
        .save_with_format(&image, image::ImageFormat::Png)
        .unwrap();
    let provider =
        aad_ocr::OcrProvider::with_command(vec![engine.display().to_string()], true).unwrap();
    let mut args = args.as_object().unwrap().clone();
    args.insert("image".into(), json!({"path": image}));
    let result = provider.invoke(
        "vision.ocr.recognize@1",
        json!(args),
        Some(Duration::from_secs(5)),
    );
    let _ = std::fs::remove_dir_all(root);
    result
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct CaptureProvider {
    manifest: CapabilityManifest,
    bytes: Vec<u8>,
}

impl CaptureProvider {
    fn new(bytes: Vec<u8>) -> Self {
        let raw = json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "CapabilityManifest",
            "metadata": {"name": "fixture.capture", "version": "1.0.0"},
            "actions": {
                "capture": {
                    "contract_major": 1,
                    "effect": {"default_class": "read_only"},
                    "risk": {"category": "observe", "level": "low"},
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "artifacts": {"outputs": {"frame": {
                        "pointer": "/frame", "media_types": ["image/png"],
                        "max_size_bytes": 67108864
                    }}}
                }
            }
        });
        Self {
            manifest: manifest::parse(&raw).unwrap(),
            bytes,
        }
    }
}

impl Provider for CaptureProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        _action: &str,
        _args: serde_json::Value,
        _timeout: Option<Duration>,
    ) -> Result<serde_json::Value, aad_core::AutomationError> {
        unreachable!("artifact actions use invoke_with_artifacts")
    }

    fn invoke_with_artifacts(
        &self,
        _action: &str,
        _args: serde_json::Value,
        _timeout: Option<Duration>,
        artifacts: &ArtifactStore,
    ) -> Result<serde_json::Value, aad_core::AutomationError> {
        let reference = artifacts.import_bytes(&self.bytes, Some("image/png"))?;
        Ok(json!({"frame": reference.to_value()}))
    }
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_path_and_artifact_actions_are_equivalent() {
    let fixture = Fixture::new();
    let store = Arc::new(ArtifactStore::default());
    let provider =
        aad_ocr::OcrProvider::with_command(vec![fixture.engine.display().to_string()], true)
            .unwrap();
    let common = json!({
        "languages": ["eng"],
        "patterns": [{"id": "invoice", "value": "A-42"}],
    });
    let mut path_args = common.as_object().unwrap().clone();
    path_args.insert("image".into(), json!({"path": fixture.image}));
    let path_result = provider
        .invoke(
            "vision.ocr.recognize@1",
            json!(path_args),
            Some(Duration::from_secs(5)),
        )
        .unwrap();

    let reference = store
        .import_bytes(std::fs::read(&fixture.image).unwrap(), Some("image/png"))
        .unwrap();
    let mut artifact_args = common.as_object().unwrap().clone();
    artifact_args.insert("artifact".into(), reference.to_value());
    let artifact_result = provider
        .invoke_with_artifacts(
            "vision.ocr.recognize_artifact@1",
            json!(artifact_args),
            Some(Duration::from_secs(5)),
            &store,
        )
        .unwrap();

    assert_eq!(path_result["text"], artifact_result["text"]);
    assert_eq!(path_result["matches"], artifact_result["matches"]);
    assert!(path_result["source"].get("path").is_some());
    assert!(artifact_result["source"].get("path").is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn runtime_keeps_artifacts_private_across_native_providers() {
    let fixture = Fixture::new();
    let bytes = std::fs::read(&fixture.image).unwrap();
    let artifacts = Arc::new(ArtifactStore::default());
    let mut providers = ProviderRegistry::new();
    providers.insert(Arc::new(CaptureProvider::new(bytes)));
    providers.insert(Arc::new(
        aad_ocr::OcrProvider::with_command(vec![fixture.engine.display().to_string()], true)
            .unwrap(),
    ));
    let descriptor = compile_descriptor(
        json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "native-artifact-ocr"},
            "budgets": {"max_duration": "10s", "max_executed_steps": 2},
            "outputs": {"text": {"value": "${{ steps.ocr.output.text }}"}},
            "steps": [
                {"id": "capture", "type": "action", "uses": "fixture.capture.capture@1", "with": {}},
                {"id": "ocr", "type": "action", "uses": "vision.ocr.recognize_artifact@1",
                 "with": {"artifact": "${{ steps.capture.output.frame }}"}, "timeout": "5s"}
            ]
        }),
        None,
    )
    .unwrap();

    let result = run(
        &descriptor,
        RunOptions::default()
            .with_providers(providers)
            .with_artifacts(artifacts),
    );

    assert_eq!(result.status, RunStatus::Succeeded, "{:?}", result.error);
    assert_eq!(result.outputs["text"], "Invoice A-42");
}

#[cfg(target_os = "linux")]
#[test]
fn provider_reports_no_text_and_low_confidence() {
    let header = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n";
    assert_eq!(
        invoke_fixture(header, json!({})).unwrap_err().code,
        "OCR.NO_TEXT"
    );
    assert_eq!(
        invoke_fixture(TSV, json!({"minimum_confidence": 0.99}))
            .unwrap_err()
            .code,
        "OCR.LOW_CONFIDENCE"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn region_coordinates_and_bounds_remain_in_source_space() {
    let result = invoke_fixture(
        TSV,
        json!({
            "region": {"x": 5, "y": 7, "width": 150, "height": 60},
            "patterns": [{"id": "invoice", "value": "Invoice"}]
        }),
    )
    .unwrap();
    assert_eq!(
        result["source_region"],
        json!({"x": 5, "y": 7, "width": 150, "height": 60})
    );
    assert_eq!(
        result["lines"][0]["bounds"],
        json!({"x": 15, "y": 27, "width": 95, "height": 12})
    );
    assert_eq!(
        result["matches"][0]["bounds"],
        json!({"x": 15, "y": 27, "width": 40, "height": 12})
    );
}

#[cfg(target_os = "linux")]
#[test]
fn artifact_ref_tampering_is_reported_at_the_ocr_boundary() {
    let fixture = Fixture::new();
    let store = ArtifactStore::default();
    let provider =
        aad_ocr::OcrProvider::with_command(vec![fixture.engine.display().to_string()], true)
            .unwrap();
    let reference = store
        .import_bytes(std::fs::read(&fixture.image).unwrap(), Some("image/png"))
        .unwrap();
    let mut tampered = reference.to_value();
    tampered["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    let error = provider
        .invoke_with_artifacts(
            "vision.ocr.recognize_artifact@1",
            json!({"artifact": tampered}),
            Some(Duration::from_secs(5)),
            &store,
        )
        .unwrap_err();
    assert_eq!(error.code, "OCR.ARTIFACT_IPC");
    assert_eq!(error.cause.unwrap().code, "ARTIFACT.INTEGRITY_FAILED");
}
