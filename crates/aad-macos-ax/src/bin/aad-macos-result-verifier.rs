use aad_macos_ax::result_verifier::{failure_document, verify, VerifyOptions};
use std::path::PathBuf;

fn main() {
    let mut options = VerifyOptions::default();
    let mut archive: Option<PathBuf> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let target = match argument.as_str() {
            "--expected-archive-sha256" => &mut options.expected_archive_sha256,
            "--expected-source-revision" => &mut options.expected_source_revision,
            "--expected-source-package-digest" => &mut options.expected_source_package_digest,
            _ if !argument.starts_with('-') && archive.is_none() => {
                archive = Some(PathBuf::from(argument));
                continue;
            }
            _ => {
                print_and_exit(failure_document("usage", usage()), 64);
            }
        };
        let Some(value) = arguments.next() else {
            print_and_exit(failure_document("usage", usage()), 64);
        };
        *target = Some(value);
    }

    let Some(archive) = archive else {
        print_and_exit(failure_document("usage", usage()), 64);
    };
    match verify(&archive, &options) {
        Ok(result) => {
            let qualified = result["qualified"].as_bool() == Some(true);
            print_and_exit(result, if qualified { 0 } else { 1 });
        }
        Err(error) => print_and_exit(error.document(), 1),
    }
}

fn usage() -> &'static str {
    "用法：aad-macos-result-verifier [--expected-archive-sha256 HEX] \
     [--expected-source-revision SHA] [--expected-source-package-digest HEX] \
     /path/to/macos-ax-test-result.tar.gz"
}

fn print_and_exit(document: serde_json::Value, code: i32) -> ! {
    println!(
        "{}",
        serde_json::to_string(&document).expect("serialize verifier result")
    );
    std::process::exit(code);
}
