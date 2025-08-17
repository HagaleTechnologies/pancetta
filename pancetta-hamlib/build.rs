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

    // Try to find hamlib library
    if let Ok(lib_dir) = env::var("HAMLIB_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", lib_dir);
    }

    if let Ok(include_dir) = env::var("HAMLIB_INCLUDE_DIR") {
        println!("cargo:include={}", include_dir);
    }

    // Try pkg-config first
    match pkg_config::Config::new()
        .atleast_version("4.0")
        .probe("hamlib")
    {
        Ok(library) => {
            println!("cargo:rustc-cfg=feature=\"hamlib-found\"");
            for path in &library.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for lib in &library.libs {
                println!("cargo:rustc-link-lib={}", lib);
            }
        }
        Err(_) => {
            // Fallback: try common library names and paths
            println!("cargo:warning=hamlib not found via pkg-config, trying fallback");
            
            // Common library names to try
            let lib_names = ["hamlib", "rig"];
            let mut found = false;
            
            for lib_name in &lib_names {
                // Try linking
                println!("cargo:rustc-link-lib={}", lib_name);
                found = true;
                break; // For now, just try the first one
            }
            
            if !found {
                println!("cargo:warning=hamlib library not found. Mock rig will be used instead.");
                println!("cargo:warning=To use real hardware, install hamlib development packages:");
                println!("cargo:warning=  Ubuntu/Debian: sudo apt-get install libhamlib-dev");
                println!("cargo:warning=  Fedora/RHEL: sudo dnf install hamlib-devel");
                println!("cargo:warning=  macOS: brew install hamlib");
                println!("cargo:rustc-cfg=feature=\"no-hamlib\"");
            } else {
                println!("cargo:rustc-cfg=feature=\"hamlib-found\"");
            }
        }
    }

    // Platform-specific configuration
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    match target_os.as_str() {
        "windows" => {
            // Windows-specific configuration
            println!("cargo:rustc-link-lib=ws2_32");
            if env::var("CARGO_CFG_TARGET_ENV").unwrap() == "msvc" {
                // MSVC-specific settings
                println!("cargo:rustc-link-lib=user32");
            }
        }
        "macos" => {
            // macOS-specific configuration
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=IOKit");
        }
        "linux" => {
            // Linux-specific configuration
            println!("cargo:rustc-link-lib=pthread");
        }
        _ => {
            // Other Unix-like systems
            println!("cargo:rustc-link-lib=pthread");
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
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
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