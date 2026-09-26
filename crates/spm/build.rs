// Records the git commit for `version` and the About screen. Release builds pass SPM_GIT_COMMIT
// explicitly; a checkout without git reports "unknown".
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SPM_GIT_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    let commit = std::env::var("SPM_GIT_COMMIT").ok().filter(|c| !c.is_empty()).or_else(|| {
        let out = Command::new("git").args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=SPM_GIT_COMMIT={}", commit.unwrap_or_else(|| "unknown".into()));
}
