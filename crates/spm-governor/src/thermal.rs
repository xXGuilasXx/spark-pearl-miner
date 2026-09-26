//! Board temperature from the ACPI thermal zones (`acpitz`). On the DGX Spark every zone in
//! `/sys/class/thermal` is an `acpitz` zone; the governor uses the hottest one.

use std::fs;
use std::io;
use std::path::Path;

/// Where the kernel exposes the thermal zones.
pub const THERMAL_ROOT: &str = "/sys/class/thermal";
/// The zone type we read.
pub const ACPITZ: &str = "acpitz";

/// Parses a sysfs `temp` file (millidegrees Celsius) into °C.
pub fn parse_millidegrees(text: &str) -> Option<f64> {
    let milli: i64 = text.trim().parse().ok()?;
    Some(milli as f64 / 1000.0)
}

/// The hottest `acpitz` zone under `root` (normally [`THERMAL_ROOT`]), in °C. `Ok(None)` when
/// there is no readable `acpitz` zone; zones that fail to read are skipped.
pub fn read_acpitz_max_c(root: &Path) -> io::Result<Option<f64>> {
    let mut max: Option<f64> = None;
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_name().to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let zone = entry.path();
        let is_acpitz = fs::read_to_string(zone.join("type")).is_ok_and(|t| t.trim() == ACPITZ);
        if !is_acpitz {
            continue;
        }
        let Some(t) =
            fs::read_to_string(zone.join("temp")).ok().and_then(|s| parse_millidegrees(&s))
        else {
            continue;
        };
        max = Some(max.map_or(t, |m: f64| m.max(t)));
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spm-governor-thermal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn zone(root: &Path, n: u32, ty: &str, temp: &str) {
        let z = root.join(format!("thermal_zone{n}"));
        fs::create_dir_all(&z).unwrap();
        fs::write(z.join("type"), format!("{ty}\n")).unwrap();
        fs::write(z.join("temp"), format!("{temp}\n")).unwrap();
    }

    #[test]
    fn millidegrees() {
        assert_eq!(parse_millidegrees("47800\n"), Some(47.8));
        assert_eq!(parse_millidegrees("-500"), Some(-0.5));
        assert_eq!(parse_millidegrees("n/a"), None);
    }

    #[test]
    fn hottest_acpitz_zone_wins_and_others_are_ignored() {
        let root = scratch("max");
        zone(&root, 0, "acpitz", "47800");
        zone(&root, 1, "acpitz", "96100");
        zone(&root, 2, "x86_pkg_temp", "99000");
        zone(&root, 3, "acpitz", "garbage");
        fs::create_dir_all(root.join("cooling_device0")).unwrap();
        assert_eq!(read_acpitz_max_c(&root).unwrap(), Some(96.1));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_acpitz_zone_is_none() {
        let root = scratch("none");
        zone(&root, 0, "cpu-thermal", "50000");
        assert_eq!(read_acpitz_max_c(&root).unwrap(), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
