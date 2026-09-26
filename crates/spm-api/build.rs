// The web GUI is embedded with include_dir!, which does not tell Cargo about the files it reads.
// Rebuild whenever anything under webui/ changes (Cargo scans a directory recursively).
fn main() {
    println!("cargo:rerun-if-changed=../../webui");
    println!("cargo:rerun-if-changed=build.rs");
}
