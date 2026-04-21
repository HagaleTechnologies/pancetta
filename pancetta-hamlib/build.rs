//! Build script for pancetta-hamlib
//!
//! This build script handles:
//! - Detection of hamlib library on the system
//! - Generation of bindings if needed
//! - Platform-specific configuration

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(hamlib_found)");
    println!("cargo::rustc-check-cfg=cfg(no_hamlib)");

    // Check for explicit hamlib path overrides
    if let Ok(lib_dir) = env::var("HAMLIB_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", lib_dir);
    }
    if let Ok(include_dir) = env::var("HAMLIB_INCLUDE_DIR") {
        println!("cargo:include={}", include_dir);
    }

    // Try pkg-config to find hamlib
    let hamlib_found = match pkg_config::Config::new()
        .atleast_version("4.0")
        .probe("hamlib")
    {
        Ok(library) => {
            println!("cargo:rustc-cfg=hamlib_found");
            for path in &library.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for lib in &library.libs {
                println!("cargo:rustc-link-lib={}", lib);
            }
            true
        }
        Err(_) => {
            // hamlib C library not found — this is fine, we use rigctld (TCP) instead.
            println!("cargo:rustc-cfg=no_hamlib");
            false
        }
    };

    // Platform-specific configuration (only needed when linking hamlib)
    if hamlib_found {
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
        match target_os.as_str() {
            "windows" => {
                println!("cargo:rustc-link-lib=ws2_32");
                if env::var("CARGO_CFG_TARGET_ENV").unwrap() == "msvc" {
                    println!("cargo:rustc-link-lib=user32");
                }
            }
            "macos" => {
                println!("cargo:rustc-link-lib=framework=CoreFoundation");
                println!("cargo:rustc-link-lib=framework=IOKit");
            }
            "linux" => {
                println!("cargo:rustc-link-lib=pthread");
            }
            _ => {
                println!("cargo:rustc-link-lib=pthread");
            }
        }
    }

    // Generate bindings if requested
    if env::var("REGENERATE_BINDINGS").is_ok() {
        generate_bindings();
    }
}

/// Generate FFI bindings using bindgen
fn generate_bindings() {
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Include only hamlib functions
        .allowlist_function("rig_.*")
        .allowlist_type("rig_.*")
        .allowlist_type("RIG_.*")
        .allowlist_var("RIG_.*")
        // Generate comments from headers
        .generate_comments(true)
        // Use core instead of std for no_std compatibility
        .use_core()
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
