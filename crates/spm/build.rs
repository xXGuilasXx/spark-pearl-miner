// Records the git commit for `version` and the About screen. Release builds pass SPM_GIT_COMMIT
// explicitly; a checkout without git reports "unknown".
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SPM_GIT_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when HEAD moves, so an incremental build does not keep the previous commit.
    if let Some(git_dir) = git_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("packed-refs").display());
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
            if let Some(r) = head.trim().strip_prefix("ref: ") {
                println!("cargo:rerun-if-changed={}", git_dir.join(r).display());
            }
        }
    }
    let commit = std::env::var("SPM_GIT_COMMIT").ok().filter(|c| !c.is_empty()).or_else(|| {
        let out = Command::new("git").args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=SPM_GIT_COMMIT={}", commit.unwrap_or_else(|| "unknown".into()));
}

/// The repository's `.git` directory for this checkout (worktree-aware), or None
/// outside a checkout. `--git-dir` is correct in worktrees (it points at
/// `<common>/worktrees/<name>`), where `--git-common-dir` would point at the
/// main checkout's `.git` and miss this worktree's HEAD moves.
fn git_dir() -> Option<std::path::PathBuf> {
    let out = Command::new("git").args(["rev-parse", "--git-dir"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(if p.is_absolute() { p } else { std::env::current_dir().ok()?.join(p) })
}
