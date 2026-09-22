use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=MPC_PDFIUM_SIGNED_SHA256");
    // The signed PDFium digest is a build input, not an install-directory
    // marker. Keep it target-specific so macOS builds do not accidentally
    // acquire a Windows runtime trust value.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if let Ok(signed_sha256) = env::var("MPC_PDFIUM_SIGNED_SHA256") {
            let signed_sha256 = signed_sha256.to_ascii_lowercase();
            if signed_sha256.len() != 64
                || !signed_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                panic!("MPC_PDFIUM_SIGNED_SHA256 must contain exactly 64 hexadecimal characters");
            }
            println!("cargo:rustc-env=MPC_PDFIUM_SIGNED_SHA256={signed_sha256}");
        }
    }
    tauri_build::build()
}
