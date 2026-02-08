use std::env;
use std::fs;
use std::path::Path;

fn main() {
    embuild::espidf::sysenv::output();

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
    fs::write(out_path.join("index_start.html"), parts[0]).expect("Failed to write index_start.html");
    fs::write(out_path.join("index_end.html"), parts[1]).expect("Failed to write index_end.html");
}
