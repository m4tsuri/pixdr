use std::{env, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let uhd_include = env::var("UHD_INCLUDE_DIR")
        .expect("UHD_INCLUDE_DIR must be set so pixdr can include UHD Android context headers");
    let uhd_source_include = manifest_dir.join("../external/uhd/host/include");

    cxx_build::bridge("src/android_uhd_context.rs")
        .file("src/uhd_android_context.cc")
        .include(uhd_include)
        .include(uhd_source_include)
        .flag_if_supported("-std=c++17")
        .compile("pixdr_uhd_android_context");

    println!("cargo:rerun-if-changed=src/android_uhd_context.rs");
    println!("cargo:rerun-if-changed=src/uhd_android_context.cc");
    println!("cargo:rerun-if-env-changed=UHD_INCLUDE_DIR");
}
