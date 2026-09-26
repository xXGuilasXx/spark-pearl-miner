//! Builds and runs cuda/tests/layout_check.cu: the host-side proof that the fused kernel's lane
//! geometry (ldmatrix addressing through the TMA swizzle, the m16n8k32 fragment layouts of MmaS8
//! and MmaE4M3) gives every lane exactly one 8 × 16 hash tile, that the dump index is the reference
//! order and that the band raster visits every CTA tile once.
//!
//! No GPU is touched: the program only runs host code. It needs nvcc (as the crate build does) and
//! the CUTLASS submodule for CuTe's MMA and swizzle definitions; without the submodule the test is
//! skipped with a message.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn fragment_layout_gives_each_lane_one_hash_tile() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cutlass = root.join("third_party/cutlass/include");
    if !cutlass.join("cute/atom/mma_traits_sm80.hpp").exists() {
        eprintln!(
            "skipped: third_party/cutlass is not checked out \
             (git submodule update --init --depth 1 third_party/cutlass)"
        );
        return;
    }
    let nvcc = std::env::var("NVCC").unwrap_or_else(|_| "nvcc".into());
    let arch = std::env::var("SPM_CUDA_ARCH").unwrap_or_else(|_| "sm_121a".into());
    let arch = arch.split(',').next().unwrap_or("sm_121a").to_owned();
    let exe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("layout_check");
    let out = Command::new(&nvcc)
        .arg("-std=c++17")
        .arg("-I")
        .arg(&cutlass)
        .arg("-I")
        .arg(root.join("cuda/include"))
        .arg("-gencode")
        .arg(format!(
            "arch={},code={arch}",
            arch.replace("sm_", "compute_")
        ))
        .arg("-o")
        .arg(&exe)
        .arg(root.join("cuda/tests/layout_check.cu"))
        .output()
        .expect("nvcc not found: install CUDA 13 or set NVCC");
    assert!(
        out.status.success(),
        "nvcc failed on cuda/tests/layout_check.cu:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe).output().expect("run layout_check");
    let stdout = String::from_utf8_lossy(&run.stdout);
    eprintln!("{stdout}");
    assert!(run.status.success(), "layout check failed:\n{stdout}");
    for what in [
        "OK MmaS8",
        "OK MmaE4M3",
        "OK raster",
        "OK transcript",
        "all passed",
    ] {
        assert!(stdout.contains(what), "layout check output lacks {what:?}");
    }
}
