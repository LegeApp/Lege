use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rustc-env=JBIG2ENC_VERSION=0.29");

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=VERGEN_BUILD_TIMESTAMP={ts}");
}
