//! Compiles resources/phodder.gresource.xml with glib-compile-resources
//! (a build dependency; see shell.nix). main.rs registers the blob.

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let out_path = std::path::Path::new(&out_dir).join("phodder.gresource");

    let status = std::process::Command::new("glib-compile-resources")
        .arg("--sourcedir=resources")
        .arg(format!("--target={}", out_path.display()))
        .arg("resources/phodder.gresource.xml")
        .status()
        .expect("failed to run glib-compile-resources -- is it on PATH? (see shell.nix's own nativeBuildInputs entry for it)");

    if !status.success() {
        panic!("glib-compile-resources failed with status: {status}");
    }

    println!("cargo:rerun-if-changed=resources/phodder.gresource.xml");
    // A directory reruns the build when any file in it changes.
    println!("cargo:rerun-if-changed=resources/icons");
    println!("cargo:rerun-if-changed=../../data/net.dunevent.Phodder.metainfo.xml");
}
