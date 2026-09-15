use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/exactextract");
    for key in [
        "SKARVE_EE_CACHE",
        "SKARVE_GEOS_INCLUDE_DIR",
        "SKARVE_GEOS_LIBRARY",
        "SKARVE_GDAL_LIBRARY",
        "SKARVE_CMAKE",
        "PYTHON",
        "CXX",
        "AR",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    if env::var_os("CARGO_FEATURE_EXACTEXTRACT").is_none() {
        return;
    }
    let out = env::var("OUT_DIR").expect("Cargo sets OUT_DIR");
    let python = env::var("PYTHON").unwrap_or_else(|_| "python3".into());
    let status = Command::new(python)
        // Cargo may run another Rust compiler concurrently. Use one native
        // build worker so `cargo -j2` stays within the two-worker release bound.
        .args(["native/exactextract/build.py", "--out", &out, "--jobs", "1"])
        .status()
        .expect("optional exactextract build requires Python 3, CMake and C++17 build tools");
    assert!(
        status.success(),
        "optional exactextract build failed; see the preceding precise prerequisite or compilation error"
    );
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=skarve_exactextract_bridge");
    println!("cargo:rustc-link-lib=static=exactextract");
    println!("cargo:rustc-link-lib=dylib=geos_c");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
