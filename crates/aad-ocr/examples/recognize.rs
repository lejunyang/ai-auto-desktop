use serde_json::json;
use std::time::Duration;

fn main() {
    let path = std::env::args().nth(1).expect("usage: recognize IMAGE");
    let provider = aad_ocr::OcrProvider::new().expect("create OCR provider");
    let result = provider
        .recognize_path(
            json!({
                "image": {"path": path},
                "languages": ["eng", "chi_sim"],
                "patterns": [{"id": "ascii", "value": "TEST"}],
            }),
            Duration::from_secs(15),
        )
        .expect("recognize image");
    println!("{}", result);
}
