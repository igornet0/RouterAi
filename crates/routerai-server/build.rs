//! Ensure `web/dist` exists so `rust-embed` can bake the Console into the binary.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let web_dir = manifest.join("../../web");
    let dist_index = web_dir.join("dist/index.html");

    println!("cargo:rerun-if-changed={}", dist_index.display());
    println!("cargo:rerun-if-changed={}", web_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        web_dir.join("package.json").display()
    );

    if dist_index.is_file() {
        return;
    }

    eprintln!(
        "routerai-server: web/dist missing — running `npm run build` in {}",
        web_dir.display()
    );

    let status = Command::new("npm")
        .args(["run", "build"])
        .current_dir(&web_dir)
        .status();

    match status {
        Ok(s) if s.success() && dist_index.is_file() => {}
        Ok(s) => {
            panic!(
                "npm run build failed (exit {s}). Run `cd web && npm install && npm run build` then rebuild."
            );
        }
        Err(err) => {
            panic!(
                "web/dist missing and npm unavailable ({err}). Run `cd web && npm install && npm run build` then rebuild."
            );
        }
    }
}
