use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn table(dir: &Path, args: &[&str], code: i32) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_aad"))
        .current_dir(dir)
        .arg("table")
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn cli_exports_csv_and_reconciles_keyed_data_and_exact_totals() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut data = json!({"complete":true,"columns":["ID","Note","Amount"],"row_key":"ID","row_count":2,"records":[
        {"ID":"002","Note":"line 1\nline 2","Amount":"0.10"},{"ID":"001","Note":"Say \"hi\", 茶","Amount":"-0.03"}
    ]});
    let save = |data: &Value| fs::write(dir.join("input.json"), data.to_string()).unwrap();
    save(&data);
    let export = ["export", "--input", "input.json", "--output", "report.csv"];
    table(dir, &export, 0);
    let csv = fs::read_to_string(dir.join("report.csv")).unwrap();
    assert_eq!(
        csv,
        "ID,Note,Amount\r\n002,\"line 1\nline 2\",0.10\r\n001,\"Say \"\"hi\"\", 茶\",-0.03\r\n"
    );
    table(dir, &export, 1);
    assert_eq!(fs::read_to_string(dir.join("report.csv")).unwrap(), csv);
    let check = [
        "compare",
        "--input",
        "input.json",
        "--csv",
        "expected.csv",
        "--decimal",
        "Amount",
    ];
    let expected = dir.join("expected.csv");
    fs::write(
        &expected,
        "Amount,ID,Note\r\n-0.030,001,\"Say \"\"hi\"\", 茶\"\r\n0.1,002,\"line 1\nline 2\"\r\n",
    )
    .unwrap();
    let equal = table(dir, &check, 0);
    assert_eq!(equal["matches"], true);
    assert_eq!(
        equal["totals"]["Amount"],
        json!({"actual":"0.07","expected":"0.07"})
    );
    fs::write(
        &expected,
        "ID,Note,Amount\n001,changed,-0.030\n003,replaced,0.1\n",
    )
    .unwrap();
    let different = table(dir, &check, 1);
    assert_eq!(different["difference_count"], 3);
    assert_eq!(
        different["differences"][0],
        json!({"kind":"changed","key":"001","column":"Note","actual":"Say \"hi\", 茶","expected":"changed"})
    );
    assert_eq!(different["differences"][1]["kind"], "unexpected");
    assert_eq!(different["differences"][2]["kind"], "missing");
    fs::write(&expected, "ID,Note,Amount\n001,a,1e-2\n").unwrap();
    assert!(table(dir, &check, 1).get("matches").is_none());
    data["scope"] = json!("pagination_from_current_page");
    save(&data);
    table(
        dir,
        &["export", "--input", "input.json", "--output", "suffix.csv"],
        1,
    );
    assert!(!dir.join("suffix.csv").exists());
    data["complete"] = json!(false);
    save(&data);
    table(
        dir,
        &["export", "--input", "input.json", "--output", "partial.csv"],
        1,
    );
    assert!(!dir.join("partial.csv").exists());
}
