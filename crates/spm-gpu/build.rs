//! Compiles cuda/{common,prep,gemm,job}/*.cu into a static library with nvcc and links it (plus
//! cudart) into this crate.
//!
//! Environment:
//! - `SPM_CUDA_ARCH`: target(s), default "sm_121a" (GB10); e.g. "sm_120a" or "sm_121a,sm_120f".
//! - `NVCC`: compiler, default "nvcc". `CUDA_HOME`: default /usr/local/cuda.
//! - `SPM_PTXAS_VERBOSE=1`: pass `-Xptxas -v` and print the register/spill report as warnings.
//! - `SPM_NVCC_FLAGS`: extra flags for experiments, e.g. "-DSPM_TRANSCRIPT_IN_REGS=1".
use std::{env, fs, path::PathBuf, process::Command};

const SOURCE_DIRS: [&str; 4] = ["common", "prep", "gemm", "job"];

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let cuda_dir = root.join("cuda");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = env::var("SPM_CUDA_ARCH").unwrap_or_else(|_| "sm_121a".into());
    let nvcc = env::var("NVCC").unwrap_or_else(|_| "nvcc".into());
    let verbose = env::var("SPM_PTXAS_VERBOSE").is_ok_and(|v| v == "1");
    let extra: Vec<String> = env::var("SPM_NVCC_FLAGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();

    let mut srcs = Vec::new();
    for sub in SOURCE_DIRS.iter().copied().chain(["include"]) {
        let Ok(rd) = fs::read_dir(cuda_dir.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if matches!(ext, "cu" | "cuh" | "h" | "hpp") {
                println!("cargo:rerun-if-changed={}", p.display());
            }
            if ext == "cu" && sub != "include" {
                srcs.push(p);
            }
        }
    }
    srcs.sort();

    let objs: Vec<PathBuf> = std::thread::scope(|scope| {
        let jobs: Vec<_> = srcs
            .iter()
            .map(|s| {
                let obj = out.join(format!("{}.o", s.file_stem().unwrap().to_string_lossy()));
                let mut cmd = Command::new(&nvcc);
                cmd.arg("-O3")
                    .arg("-std=c++17")
                    .arg("-Xcompiler")
                    .arg("-fPIC")
                    .arg("-lineinfo")
                    .arg("-I")
                    .arg(cuda_dir.join("include"))
                    .arg("-I")
                    .arg(root.join("third_party/cutlass/include"));
                for a in arch.split(',') {
                    let compute = a.replace("sm_", "compute_");
                    cmd.arg("-gencode").arg(format!("arch={compute},code={a}"));
                }
                if verbose {
                    cmd.arg("-Xptxas").arg("-v");
                }
                cmd.args(&extra);
                cmd.arg("-c").arg(s).arg("-o").arg(&obj);
                let src = s.clone();
                scope.spawn(move || {
                    let output = cmd
                        .output()
                        .expect("nvcc not found: install CUDA 13 or set NVCC");
                    let log = String::from_utf8_lossy(&output.stderr);
                    if !output.status.success() {
                        panic!("nvcc failed on {}:\n{log}", src.display());
                    }
                    if verbose {
                        for line in log.lines().filter(|l| {
                            l.contains("registers")
                                || l.contains("spill")
                                || l.contains("Compiling entry")
                        }) {
                            println!(
                                "cargo:warning={}: {line}",
                                src.file_name().unwrap().to_string_lossy()
                            );
                        }
                    }
                    obj
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().expect("nvcc thread panicked"))
            .collect()
    });

    println!("cargo:rerun-if-env-changed=SPM_CUDA_ARCH");
    println!("cargo:rerun-if-env-changed=SPM_PTXAS_VERBOSE");
    println!("cargo:rerun-if-env-changed=SPM_NVCC_FLAGS");
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
