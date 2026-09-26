#!/usr/bin/env python3
"""CI guard: README.md, README.pt-BR.md and docs/*/FEE*.md must state exactly the fee constants
defined in crates/spm-fee/src/lib.rs (rate, wallet, first dev pool host:port, worker, slice)."""
import re, sys, pathlib
root = pathlib.Path(__file__).resolve().parents[1]
src = (root / "crates/spm-fee/src/lib.rs").read_text()
bps = int(re.search(r'pub const FEE_BPS: u32 = (\d+);', src).group(1))
wallet = re.search(r'pub const DEV_WALLET: &str = "([a-z0-9]+)";', src).group(1)
worker = re.search(r'pub const DEV_WORKER: &str = "([^"]+)";', src).group(1)
slice_s = int(re.search(r'pub const SLICE_SECS: u64 = (\d+);', src).group(1))
host, port = re.search(r'DEV_POOLS: &\[\(&str, u16, bool\)\] = &\[\s*\("([^"]+)", (\d+), (?:true|false)\)', src).groups()
expected = f'dev fee {bps//100}.{bps%100:02d}% → {wallet} @ {host}:{port} (HeroMiners), worker "{worker}", {slice_s} s slices, only while mining'
expected_pt = f'dev fee {bps//100}.{bps%100:02d}% → {wallet} @ {host}:{port} (HeroMiners), worker "{worker}", fatias de {slice_s} s, só enquanto minera'
checks = {"README.md": expected, "README.pt-BR.md": expected_pt}
for p in ("docs/en/FEE.md", "docs/pt-BR/TAXA.md"):
    if (root / p).exists():
        checks[p] = expected if p.startswith("docs/en") else expected_pt
bad = 0
for rel, line in checks.items():
    text = (root / rel).read_text()
    if line not in text:
        bad += 1; print(f"MISMATCH {rel}: expected line not found:\n  {line}")
    # no other prl1 address may appear as a fee wallet
    for m in set(re.findall(r'prl1[a-z0-9]{59}', text)) - {wallet}:
        if re.search(r'(dev fee|taxa)[^\n]*' + re.escape(m), text):
            bad += 1; print(f"MISMATCH {rel}: another wallet appears in a fee line: {m}")
print("fee constants:", bps, "bps |", wallet[:12] + "…", "|", f"{host}:{port}", "|", worker, "|", slice_s, "s")
sys.exit(1 if bad else 0)
