//! OpenH264 is compiled from source by `openh264-sys2`, which quietly leaves out its SIMD
//! assembly when `nasm` is not installed. The encoder still works but is several times slower
//! (a 1080p frame takes ~70 ms instead of ~17 ms, so the stream tops out near 14 fps), so say
//! so loudly. `DALKER_REQUIRE_NASM=1` (set by CI and the release workflow) makes it an error.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for var in ["NASM", "PATH", "OPENH264_NO_ASM", "DALKER_REQUIRE_NASM"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !matches!(arch.as_str(), "x86" | "x86_64") || std::env::var_os("OPENH264_NO_ASM").is_some() {
        return;
    }
    let nasm = std::env::var("NASM").unwrap_or_else(|_| "nasm".into());
    if Command::new(&nasm)
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return;
    }
    let msg = "nasm not found: OpenH264 is built without its assembly and encodes video several \
               times slower. Install NASM (winget install NASM.NASM / apt install nasm), then \
               `cargo clean -p openh264-sys2` and rebuild.";
    if std::env::var("DALKER_REQUIRE_NASM").is_ok_and(|v| v != "0" && !v.is_empty()) {
        panic!("{msg}");
    }
    println!("cargo:warning={msg}");
}
