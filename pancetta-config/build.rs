fn main() {
    // ClubLog's per-application API key is baked in via `option_env!` in
    // src/network.rs (release builds only — CI supplies it, local `cargo
    // build` does not). Without this directive, rust-cache (release.yml)
    // could serve a stale cached build of this crate compiled before the
    // secret was set, silently shipping an empty key.
    println!("cargo:rerun-if-env-changed=CLUBLOG_API_KEY");
}
