//! The power governor wired into the daemon, with a fake telemetry source (virtual time) and a
//! fake external worker: SetDuty ramp, over-power pause and resume after 60 s, a fault signature
//! that stops mining (status, /api/v1/gpu, SSE), the running.marker step-down, and Max refused
//! without the acknowledgement. No GPU is touched.

mod common;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spm::paths::Paths;
use spm::power::{GpuReading, TelemetryChoice, TelemetrySource};
use spm_api::{ApiEvent, Backend};
use spm_ipc::ToWorker;
use spm_mockpool::{MockConfig, MockPool};

/// Readings pushed by the test; `at` carries the virtual time.
struct FakeSource {
    rx: Option<Receiver<GpuReading>>,
}

impl TelemetrySource for FakeSource {
    fn name(&self) -> String {
        "fake".into()
    }
    fn period(&self) -> Duration {
        Duration::ZERO
    }
    fn read(&mut self) -> Result<GpuReading, String> {
        let rx = self.rx.as_ref().ok_or("no feed")?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())
    }
}

fn fake_telemetry() -> (TelemetryChoice, Sender<GpuReading>) {
    let (tx, rx) = channel();
    let slot = Arc::new(Mutex::new(Some(rx)));
    let make = move || -> Box<dyn TelemetrySource> { Box::new(FakeSource { rx: slot.lock().unwrap().take() }) };
    (TelemetryChoice::Custom(Arc::new(make)), tx)
}

struct Feed {
    tx: Sender<GpuReading>,
    t_ms: u64,
}

impl Feed {
    /// `n` samples 100 ms apart (virtual), paced at 2 ms of real time.
    async fn samples(&mut self, n: usize, power_w: f64, sm_mhz: u32) {
        for _ in 0..n {
            self.t_ms += 100;
            let r = GpuReading {
                power_w,
                temp_gpu_c: 60.0,
                sm_mhz,
                event_reasons: Some(0),
                acpitz_c: Some(50.0),
                at: Some(Duration::from_millis(self.t_ms)),
            };
            self.tx.send(r).unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn is_duty(m: &ToWorker) -> bool {
    matches!(m, ToWorker::SetDuty { .. })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn governor_duty_trips_and_faults_reach_the_worker() {
    let root = temp_root("power");
    let pool = MockPool::start(MockConfig::trivial()).await.unwrap();
    let cfg = external_config(vec![mock_slot("mock", pool.port())]);
    let (choice, tx) = fake_telemetry();
    let d = start_daemon_with(&root, &cfg, |o| o.governor = choice).await;
    let mut events = d.bridge.subscribe();
    let paths = Paths::under(&root);
    d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    let sock = paths.worker_sock();
    let mut w = tokio::task::spawn_blocking(move || FakeWorker::attach(&sock, true)).await.unwrap();

    // The worker starts at the governor's 10 % before it is resumed.
    let (_, duty) = w.next(Duration::from_secs(5), is_duty).await.expect("SetDuty");
    assert_eq!(duty, ToWorker::SetDuty { pct: 10 });
    w.next(Duration::from_secs(10), |m| *m == ToWorker::Resume).await.expect("Resume");
    assert!(d.wait_for(Duration::from_secs(5), |s| s.status.worker.state == "hashing").await.is_some());

    // 30 W against the 75 W target: the duty ramps (2 % per 100 ms sample at most).
    let mut feed = Feed { tx, t_ms: 0 };
    feed.samples(10, 30.0, 2200).await;
    let mut ramp = Vec::new();
    while let Some((_, ToWorker::SetDuty { pct })) = w.next(Duration::from_millis(500), is_duty).await {
        ramp.push(pct);
        if ramp.len() == 9 {
            break;
        }
    }
    assert!(ramp.len() >= 5 && ramp.windows(2).all(|p| p[1] > p[0] && p[1] - p[0] <= 2), "{ramp:?}");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.power.state == "running").await.expect("governor running");
    assert_eq!((s.status.power.source.as_str(), s.status.power.profile.as_str()), ("fake", "balanced"));
    assert_eq!((s.status.power.target_w, s.status.power.hard_stop_w), (75.0, 85.0));

    // Three samples over the 85 W hard stop: paused, alert, SSE event.
    feed.samples(3, 90.0, 2200).await;
    w.next(Duration::from_secs(2), |m| *m == ToWorker::Pause).await.expect("Pause on the trip");
    let s = d
        .wait_for(Duration::from_secs(2), |s| s.status.power.state == "tripped" && s.status.pause_reason.as_deref() == Some("power_trip"))
        .await
        .expect("tripped");
    assert_eq!(s.status.power.trip.as_ref().unwrap().code, "over_power");
    assert!(s.status.alerts.iter().any(|a| a.msg.contains("85 W hard stop")), "{:?}", s.status.alerts);
    // Held for 60 s of (virtual) time, then resumed at the minimum duty (already sent when the
    // controller backed off above the stop).
    feed.samples(599, 15.0, 2200).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!w.pending().contains(&ToWorker::Resume), "resumed before 60 s: {:?}", w.pending());
    feed.samples(2, 15.0, 2200).await;
    w.next(Duration::from_secs(2), |m| *m == ToWorker::Resume).await.expect("Resume after the trip");
    let before_resume = w.frames()[..w.cursor - 1].iter().rev().find_map(|(_, m)| match m {
        ToWorker::SetDuty { pct } => Some(*pct),
        _ => None,
    });
    assert_eq!(before_resume, Some(10), "a resumed worker restarts at 10 %");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.power.trip.is_none() && s.status.pause_reason.is_none()).await;
    assert!(s.expect("trip cleared").status.power.duty_pct <= 12);

    // Pinned at 100 W for more than 10 s: over-power pause first, then the thermal-cap fault
    // stops mining and releases the worker.
    feed.samples(120, 100.0, 1500).await;
    w.next(Duration::from_secs(3), |m| *m == ToWorker::Release).await.expect("Release on the fault");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.power.state == "fault").await.expect("fault state");
    assert_eq!(s.status.pause_reason.as_deref(), Some("power_fault"));
    assert_eq!(s.status.power.fault.as_ref().unwrap().signature, "thermal_cap_100w");
    assert!(s.status.power.trips_total >= 2);
    assert!(s.status.alerts.iter().any(|a| a.level == "error" && a.msg.contains("thermal cap")), "{:?}", s.status.alerts);
    // Exposed in /api/v1/gpu.
    let (cookie, _) = login(d.api_port.unwrap(), &d.token).await;
    let (code, _, body) = http(d.api_port.unwrap(), "GET", "/api/v1/gpu", &[("Cookie", &cookie)], None).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["power"]["state"], "fault", "{v}");
    assert_eq!(v["power"]["fault"]["signature"], "thermal_cap_100w");
    // SSE saw the trip, the resume and the fault.
    let mut kinds = Vec::new();
    loop {
        match events.try_recv() {
            Ok(ApiEvent::Power { kind, .. }) => kinds.push(kind),
            Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    for k in ["trip", "resume", "fault"] {
        assert!(kinds.iter().any(|x| x == k), "no {k} event in {kinds:?}");
    }
    // Start clears the fault.
    d.bridge.control_op(spm_api::ControlOp::Start).await.unwrap();
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.power.fault.is_none()).await.expect("fault cleared");
    assert_ne!(s.status.pause_reason.as_deref(), Some("power_fault"));
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclean_start_steps_the_profile_down_and_a_clean_stop_clears_the_marker() {
    let root = temp_root("marker");
    let paths = Paths::under(&root);
    paths.ensure().unwrap();
    let marker = paths.state_dir.join("running.marker");
    // What a run killed by a power-off leaves behind.
    std::fs::write(&marker, "profile=balanced\nstarted_unix_s=1790000000\npid=4242\n").unwrap();
    let cfg = test_config(vec![mock_slot("mock", 9)]);
    let d = start_daemon(&root, &cfg).await;
    let s = d.snapshot();
    let p = &s.status.power;
    assert_eq!((p.profile.as_str(), p.configured_profile.as_str(), p.stepped_down), ("eco", "balanced", true));
    assert_eq!((p.target_w, p.hard_stop_w, p.clock_cap.cap_mhz), (60.0, 70.0, 1800));
    assert!(p.unclean_start.as_deref().unwrap_or_default().contains("did not stop cleanly"));
    assert!(s.status.alerts.iter().any(|a| a.msg.contains("did not stop cleanly")));
    let text = std::fs::read_to_string(&marker).unwrap();
    assert!(text.contains("profile=eco") && text.contains(&format!("pid={}", std::process::id())), "{text}");
    d.shutdown().await;
    assert!(!marker.exists(), "a clean stop removes the marker");
    // The next start is clean: the configured profile again.
    let d = start_daemon(&root, &cfg).await;
    let p = d.snapshot().status.power.clone();
    assert_eq!((p.profile.as_str(), p.stepped_down, p.unclean_start), ("balanced", false, None));
    assert!(marker.exists());
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_is_refused_without_the_acknowledgement_and_the_status_api_shows_the_governor() {
    let root = temp_root("maxack");
    let cfg = test_config(vec![mock_slot("mock", 9)]);
    let d = start_daemon(&root, &cfg).await;
    let port = d.api_port.unwrap();
    let (cookie, csrf) = login(port, &d.token).await;

    // The status API carries the governor and the coexistence state.
    let (code, _, body) = http(port, "GET", "/api/v1/status", &[("Cookie", &cookie)], None).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let p = &v["power"];
    assert_eq!((p["state"].as_str(), p["profile"].as_str()), (Some("off"), Some("balanced")), "{p}");
    assert_eq!((p["target_w"].as_f64(), p["hard_stop_w"].as_f64()), (Some(75.0), Some(85.0)));
    for k in ["duty_pct", "trips_total", "clock_cap", "trip", "last_trip", "fault", "telemetry", "source"] {
        assert!(p.get(k).is_some(), "power.{k} missing: {p}");
    }
    assert_eq!(p["clock_cap"]["cap_mhz"], 2000);
    let c = &v["coexist"];
    assert_eq!((c["mode"].as_str(), c["gate"].as_str()), (Some("exclusive"), Some("mine")), "{c}");
    assert_eq!(c["memory"]["state"], "ok");
    assert!(c["handshake"].get("acks").is_some());

    // Max without the typed acknowledgement: refused, nothing changes.
    let mut max = cfg.clone();
    max.power.profile = spm_api::config::PowerProfile::Max;
    let (code, _, out) =
        http(port, "PUT", "/api/v1/config", &[("Cookie", &cookie), ("X-SPM-CSRF", &csrf)], Some(&serde_json::to_string(&max).unwrap())).await;
    assert_eq!(code, 422, "{out}");
    assert!(out.contains("max_not_acknowledged"), "{out}");
    assert_eq!(d.snapshot().status.power.profile, "balanced");
    // With it: in force at once.
    max.power.max_acknowledged = true;
    let (code, _, out) =
        http(port, "PUT", "/api/v1/config", &[("Cookie", &cookie), ("X-SPM-CSRF", &csrf)], Some(&serde_json::to_string(&max).unwrap())).await;
    assert_eq!(code, 200, "{out}");
    let s = d.wait_for(Duration::from_secs(2), |s| s.status.power.profile == "max").await.expect("max in force");
    assert_eq!((s.status.power.target_w, s.status.power.hard_stop_w), (88.0, 92.0));
    d.shutdown().await;

    // A config file with max and no acknowledgement does not start the daemon.
    let paths = Paths::under(&root);
    let mut bad = cfg.clone();
    bad.power.profile = spm_api::config::PowerProfile::Max;
    std::fs::write(paths.config_file(), bad.to_toml()).unwrap();
    let mut opts = spm::daemon::DaemonOptions::new(paths.clone());
    opts.api_port_override = Some(0);
    opts.telemetry = false;
    opts.governor = TelemetryChoice::Off;
    let e = spm::daemon::start(opts).await.err().expect("refused");
    assert!(e.to_string().contains("max_acknowledged"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}
