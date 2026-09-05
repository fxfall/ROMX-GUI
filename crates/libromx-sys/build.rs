use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=ROMX_LIBROMX_DIR");
    println!("cargo:rerun-if-changed=../../vendor/libromx/CMakeLists.txt");
    println!("cargo:rerun-if-changed=../../vendor/libromx/include/romx/romx.h");
    println!("cargo:rerun-if-changed=../../vendor/libromx/src");
    println!("cargo:rerun-if-changed=../../vendor/libromx/cmake");

    let source = env::var_os("ROMX_LIBROMX_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/libromx"));
    let source = source.canonicalize().unwrap_or_else(|error| {
        panic!(
            "libromx source is unavailable at {}: {error}",
            source.display()
        )
    });
    let header = source.join("include/romx/romx.h");
    if !header.is_file() {
        panic!(
            "libromx submodule is missing or incomplete: expected {}. Initialize submodules with `git submodule update --init --recursive`.",
            header.display()
        );
    }

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_owned());
    let mut config = cmake::Config::new(&source);
    config
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("ROMX_BUILD_TESTS", "OFF")
        .define("ROMX_BUILD_EXAMPLES", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .profile(&profile)
        .out_dir(
            env::var_os("OUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_default(),
        );
    let output = config.build();

    // cmake-rs normally emits these directives.  Keep the explicit search
    // paths as well because libromx is a static-only target and different
    // generators place the archive in slightly different subdirectories.
    let candidates = [output.clone(), output.join("build"), output.join("lib")];
    for directory in candidates.iter().filter(|path| Path::new(path).is_dir()) {
        println!("cargo:rustc-link-search=native={}", directory.display());
    }
    println!("cargo:rustc-link-lib=static=romx");

    // Keep platform system libraries explicit for static builds.  The CMake
    // target has no third-party runtime dependencies, but these are required
    // by the platform file/locking implementation on the listed targets.
    if cfg!(target_os = "windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
