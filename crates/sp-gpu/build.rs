//! Build script: on a Windows target, compile the vendored Spout2 SDK
//! 2.007.017 SpoutDX sender sources (`vendor/spout2/`, BSD-2, unmodified) and
//! the `extern "C"` shim (`src/win/spout_shim.cpp`) into one static library
//! with MSVC, through the `cc` crate (#223 S1b). Any other target has no Spout
//! (and no Direct3D), so the script does nothing there: the Linux CI jobs never
//! see the C++.

/// The vendored SDK files that are compiled: upstream's `SpoutDX_SOURCES`
/// (`SPOUTSDK/SpoutDirectX/SpoutDX/CMakeLists.txt`), flat in one folder.
const SPOUT_SOURCES: [&str; 7] = [
    "SpoutDX.cpp",
    "SpoutCopy.cpp",
    "SpoutDirectX.cpp",
    "SpoutFrameCount.cpp",
    "SpoutSenderNames.cpp",
    "SpoutSharedMemory.cpp",
    "SpoutUtils.cpp",
];

/// The system libraries the SDK calls into. MSVC would take most from the
/// SDK's `#pragma comment(lib, …)` lines, but `user32` (MessageBox,
/// EnumDisplaySettings) has none, so every one is named here.
const SYSTEM_LIBS: [&str; 10] = [
    "d3d11", "dxgi", "user32", "gdi32", "advapi32", "shell32", "comctl32", "version", "winmm",
    "psapi",
];

const VENDOR: &str = "vendor/spout2";
const SHIM: &str = "src/win/spout_shim.cpp";

fn main() {
    println!("cargo:rerun-if-changed={VENDOR}");
    println!("cargo:rerun-if-changed={SHIM}");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("cargo sets CARGO_CFG_TARGET_OS for every build script");
    if target_os != "windows" {
        return;
    }
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        // The SDK uses try/catch, and cc sets no exception model for MSVC.
        .flag("-EHsc")
        // Upstream's own static Release build: CMake's SPOUT_BUILD_STATIC,
        // NDEBUG, and no UNICODE (the vcxproj's MultiByte).
        .define("SPOUT_BUILD_STATIC", None)
        .define("NDEBUG", None)
        // Vendored code: its warnings are not ours to fix.
        .warnings(false)
        .cargo_warnings(false)
        .include(VENDOR)
        .file(SHIM);
    for source in SPOUT_SOURCES {
        build.file(format!("{VENDOR}/{source}"));
    }
    build.compile("sp_spout");
    for lib in SYSTEM_LIBS {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
