use std::path::Path;
use std::process::Command;

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui");
    for p in [
        "src",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
    ] {
        println!("cargo:rerun-if-changed={}", ui.join(p).display());
    }
    println!("cargo:rerun-if-env-changed=TRACEDB_SKIP_UI");

    if std::env::var_os("TRACEDB_SKIP_UI").is_some() && ui.join("dist/index.html").exists() {
        return;
    }
    if !ui.join("node_modules").exists() {
        run(Command::new("npm")
            .args(["ci", "--no-audit", "--no-fund"])
            .current_dir(&ui));
    }
    run(Command::new("npm").args(["run", "build"]).current_dir(&ui));
}

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e} (is Node installed?)"));
    assert!(status.success(), "{cmd:?} failed with {status}");
}
