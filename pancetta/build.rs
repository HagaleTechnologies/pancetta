#[path = "src/build_identity.rs"]
mod build_identity;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let plist = manifest.join("macos").join("Info.plist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", plist.display());
    emit_build_identity(&manifest);
    // Build scripts run on the host: decide by the target, never by cfg!.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let path = plist.to_str().expect("Info.plist path must be UTF-8");
        assert!(!path.contains(','), "-Wl splits on commas: {path}");
        println!("cargo:rustc-link-arg-bin=pancetta=-Wl,-sectcreate,__TEXT,__info_plist,{path}");
    }
}

fn emit_build_identity(manifest: &std::path::Path) {
    use build_identity::{build_date, git, rerun_paths, short_sha};
    for path in rerun_paths(manifest, |name| {
        git(manifest, &["rev-parse", "--git-path", name])
    }) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!(
        "cargo:rustc-env=PANCETTA_GIT_SHA={}",
        short_sha(git(manifest, &["rev-parse", "--short=7", "HEAD"]).as_deref())
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs() as i64;
    let date = build_date(std::env::var("SOURCE_DATE_EPOCH").ok().as_deref(), now)
        .unwrap_or_else(|e| panic!("{e}"));
    println!("cargo:rustc-env=PANCETTA_BUILD_DATE={date}");
    println!(
        "cargo:rustc-env=PANCETTA_BUILD_TARGET={}",
        std::env::var("TARGET").expect("cargo TARGET")
    );
}
