# Power and thermal

How spark-pearl-miner keeps a DGX Spark out of its power-off region. Code: `crates/spm-governor`; boot unit: `packaging/systemd/system/spark-pearl-clockcap.service`; soak logger: `bench/soak-log.sh`. Numbers: `docs/_data/facts.toml` (`[power]`). Portuguese: [`docs/pt-BR/ENERGIA-TERMICA.md`](../pt-BR/ENERGIA-TERMICA.md).

## 1. The problem

Some DGX Spark units **power off hard** (no shutdown, nothing in the logs) under sustained GPU load at roughly **88–92 W of GPU draw**. NVIDIA acknowledged it as a known issue on 2026-07-27 and has shipped no fix; the author's unit already runs the newest firmware (see [VIABILITY](VIABILITY.md) §4). Public reports on the NVIDIA Developer Forums:

- [Hard power-off under sustained GPU load at ~90W, persists after full platform firmware update](https://forums.developer.nvidia.com/t/hard-power-off-under-sustained-gpu-load-at-90w-persists-after-full-platform-firmware-update/378315)
- [DGX Spark (GB10) reproducibly hard powers-off under GPU load](https://forums.developer.nvidia.com/t/dgx-spark-gb10-reproducibly-hard-powers-off-under-gpu-load-fully-updated-zero-crash-capture/373251)

What we have to work with on GB10:

- **No software power limit.** `nvidia-smi -pl` is not supported.
- **An SM clock lock**, `nvidia-smi -lgc 300,<MHz>`. It needs root and is lost on reboot.
- **Telemetry** through NVML (power, SM clock, GPU temperature, clock event reasons) and the ACPI thermal zones (`acpitz` in `/sys/class/thermal`), all readable without root.
- **Our own duty cycle.** The worker can compute for a fraction of each period and idle for the rest.

For scale: the register-only tensor-core peak (MB1, [BENCHMARKS](BENCHMARKS.md)) draws 51 W at stock clocks and 34 W at 2200 MHz. A real mining kernel adds shared-memory and L2 traffic (a closed-source Spark miner reports ~99 W), so without control we would sit right in the power-off band. With the vLLM model resident and idle, this unit idles at 15 W and 2424 MHz.

## 2. Profiles

| Profile | Target | Hard stop | Recommended clock cap | Notes |
|---|---|---|---|---|
| Eco | 60 W | 70 W | 2000 MHz | Quiet and cool, far from the band. |
| **Balanced** (default) | **75 W** | **85 W** | **2200 MHz** | The default everywhere. |
| Max | 88 W | 92 W | 2200 MHz | Inside the power-off band. Refused unless `power.max_acknowledged = true`. |

The target is what the controller steers to. The hard stop pauses mining (section 4). The clock cap is the boot unit's value.

## 3. Clock cap (optional, root once)

Locking the SM clock at 2200 MHz costs ~9 % of peak throughput (96.0 vs 108.6 T-MAC/s in MB1) and is the main safety net: even if the governor fails, the GPU cannot boost into the band. The miner works without it, but on a DGX Spark I recommend it.

```bash
packaging/install-clockcap.sh                 # prints the exact sudo commands, changes nothing
sudo packaging/install-clockcap.sh --apply    # runs them (install the unit, daemon-reload, enable --now)
sudo packaging/install-clockcap.sh --apply --mhz 2000   # Eco
sudo packaging/uninstall-clockcap.sh --apply  # disable, remove, restore default clocks (nvidia-smi -rgc)
```

The unit is a `Type=oneshot` with `RemainAfterExit=yes`: `ExecStart=/usr/bin/nvidia-smi -lgc 300,2200` at boot (after `nvidia-persistenced`), `ExecStop=/usr/bin/nvidia-smi -rgc`. It is deliberately not ordered after `multi-user.target`, because that target pulls it in and the ordering would be a cycle.

**Detection.** NVML has no getter for locked clocks, but the cap is visible in the clocks: with it installed the SM clock never goes above 2200 MHz, idle or loaded (uncapped, this unit idles at 2424 MHz). The governor reports *not capped* as soon as any sample exceeds the cap by more than 30 MHz, and *capped* after 30 s of load at ≥ 90 % duty without exceeding it. The verdict is in the status API (`power.clock_cap`: `unknown`, `capped` or `uncapped`, with the highest clock seen).

## 4. Governor behaviour

The daemon samples at **10 Hz**: NVML power, SM clock, GPU temperature and clock event reasons, plus the hottest `acpitz` zone. NVML never creates a CUDA context, so the daemon never appears as a compute process and needs no root. `libnvidia-ml.so` is loaded at runtime (cargo feature `nvml`, on by default); when it cannot be loaded the daemon falls back to `nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active --format=csv,noheader` at 2 Hz. With neither, the governor cannot work: the daemon raises an alert and keeps a real GPU worker from running (the CPU simulation still runs). A telemetry gap of more than 3 s holds the worker the same way until readings come back.

**How the daemon applies it.** Every duty change goes to the worker as an IPC `SetDuty` frame; a new or resumed worker gets the current duty before it resumes, and after more than 30 s without computing it restarts from 10 %. A trip pauses the worker (IPC `Pause`, CUDA context kept, no idle release) and resumes it when the governor lets go. A fault signature stops mining: alert, the worker is released, and mining stays stopped (also across a daemon restart) until the user presses Start. Everything is in the API: `status.power` and `GET /api/v1/gpu` (`power`) carry the profile in force, target, hard stop, duty, telemetry source and last reading, clock cap, the trip in force with its resume countdown, the trip count, the last trip and a latched fault; the SSE stream sends a `power` event on every trip, resume, fault, profile change and telemetry change, and the paused state shows `pause_reason` `power_trip`, `power_fault` or `no_telemetry`.

**Duty control.** A PI controller sets the worker's duty cycle between 10 % and 100 %: 0.5 % per watt of error (proportional) and 0.5 % per watt-second (integral), in velocity form so that clamping the duty is the anti-windup. The duty rises at most 20 %/s (10 → 100 % in 4.5 s) and falls without limit. Above 78 °C GPU the target is lowered by 3 W per °C, so the controller backs off before the temperature trip. In a first-order power/thermal plant model (`spm_governor::sim`) it holds every profile's target within **±3 W** over 30 simulated minutes (worst reading 1.2 W off, noise included), with a 4× slower or 2× noisier power reading, after a 12 % heavier job and with a hotter room. On hardware this is still to be shown (TODO M11).

**Trips.**

| Condition | Action | Resume |
|---|---|---|
| GPU power > hard stop on 3 consecutive samples | pause | after 60 s, at 10 % duty, ramping |
| GPU temperature > 83 °C | pause | after 60 s and GPU ≤ 78 °C and acpitz ≤ 90 °C |
| hottest `acpitz` > 95 °C | pause | same as above |
| a fault signature (section 6) | **stop** and alert | only after the user clears it |

Power and temperatures count even while our worker is idle: if another process already has the GPU that hot or that loaded, we must not add to it.

**Other rules.** Switching to a lower profile cuts the duty in proportion at once and keeps the previous hard stop for 2 s while the power comes down, so a change from the GUI does not trip the new stop. After more than 30 s without computing (yielding to vLLM, released) the next start ramps from 10 % again. A hole of more than 2 s in the telemetry restarts the ramp and the fault windows.

## 5. Unclean shutdown: `running.marker`

When the daemon starts, before any worker computes, it writes `$XDG_STATE_HOME/spark-pearl-miner/running.marker` (profile, start time, pid; fsync'd), and it removes it on a clean stop (SIGTERM, `systemctl --user stop`). If the marker is already there at start, the previous run ended without cleaning up: a crash, a `SIGKILL`, or a power-off. The new run then uses **one profile lower** than the one that was running (Max → Balanced → Eco → Eco, never above the configured one) and raises an alert; the status API shows it (`power.stepped_down`, `power.unclean_start`). The step-down lasts for that run: a higher profile chosen meanwhile stays capped, and after a clean stop the configured profile is used again. Repeated unclean stops keep stepping down. A profile change while running rewrites the marker with the profile in force.

## 6. Fault signatures

These patterns point at hardware or firmware problems, not at load. The governor stops mining and raises an alert instead of trying to work around them.

| Signature | Pattern | Meaning |
|---|---|---|
| `usb_pd` | SM clock < 850 MHz at 5–15 W under load (duty ≥ 50 %) for more than 10 s | USB-PD power negotiation failed; the unit runs on a fallback power budget. |
| `safety_mode` | power pinned at 30 ± 3 W (spread ≤ 4 W) with SM clock < 1400 MHz under load for more than 30 s | Firmware safety mode, treated by NVIDIA support as an RMA symptom. |
| `thermal_cap_100w` | power pinned at 100 ± 4 W (spread ≤ 4 W) for more than 10 s, whatever our worker is doing | The 100 W thermal cap is holding the GPU. Since the over-power trip pauses our worker after 0.3 s, seeing it means something else loads the GPU that hard or the pause failed. |

## 7. What to do on RMA symptoms

1. Leave mining stopped. The governor has already stopped it; clearing the alert does not fix the unit.
2. Power-cycle the Spark with the **original** USB-C power supply and cable plugged straight into the wall (no hub, dock or extension). This is the usual cure for `usb_pd`.
3. Check airflow and dust: vents clear, no stack of devices on top, room temperature. Dust has been reported as a cause of shutdowns under load.
4. Collect evidence: `sudo nvidia-bug-report.sh`, the soak log (`bench/soak-log.sh`, section 8), `journalctl -b -1` after a power-off, and the miner's diagnostics export.
5. If `safety_mode` or `usb_pd` comes back after a cold power-cycle, or power-offs continue at the Eco profile with the clock cap in place, open a case with NVIDIA support and attach the above. Do not keep mining on that unit.

## 8. Soaks (gate G1)

`bench/soak-log.sh` logs every 10 s: `nvidia-smi --query-gpu=timestamp,power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active` plus the hottest `acpitz`, to `docs/benchmarks/soak-<UTC>.csv`. It needs no root and does not touch the GPU. On exit it prints max power, min clock, max temperatures, the clock event reasons seen, every gap longer than 20 s between rows and every session that ended without its clean-end line (both mean a suspected power-off). After a power-off, `bench/soak-log.sh --summarize <file>` on the next boot.

Plan, in an announced GPU window with vLLM stopped: a clock ladder 1800–2200 MHz at 10 min per step, then 60 min and 24 h at the default profile. Pass: no power-off, zero compute mismatches, ≥ ~70 TH/s credited. Results go here when they exist; **none yet** (the GPU worker is M5).

For a quick look at what the governor sees: `cargo run --release -p spm-governor --features nvml --example telemetry -- 10`.
