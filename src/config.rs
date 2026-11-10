//! `/etc/kvm_balloond.toml`: the host's reserve and timing, the defaults every guest gets, and per-guest
//! overrides. Every key is optional; an empty file (or none) gives the defaults below.

use crate::size::Amount;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_PATH: &str = "/etc/kvm_balloond.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub host: Host,
    pub defaults: Guest,
    /// Per guest, by libvirt domain name; any key left out comes from `defaults`.
    pub guest: BTreeMap<String, GuestOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Host {
    /// What the host keeps for itself: guests are planned so that MemAvailable stays above it.
    pub reserve: Amount,
    /// How often stats are read and balloons set when nothing presses.
    #[serde(with = "secs")]
    pub interval: Duration,
    /// The PSI trigger on /proc/pressure/memory that wakes the daemon at once: `some|full STALL_US WINDOW_US`.
    pub psi_trigger: String,
    /// A guest's balloon shrinks by at most this much per round, so the guest has time to reclaim.
    pub shrink_step: Amount,
    /// A guest that runs short grows by at least this much.
    pub grow_step: Amount,
    /// Smaller changes are not made (no churn).
    pub min_change: Amount,
    /// oom_score_adj written for the QEMU of every guest (libvirt's pid files), so the host's OOM killer
    /// takes something else than a whole VM; `none` leaves it alone.
    pub qemu_oom_score_adj: Option<i32>,
    /// The guests' balloon stats period set on each running guest (seconds).
    pub stats_period: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Guest {
    /// Never planned below this: what the guest is guaranteed. The floors of all guests together must fit
    /// in the host's RAM minus its reserve.
    pub floor: Amount,
    /// Never planned above this (capped at the domain's maximum memory).
    pub ceiling: Amount,
    /// Room on top of the memory the guest uses (hot page cache included) when the host has enough.
    pub headroom: Amount,
    /// Room on top of the memory the guest can't drop (MemTotal - MemAvailable) when the host is short.
    pub tight_headroom: Amount,
    /// The guest's weight when memory above the floors is short.
    pub shares: u32,
    /// false: never touched.
    pub manage: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestOverride {
    pub floor: Option<Amount>,
    pub ceiling: Option<Amount>,
    pub headroom: Option<Amount>,
    pub tight_headroom: Option<Amount>,
    pub shares: Option<u32>,
    pub manage: Option<bool>,
}

impl Default for Host {
    fn default() -> Self {
        Host {
            reserve: Amount::KiB(2 * 1024 * 1024),
            interval: Duration::from_secs(2),
            psi_trigger: "some 150000 1000000".into(),
            shrink_step: Amount::KiB(1024 * 1024),
            grow_step: Amount::KiB(512 * 1024),
            min_change: Amount::KiB(64 * 1024),
            qemu_oom_score_adj: Some(-900),
            stats_period: 2,
        }
    }
}

impl Default for Guest {
    fn default() -> Self {
        Guest {
            floor: Amount::Percent(25.0),
            ceiling: Amount::Percent(100.0),
            headroom: Amount::KiB(1024 * 1024),
            tight_headroom: Amount::KiB(256 * 1024),
            shares: 100,
            manage: true,
        }
    }
}

impl Config {
    /// The file at `path`; a missing file is the defaults.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let c: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if c.host.interval < Duration::from_millis(200) {
            bail!("{}: host.interval below 200ms", path.display());
        }
        Ok(c)
    }

    /// The settings of one guest: its overrides on top of the defaults.
    pub fn guest(&self, name: &str) -> Guest {
        let mut g = self.defaults.clone();
        if let Some(o) = self.guest.get(name) {
            if let Some(v) = o.floor {
                g.floor = v
            }
            if let Some(v) = o.ceiling {
                g.ceiling = v
            }
            if let Some(v) = o.headroom {
                g.headroom = v
            }
            if let Some(v) = o.tight_headroom {
                g.tight_headroom = v
            }
            if let Some(v) = o.shares {
                g.shares = v
            }
            if let Some(v) = o.manage {
                g.manage = v
            }
        }
        g
    }
}

mod secs {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    /// `2`, `2.5`, `"2s"` or `"500ms"`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(f64),
            S(String),
        }
        let secs = match Raw::deserialize(d)? {
            Raw::N(n) => n,
            Raw::S(s) => {
                let s = s.trim();
                let (n, div) = if let Some(n) = s.strip_suffix("ms") {
                    (n, 1000.0)
                } else {
                    (s.strip_suffix('s').unwrap_or(s), 1.0)
                };
                n.trim().parse::<f64>().map_err(serde::de::Error::custom)? / div
            }
        };
        if secs.is_nan() || secs <= 0.0 {
            return Err(serde::de::Error::custom("a positive duration"));
        }
        Ok(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_on_defaults() {
        let c: Config = toml::from_str(
            r#"
            [host]
            reserve = "3G"
            interval = "500ms"
            [defaults]
            floor = "2G"
            [guest.alma-1b]
            floor = "8G"
            shares = 50
            [guest.git-2]
            manage = false
            "#,
        )
        .unwrap();
        assert_eq!(c.host.reserve, Amount::KiB(3 * 1024 * 1024));
        assert_eq!(c.host.interval, Duration::from_millis(500));
        let b = c.guest("alma-1b");
        assert_eq!(b.floor, Amount::KiB(8 * 1024 * 1024));
        assert_eq!(b.shares, 50);
        assert_eq!(c.guest("other").floor, Amount::KiB(2 * 1024 * 1024));
        assert!(!c.guest("git-2").manage);
    }

    #[test]
    fn unknown_keys_refused() {
        assert!(toml::from_str::<Config>("[host]\nreserv = \"2G\"\n").is_err());
    }

    #[test]
    fn the_example_is_the_defaults() {
        let c: Config = toml::from_str(include_str!("../dist/kvm_balloond.toml")).unwrap();
        let d = Config::default();
        assert_eq!(c.host.reserve, d.host.reserve);
        assert_eq!(c.host.interval, d.host.interval);
        assert_eq!(c.defaults.floor, d.defaults.floor);
        assert_eq!(c.defaults.headroom, d.defaults.headroom);
    }
}
