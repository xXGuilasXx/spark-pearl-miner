#!/usr/bin/env python3
"""SASS gate of the fused GEMM + hash kernel (docs/en/KERNEL.md, "Resource limits").

Disassembles libspm_cuda.a with cuobjdump and checks every instantiation of the fused kernel
(spm::gemm::gemm_hash_kernel<...>, strategy C):

  * present: IMMA.16832.S8.S8 (int8 mma.sync m16n8k32), LDSM (ldmatrix), UTMALDG (TMA tile
    loads) and SYNCS (mbarrier waits / arrivals);
  * absent: HMMA, and local memory (STL / LDL: register spills or a stack frame);
  * cuobjdump -res-usage: STACK:0 and LOCAL:0.

No HMMA may appear anywhere in the library. Prints the opcode counts of every kernel.

Usage: tools/check-sass.py [path/to/libspm_cuda.a]
Without an argument it picks the newest libspm_cuda.a under $CARGO_TARGET_DIR (or ./target), so
build spm-gpu first. cuobjdump is looked up on PATH, then in $CUDA_HOME/bin (default
/usr/local/cuda); $CUOBJDUMP overrides both. Exit status: 0 pass, 1 fail, 2 cannot run (no
library or no cuobjdump).
"""
import collections
import glob
import os
import re
import shutil
import subprocess
import sys

FUSED = "gemm_hash_kernel"
TRACKED = re.compile(r"\b(IMMA|HMMA|QMMA|LDSM|LDGSTS|UTMALDG|UTMAPF|SYNCS|STL|LDL)(\.[A-Z0-9_.]+)?\b")
REQUIRED = {
    "IMMA.16832.S8.S8": "int8 mma.sync m16n8k32",
    "LDSM": "ldmatrix",
    "UTMALDG": "TMA tile load",
    "SYNCS": "mbarrier",
}
FORBIDDEN = {"HMMA": "fp16 tensor-core op", "STL": "local store (spill)", "LDL": "local load (spill)"}


def newest_lib():
    target = os.environ.get("CARGO_TARGET_DIR", "target")
    libs = glob.glob(os.path.join(target, "*", "build", "spm-gpu-*", "out", "libspm_cuda.a"))
    if not libs:
        print(f"check-sass: no libspm_cuda.a under {target}; build spm-gpu first or pass the path")
        sys.exit(2)
    return max(libs, key=os.path.getmtime)


def find_cuobjdump():
    explicit = os.environ.get("CUOBJDUMP")
    if explicit:
        return explicit if os.access(explicit, os.X_OK) else None
    found = shutil.which("cuobjdump")
    if found:
        return found
    cand = os.path.join(os.environ.get("CUDA_HOME", "/usr/local/cuda"), "bin", "cuobjdump")
    return cand if os.access(cand, os.X_OK) else None


def run(tool, *args):
    return subprocess.run([tool, *args], check=True, capture_output=True, text=True).stdout


def main():
    lib = sys.argv[1] if len(sys.argv) > 1 else newest_lib()
    tool = find_cuobjdump()
    if tool is None:
        print("check-sass: cuobjdump not found (install the CUDA toolkit or set CUOBJDUMP)")
        sys.exit(2)

    counts = collections.defaultdict(collections.Counter)
    func = None
    for line in run(tool, "-sass", lib).splitlines():
        m = re.search(r"Function : (\S+)", line)
        if m:
            func = m.group(1)
            counts[func]  # every kernel is listed, even one without tracked opcodes
            continue
        if func is None:
            continue
        m = TRACKED.search(line)
        if m:
            counts[func][m.group(0)] += 1

    resources = {}
    func = None
    for line in run(tool, "-res-usage", lib).splitlines():
        m = re.search(r"Function (\S+):", line)
        if m:
            func = m.group(1)
            continue
        m = re.search(r"REG:(\d+) STACK:(\d+) SHARED:(\d+) LOCAL:(\d+)", line)
        if m and func:
            resources[func] = tuple(int(x) for x in m.groups())

    ok = True
    print(f"library: {lib}")
    fused = sorted(f for f in counts if FUSED in f)
    if not fused:
        print(f"  FAIL: no {FUSED} instantiation in the library")
        ok = False
    for f in sorted(counts):
        ops = ", ".join(f"{op} x{n}" for op, n in sorted(counts[f].items())) or "(none tracked)"
        res = resources.get(f)
        res_s = f" [REG {res[0]}, STACK {res[1]}, LOCAL {res[3]}]" if res else ""
        print(f"{f}{res_s}: {ops}")
        if any(op.startswith("HMMA") for op in counts[f]):
            print("  FAIL: HMMA present")
            ok = False
    for f in fused:
        ops = counts[f]
        for prefix, what in REQUIRED.items():
            if not any(op.startswith(prefix) for op in ops):
                print(f"  FAIL: {f} has no {prefix} ({what})")
                ok = False
        for prefix, what in FORBIDDEN.items():
            if any(op == prefix or op.startswith(prefix + ".") for op in ops):
                print(f"  FAIL: {f} contains {prefix} ({what})")
                ok = False
        res = resources.get(f)
        if res is None:
            print(f"  FAIL: no -res-usage line for {f}")
            ok = False
        elif res[1] != 0 or res[3] != 0:
            print(f"  FAIL: {f} uses a stack ({res[1]} B) or local memory ({res[3]} B)")
            ok = False
    print("SASS gate:", "pass" if ok else "FAIL", f"({len(fused)} fused kernel(s) checked)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
