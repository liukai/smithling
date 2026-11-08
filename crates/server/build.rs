//! Embeds the built UI (`ui/dist`) if it exists, else a placeholder page. The UI is built on its own
//! (`npm run build` in ui/, or the CI `ui` job), so building the server never needs Node (M2-FR-25).

use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("ui");
    println!("cargo:rerun-if-changed={}", dist.display());
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();
    if dist.join("index.html").exists() {
        copy_dir(&dist, &out);
        println!("cargo:rerun-if-changed={}", dist.join("index.html").display());
    } else {
        println!(
            "cargo:warning=ui/dist not found; embedding a placeholder page (run `npm ci && npm run build` in ui/)"
        );
        fs::write(out.join("index.html"), PLACEHOLDER).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dest).unwrap();
            copy_dir(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
            println!("cargo:rerun-if-changed={}", e.path().display());
        }
    }
}

const PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8><title>tracedb</title>\
<body style=\"font-family:system-ui;margin:3em\"><h1>tracedb</h1>\
<p>The UI was not built into this binary. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> and \
rebuild, or start with <code>--ui-dir ui/dist</code>. The API is available at <code>/api/…</code>.</p>";
