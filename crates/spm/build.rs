// Records the git commit for `version` and the About screen. Release builds pass SPM_GIT_COMMIT
// explicitly; a checkout without git reports "unknown".
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SPM_GIT_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when HEAD moves, so an incremental build does not keep the previous commit.
    if let Some(head_path) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={}", head_path.display());
        if let Some(packed_refs) = git_path("packed-refs") {
            println!("cargo:rerun-if-changed={}", packed_refs.display());
        }
        if let Ok(head) = std::fs::read_to_string(head_path) {
            if let Some(r) = head.trim().strip_prefix("ref: ") {
                if let Some(reference) = git_path(r) {
                    println!("cargo:rerun-if-changed={}", reference.display());
                }
            }
        }
    }
    let commit = std::env::var("SPM_GIT_COMMIT").ok().filter(|c| !c.is_empty()).or_else(|| {
        let out = Command::new("git").args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=SPM_GIT_COMMIT={}", commit.unwrap_or_else(|| "unknown".into()));
}

/// Resolve each Git metadata path separately: worktrees have their own HEAD,
/// but share branch refs and packed-refs with the common repository directory.
fn git_path(path: &str) -> Option<std::path::PathBuf> {
    let out = Command::new("git").args(["rev-parse", "--git-path", path]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(if p.is_absolute() { p } else { std::env::current_dir().ok()?.join(p) })
}
