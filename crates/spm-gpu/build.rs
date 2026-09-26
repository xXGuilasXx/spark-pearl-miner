//! Compiles cuda/{common,prep,gemm}/*.cu into a static library with nvcc and links it (plus cudart)
//! into this crate.
//! Arch: sm_121a by default (GB10); override with SPM_CUDA_ARCH (e.g. "sm_120a" or "sm_121a,sm_120f").
//! SPM_PTXAS_VERBOSE=1 prints the `ptxas -v` report (registers, spills, smem) as build warnings.
use std::{env, fs, path::Path, path::PathBuf, process::Command};

const SOURCE_DIRS: [&str; 3] = ["common", "prep", "gemm"];

fn watch_dir(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(ext, "cu" | "cuh" | "h" | "hpp") {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let cuda_dir = root.join("cuda");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = env::var("SPM_CUDA_ARCH").unwrap_or_else(|_| "sm_121a".into());
    let nvcc = env::var("NVCC").unwrap_or_else(|_| "nvcc".into());
    let verbose = env::var("SPM_PTXAS_VERBOSE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let mut srcs = Vec::new();
    for sub in SOURCE_DIRS {
        watch_dir(&cuda_dir.join(sub));
        if let Ok(rd) = fs::read_dir(cuda_dir.join(sub)) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "cu").unwrap_or(false) {
                    srcs.push(p);
                }
            }
        }
    }
    watch_dir(&cuda_dir.join("include"));
    srcs.sort();
    let mut objs = Vec::new();
    for s in &srcs {
        let obj = out.join(format!("{}.o", s.file_stem().unwrap().to_string_lossy()));
        let mut cmd = Command::new(&nvcc);
        cmd.arg("-O3")
            .arg("-std=c++17")
            .arg("-Xcompiler")
            .arg("-fPIC")
            .arg("-lineinfo");
        cmd.arg("-I").arg(cuda_dir.join("include"));
        for sub in SOURCE_DIRS {
            cmd.arg("-I").arg(cuda_dir.join(sub));
        }
        cmd.arg("-I").arg(root.join("third_party/cutlass/include"));
        if verbose {
            cmd.arg("-Xptxas").arg("-v");
        }
        for a in arch.split(',') {
            let compute = a.replace("sm_", "compute_");
            cmd.arg("-gencode").arg(format!("arch={compute},code={a}"));
        }
        cmd.arg("-c").arg(s).arg("-o").arg(&obj);
        let output = cmd
            .output()
            .expect("nvcc not found: install CUDA 13 or set NVCC");
        let log = String::from_utf8_lossy(&output.stderr);
        if verbose || !output.status.success() {
            for line in log.lines().filter(|l| !l.trim().is_empty()) {
                println!(
                    "cargo:warning={}: {line}",
                    s.file_name().unwrap().to_string_lossy()
                );
            }
        }
        assert!(
            output.status.success(),
            "nvcc failed on {}:\n{log}",
            s.display()
        );
        objs.push(obj);
    }
    println!("cargo:rerun-if-env-changed=SPM_CUDA_ARCH");
    println!("cargo:rerun-if-env-changed=SPM_PTXAS_VERBOSE");
    println!("cargo:rerun-if-env-changed=NVCC");
    let lib = out.join("libspm_cuda.a");
    let _ = fs::remove_file(&lib);
    let st = Command::new("ar")
        .arg("rcs")
        .arg(&lib)
        .args(&objs)
        .status()
        .expect("ar");
    assert!(st.success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=spm_cuda");
    let cuda_home = env::var("CUDA_HOME").unwrap_or_else(|_| "/usr/local/cuda".into());
    println!("cargo:rustc-link-search=native={cuda_home}/lib64");
    println!("cargo:rustc-link-search=native={cuda_home}/targets/sbsa-linux/lib");
    println!("cargo:rustc-link-lib=dylib=cudart");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
