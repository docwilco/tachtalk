use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    embuild::espidf::sysenv::output();

    // Expose full git version (e.g. "v0.1.0" or "v0.1.0-test3-5-gabcdef")
    let git_version = Command::new("git")
        .args(["describe", "--tags", "--always"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=GIT_VERSION={}", git_version.trim());
    // Rebuild when HEAD moves or tags change
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/tags");

    // Generate HTML parts from index.html
    let out_dir = env::var("OUT_DIR").unwrap();
    let html_path = Path::new("src/index.html");

    println!("cargo:rerun-if-changed=src/index.html");

    let html = fs::read_to_string(html_path).expect("Failed to read src/index.html");

    // Split at the {{SSE_PORT}} placeholder
    let parts: Vec<&str> = html.split("{{SSE_PORT}}").collect();
    assert_eq!(
        parts.len(),
        2,
        "Expected exactly one {{{{SSE_PORT}}}} placeholder in index.html, found {}",
        parts.len() - 1
    );

    let out_path = Path::new(&out_dir);
    fs::write(out_path.join("index_start.html"), parts[0])
        .expect("Failed to write index_start.html");
    fs::write(out_path.join("index_end.html"), parts[1]).expect("Failed to write index_end.html");
}
