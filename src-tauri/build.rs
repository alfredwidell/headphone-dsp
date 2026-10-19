use std::env;

fn main() {
    // Expose the compile-time target triple so the runtime can locate the
    // CamillaDSP sidecar binary named `camilladsp-<triple>[.exe]`.
    let triple = env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=TARGET_TRIPLE={triple}");
    println!("cargo:rerun-if-changed=data_ku100");
    tauri_build::build()
}
