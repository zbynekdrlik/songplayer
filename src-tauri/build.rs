fn main() {
    // Set build metadata for runtime version display
    println!(
        "cargo:rustc-env=BUILD_VERSION={}",
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into())
    );
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", chrono_lite_now());

    tauri_build::build();
}

fn chrono_lite_now() -> String {
    // Simple UTC timestamp without pulling in chrono for build script
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}
