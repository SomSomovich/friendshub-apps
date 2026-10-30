fn main() {
    println!("cargo:rerun-if-changed=Info.plist");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo must provide the macOS package directory");
    let info_plist = std::path::PathBuf::from(manifest_dir).join("Info.plist");

    // The current distribution is a standalone Mach-O executable. Embedding
    // the plist gives macOS its microphone usage text without requiring a
    // surrounding .app bundle. Pass each ld argument through clang separately
    // so paths with spaces or commas remain a single argument.
    for argument in ["-sectcreate", "__TEXT", "__info_plist"] {
        println!("cargo:rustc-link-arg=-Xlinker");
        println!("cargo:rustc-link-arg={argument}");
    }
    println!("cargo:rustc-link-arg=-Xlinker");
    println!("cargo:rustc-link-arg={}", info_plist.display());
}
