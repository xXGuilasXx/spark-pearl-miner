#!/usr/bin/env python3
"""SASS gate of the fused kernel (docs/en/KERNEL.md, "Resource limits").

Disassembles libspm_cuda.a with cuobjdump and checks every fused_kernel instantiation:
IMMA.16832.S8.S8 and LDSM present, no HMMA anywhere in the library. Prints the opcode counts.

Usage: tools/check-sass.py [path/to/libspm_cuda.a]
Without an argument it picks the newest libspm_cuda.a under $CARGO_TARGET_DIR (or ./target).
"""
import collections
import glob
import os
import re
import subprocess
import sys


def newest_lib():
    target = os.environ.get("CARGO_TARGET_DIR", "target")
    libs = glob.glob(os.path.join(target, "*", "build", "spm-gpu-*", "out", "libspm_cuda.a"))
    if not libs:
        sys.exit(f"no libspm_cuda.a under {target}; build spm-gpu first or pass the path")
    return max(libs, key=os.path.getmtime)


def main():
    lib = sys.argv[1] if len(sys.argv) > 1 else newest_lib()
    cuobjdump = os.environ.get("CUOBJDUMP", "cuobjdump")
    sass = subprocess.run([cuobjdump, "-sass", lib], check=True, capture_output=True, text=True).stdout
    counts = collections.defaultdict(collections.Counter)
    func = None
    for line in sass.splitlines():
        m = re.search(r"Function : (\S+)", line)
        if m:
            func = m.group(1)
            continue
        m = re.search(r"\b(IMMA|HMMA|QMMA|LDSM|LDGSTS)(\.[A-Z0-9_.]+)?", line)
        if m and func:
            counts[func][m.group(0)] += 1
    ok = True
    print(f"library: {lib}")
    kernels = [f for f in counts if "fused_kernel" in f]
    if not kernels:
        print("no fused_kernel found")
        ok = False
    for f in sorted(counts):
        print(f"{f}: " + ", ".join(f"{op} x{n}" for op, n in sorted(counts[f].items())))
        if any(op.startswith("HMMA") for op in counts[f]):
            print("  FAIL: HMMA present")
            ok = False
    for f in kernels:
        if not any(op.startswith("IMMA.16832.S8.S8") for op in counts[f]):
            print(f"  FAIL: {f} has no IMMA.16832.S8.S8")
            ok = False
        if not any(op.startswith("LDSM") for op in counts[f]):
            print(f"  FAIL: {f} has no LDSM")
            ok = False
    print("SASS gate:", "pass" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
