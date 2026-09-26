//! Power profiles. Every number here is also in `docs/_data/facts.toml` (`[power]`) and in
//! `docs/en/POWER-THERMAL.md`; change them together.

use std::fmt;
use std::str::FromStr;

/// A power profile. Ordered from the most to the least conservative: `Eco < Balanced < Max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Profile {
    /// Quiet and cool, far from the power-off region.
    Eco,
    /// The default: 75 W target, 85 W hard stop.
    #[default]
    Balanced,
    /// 88 W target, 92 W hard stop. This is inside the band where a DGX Spark has been seen to
    /// power off (~88–92 W GPU draw), so it needs an explicit acknowledgement.
    Max,
}

/// The limits of one profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileLimits {
    /// GPU power the duty controller steers to, in watts.
    pub target_w: f64,
    /// GPU power that pauses mining for 60 s when exceeded on 3 consecutive samples, in watts.
    pub hard_stop_w: f64,
    /// SM clock the boot-time cap unit should lock to (`nvidia-smi -lgc 300,<mhz>`).
    pub clock_cap_mhz: u32,
}

/// Clock cap recommended when nothing else is known (the value the packaged unit installs).
pub const DEFAULT_CLOCK_CAP_MHZ: u32 = 2000;

const ECO: ProfileLimits = ProfileLimits { target_w: 60.0, hard_stop_w: 70.0, clock_cap_mhz: 1800 };
const BALANCED: ProfileLimits =
    ProfileLimits { target_w: 75.0, hard_stop_w: 85.0, clock_cap_mhz: DEFAULT_CLOCK_CAP_MHZ };
const MAX: ProfileLimits =
    ProfileLimits { target_w: 88.0, hard_stop_w: 92.0, clock_cap_mhz: 2200 };

impl Profile {
    /// Every profile, most conservative first.
    pub const ALL: [Profile; 3] = [Profile::Eco, Profile::Balanced, Profile::Max];

    /// The limits of this profile.
    pub const fn limits(self) -> ProfileLimits {
        match self {
            Profile::Eco => ECO,
            Profile::Balanced => BALANCED,
            Profile::Max => MAX,
        }
    }

    /// Target GPU power in watts.
    pub const fn target_w(self) -> f64 {
        self.limits().target_w
    }

    /// Hard-stop GPU power in watts.
    pub const fn hard_stop_w(self) -> f64 {
        self.limits().hard_stop_w
    }

    /// SM clock cap recommended for this profile. Measured on the author's GB10 (G1 soak, 2026-09-26): at a
    /// 2200 MHz cap the real kernel draws 83–87 W and the SoC reaches 97 °C, above the Balanced target, so
    /// Balanced caps at 2000 MHz and only Max keeps 2200.
    pub const fn recommended_clock_cap_mhz(self) -> u32 {
        self.limits().clock_cap_mhz
    }

    /// Whether selecting this profile needs the explicit acknowledgement flag.
    pub const fn requires_ack(self) -> bool {
        matches!(self, Profile::Max)
    }

    /// One notch more conservative (`Max → Balanced → Eco → Eco`).
    pub const fn step_down(self) -> Profile {
        match self {
            Profile::Max => Profile::Balanced,
            Profile::Balanced | Profile::Eco => Profile::Eco,
        }
    }

    /// Lower-case name used in config files, the marker and the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::Eco => "eco",
            Profile::Balanced => "balanced",
            Profile::Max => "max",
        }
    }

    /// Validates a requested profile against the acknowledgement flag
    /// (`power.max_acknowledged` in the config).
    pub fn select(requested: Profile, max_acknowledged: bool) -> Result<Profile, ProfileError> {
        if requested.requires_ack() && !max_acknowledged {
            Err(ProfileError::MaxNotAcknowledged)
        } else {
            Ok(requested)
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Profile {
    type Err = ProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eco" => Ok(Profile::Eco),
            "balanced" => Ok(Profile::Balanced),
            "max" => Ok(Profile::Max),
            other => Err(ProfileError::Unknown(other.to_string())),
        }
    }
}

/// Why a profile cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// `max` was requested without the acknowledgement flag.
    MaxNotAcknowledged,
    /// Not one of `eco`, `balanced`, `max`.
    Unknown(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileError::MaxNotAcknowledged => f.write_str(
                "the max profile runs at 88 W, inside the band where a DGX Spark can power off; \
                 set power.max_acknowledged = true to use it",
            ),
            ProfileError::Unknown(s) => {
                write!(f, "unknown power profile {s:?} (eco, balanced, max)")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_is_the_default_and_matches_the_architecture() {
        assert_eq!(Profile::default(), Profile::Balanced);
        assert_eq!(Profile::Balanced.target_w(), 75.0);
        assert_eq!(Profile::Balanced.hard_stop_w(), 85.0);
        assert_eq!(Profile::Max.target_w(), 88.0);
        assert_eq!(Profile::Max.hard_stop_w(), 92.0);
        assert_eq!(Profile::Balanced.recommended_clock_cap_mhz(), 2000);
        assert_eq!(Profile::Eco.recommended_clock_cap_mhz(), 1800);
        assert_eq!(Profile::Max.recommended_clock_cap_mhz(), 2200);
    }

    #[test]
    fn every_profile_stops_above_its_target_and_below_the_power_off_band() {
        for p in Profile::ALL {
            let l = p.limits();
            assert!(l.hard_stop_w > l.target_w + 3.0, "{p}");
            assert!(l.hard_stop_w <= 92.0, "{p}");
        }
    }

    #[test]
    fn max_needs_the_acknowledgement() {
        assert_eq!(Profile::select(Profile::Max, false), Err(ProfileError::MaxNotAcknowledged));
        assert_eq!(Profile::select(Profile::Max, true), Ok(Profile::Max));
        assert_eq!(Profile::select(Profile::Balanced, false), Ok(Profile::Balanced));
        assert_eq!(Profile::select(Profile::Eco, false), Ok(Profile::Eco));
    }

    #[test]
    fn step_down_and_parse() {
        assert_eq!(Profile::Max.step_down(), Profile::Balanced);
        assert_eq!(Profile::Balanced.step_down(), Profile::Eco);
        assert_eq!(Profile::Eco.step_down(), Profile::Eco);
        for p in Profile::ALL {
            assert_eq!(p.as_str().parse::<Profile>(), Ok(p));
        }
        assert_eq!(" Balanced ".parse::<Profile>(), Ok(Profile::Balanced));
        assert!(matches!("turbo".parse::<Profile>(), Err(ProfileError::Unknown(_))));
        assert!(Profile::Eco < Profile::Balanced && Profile::Balanced < Profile::Max);
    }
}
