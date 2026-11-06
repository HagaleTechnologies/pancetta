fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let plist = manifest.join("macos").join("Info.plist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", plist.display());
    // Build scripts run on the host: decide by the target, never by cfg!.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let path = plist.to_str().expect("Info.plist path must be UTF-8");
        assert!(!path.contains(','), "-Wl splits on commas: {path}");
        println!("cargo:rustc-link-arg-bin=pancetta=-Wl,-sectcreate,__TEXT,__info_plist,{path}");
    }
}
