//! Exports the target triple to the integration tests (the C smoke test needs it to pick a C
//! compiler).
fn main() {
    println!(
        "cargo:rustc-env=TARGET_TRIPLE={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!("cargo:rerun-if-changed=build.rs");
}
