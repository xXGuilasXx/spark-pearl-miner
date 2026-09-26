//! `running.marker`: unclean-shutdown detection and the profile step-down.

use std::fs;
use std::path::PathBuf;

use spm_governor::marker::{
    begin_run, clear_marker, plan_start, read_marker, write_marker, MarkerInfo, MARKER_FILE,
};
use spm_governor::Profile;

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("spm-governor-marker-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn marker(profile: Option<Profile>) -> MarkerInfo {
    MarkerInfo { profile, started_unix_s: Some(1_790_000_000), pid: Some(4242) }
}

#[test]
fn clean_previous_run_keeps_the_configured_profile() {
    for p in Profile::ALL {
        let plan = plan_start(p, None);
        assert_eq!(plan.profile, p);
        assert!(plan.alert.is_none() && plan.unclean.is_none());
    }
}

#[test]
fn unclean_previous_run_steps_down_one_notch_and_alerts() {
    let cases = [
        (Profile::Balanced, Profile::Balanced, Profile::Eco),
        (Profile::Max, Profile::Max, Profile::Balanced),
        (Profile::Eco, Profile::Eco, Profile::Eco),
        // Configured more conservative than the stepped-down profile: the configured one wins.
        (Profile::Eco, Profile::Max, Profile::Eco),
        // Configured Max, but the crashed run was already stepped down to Balanced.
        (Profile::Max, Profile::Balanced, Profile::Eco),
    ];
    for (configured, ran_with, expected) in cases {
        let prev = marker(Some(ran_with));
        let plan = plan_start(configured, Some(&prev));
        assert_eq!(plan.profile, expected, "configured {configured}, ran with {ran_with}");
        let alert = plan.alert.expect("alert");
        assert!(alert.contains("did not stop cleanly"));
        assert!(alert.contains(&format!("profile {ran_with}")));
        assert_eq!(plan.unclean, Some(prev));
    }
}

#[test]
fn unreadable_marker_still_counts_as_unclean() {
    let plan = plan_start(Profile::Balanced, Some(&MarkerInfo::parse("\0\u{fffd}garbage")));
    assert_eq!(plan.profile, Profile::Eco);
    assert!(plan.alert.unwrap().contains("unknown time"));
}

#[test]
fn encode_parse_round_trip_and_leniency() {
    let m = marker(Some(Profile::Max));
    assert_eq!(MarkerInfo::parse(&m.encode()), m);
    let partial = MarkerInfo::parse("pid=12\nprofile=turbo\nfuture_key=1\nstarted_unix_s=x\n");
    assert_eq!(partial, MarkerInfo { profile: None, started_unix_s: None, pid: Some(12) });
    assert_eq!(MarkerInfo::parse(""), MarkerInfo::default());
}

#[test]
fn clean_stop_then_start_is_clean() {
    let dir = scratch("clean");
    let path = dir.join(MARKER_FILE);
    let plan = begin_run(&path, Profile::Balanced, 100, 1).unwrap();
    assert_eq!(plan.profile, Profile::Balanced);
    assert!(plan.alert.is_none());
    assert_eq!(read_marker(&path).unwrap().unwrap().profile, Some(Profile::Balanced));
    clear_marker(&path).unwrap();
    assert!(read_marker(&path).unwrap().is_none());
    clear_marker(&path).unwrap(); // idempotent
    let plan = begin_run(&path, Profile::Balanced, 200, 2).unwrap();
    assert!(plan.alert.is_none());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn repeated_kills_keep_stepping_down_until_a_clean_stop() {
    // What a SIGKILL (or a power-off) looks like on disk: the marker is never removed.
    let dir = scratch("kills");
    let path = dir.join(MARKER_FILE);
    let seen: Vec<Profile> = (0..4)
        .map(|i| begin_run(&path, Profile::Max, 1000 + i, 10 + i as u32).unwrap().profile)
        .collect();
    assert_eq!(seen, [Profile::Max, Profile::Balanced, Profile::Eco, Profile::Eco]);
    // The marker records the profile actually used.
    assert_eq!(read_marker(&path).unwrap().unwrap().profile, Some(Profile::Eco));
    clear_marker(&path).unwrap();
    assert_eq!(begin_run(&path, Profile::Max, 2000, 20).unwrap().profile, Profile::Max);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_marker_file_from_a_torn_write_is_unclean() {
    let dir = scratch("empty");
    let path = dir.join(MARKER_FILE);
    fs::write(&path, b"").unwrap();
    let plan = begin_run(&path, Profile::Balanced, 1, 1).unwrap();
    assert_eq!(plan.profile, Profile::Eco);
    assert!(plan.alert.is_some());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_marker_creates_the_state_directory() {
    let dir = scratch("mkdir");
    let path = dir.join("state").join("spark-pearl-miner").join(MARKER_FILE);
    write_marker(&path, &marker(Some(Profile::Eco))).unwrap();
    assert_eq!(read_marker(&path).unwrap(), Some(marker(Some(Profile::Eco))));
    assert!(!path.with_extension("marker.tmp").exists());
    fs::remove_dir_all(&dir).unwrap();
}
