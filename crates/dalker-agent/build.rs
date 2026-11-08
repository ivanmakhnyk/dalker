//! Gives dalkerd.exe the Dalker icon (nothing to do on other targets).

fn main() {
    println!("cargo:rerun-if-changed=dalkerd.rc");
    println!("cargo:rerun-if-changed=../../assets/branding/dalker.ico");
    embed_resource::compile("dalkerd.rc", embed_resource::NONE).manifest_optional().unwrap();
}
