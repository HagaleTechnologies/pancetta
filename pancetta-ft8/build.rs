fn main() {
    // Declare the custom cfg key so Rust doesn't warn about it.
    println!("cargo::rustc-check-cfg=cfg(ft8lib_stub)");
    let ft8_dir = "vendor/ft8_lib";
    let constants_path = format!("{}/ft8/constants.c", ft8_dir);

    // Only compile the C library if the vendor source files are present
    // (they may be absent in worktrees where the git submodule is not initialized)
    if std::path::Path::new(&constants_path).exists() {
        cc::Build::new()
            .files([
                constants_path,
                format!("{}/ft8/encode.c", ft8_dir),
                format!("{}/ft8/decode.c", ft8_dir),
                format!("{}/ft8/message.c", ft8_dir),
                format!("{}/ft8/ldpc.c", ft8_dir),
                format!("{}/ft8/crc.c", ft8_dir),
                format!("{}/ft8/text.c", ft8_dir),
                format!("{}/fft/kiss_fft.c", ft8_dir),
                format!("{}/fft/kiss_fftr.c", ft8_dir),
                format!("{}/common/monitor.c", ft8_dir),
            ])
            .include(ft8_dir)
            // Suppress ft8_lib's LOG() output that corrupts the TUI.
            // monitor.c #defines LOG_LEVEL before #include debug.h, so we
            // can't override it via -D. Instead, redefine LOG_PRINTF (the
            // actual output macro) to a no-op before debug.h sees it.
            .flag("-DLOG_PRINTF(...)=")
            .warnings(false)
            .opt_level(2)
            .compile("ft8_lib");
    } else {
        // ft8_lib vendor sources not found — using pure-Rust decoder instead.
        // Signal to the Rust code that stubs should be used instead of real FFI.
        println!("cargo:rustc-cfg=ft8lib_stub");
    }

    println!("cargo:rerun-if-changed={}", ft8_dir);
}
