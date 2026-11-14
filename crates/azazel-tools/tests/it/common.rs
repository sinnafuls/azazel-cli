//! Helpers the test modules share.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// The program of the fixture package `package`, built on first use into the directory this
/// test runs from. Built once per test run, however many tests ask for it.
pub fn fixture_program(package: &str) -> PathBuf {
    static BUILT: LazyLock<Mutex<HashMap<String, PathBuf>>> = LazyLock::new(Default::default);
    let mut built = BUILT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = built.get(package) {
        return binary.clone();
    }
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", package, "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the {package} fixture"
    );
    let binary = profile_dir.join(format!("{package}{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    built.insert(package.to_string(), binary.clone());
    binary
}
