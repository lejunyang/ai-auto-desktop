#[cfg(target_os = "macos")]
fn main() {
    println!("{}", aad_macos::ax::manifest_json());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This example requires macos.");
    std::process::exit(1);
}
