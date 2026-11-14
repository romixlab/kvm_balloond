//! The plan: what each guest's balloon should be, from the host's free memory and the guests' own stats.
//! Pure (no IO), so every case is a test.
//!
//! Each managed guest has three levels, all clamped to `[floor, ceiling]`:
//! - **comfortable**: what it uses (MemTotal - MemFree, hot page cache included) plus `headroom`; it grows by
//!   `grow_step` above what it has when it runs short (MemAvailable under `tight_headroom`, or it swapped in).
//! - **tight**: what it can't drop (MemTotal - MemAvailable) plus `tight_headroom`.
//! - **floor**: guaranteed.
//!
//! The guests get memory from a budget: the host's MemAvailable minus its reserve, plus what the managed
//! guests' QEMUs hold now (their RSS: memory they would give back by shrinking). If the comfortable levels
//! fit, everyone gets theirs. If not, everyone gets tight and the rest goes by shares up to comfortable; if not
//! even that fits, everyone gets the floor and the rest by shares up to tight. A guest shrunk below what it
//! uses deals with it itself: page cache first, then its swap, then its own OOM killer; the host never runs
//! out. Shrinking goes at most `shrink_step` per round so the guest has time to reclaim.

use crate::config::{Guest as Settings, Host as HostSettings};

/// One running guest as the stats show it (all KiB).
#[derive(Clone, Debug, Default)]
pub struct GuestStats {
    pub name: String,
    /// The balloon's current size: what the guest has now.
    pub current: u64,
    /// The domain's maximum memory.
    pub maximum: u64,
    /// The guest's MemTotal (`available` in libvirt's balloon stats).
    pub total: Option<u64>,
    /// The guest's MemFree (`unused`).
    pub free: Option<u64>,
    /// The guest's MemAvailable (`usable`).
    pub usable: Option<u64>,
    /// Pages swapped in since the last round (from the cumulative counter).
    pub swapped_in: u64,
    /// The QEMU process's resident memory on the host.
    pub rss: u64,
    /// The stats are recent (the guest's balloon driver answers).
    pub fresh: bool,
    /// Set through this virtio-mem device instead of the balloon; then `current` is the static RAM plus what
    /// the device has plugged now and `maximum` the static RAM plus the device's size.
    pub virtio_mem: Option<VirtioMem>,
}

/// A guest's virtio-mem device (KiB): memory plugged into it and unplugged again in blocks. Unlike a balloon it
/// works with passed-through devices (VFIO), since QEMU maps and unmaps each block for DMA as it plugs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VirtioMem {
    /// libvirt's alias for the device (`virtiomem0`).
    pub alias: String,
    /// The static RAM the guest boots with (its NUMA cells): never unplugged.
    pub base: u64,
    /// The most the device can plug.
    pub size: u64,
    /// The unit it plugs in.
    pub block: u64,
    /// The size last asked for.
    pub requested: u64,
    /// What the guest has plugged now: behind `requested` while it plugs or unplugs.
    pub plugged: u64,
}

impl VirtioMem {
    /// The requested size that gives the guest `to` in all: whole blocks, rounded up, within the device.
    pub fn requested_for(&self, to: u64) -> u64 {
        let block = self.block.max(1);
        (to.saturating_sub(self.base).div_ceil(block) * block).min(self.size / block * block)
    }
}

/// The host as /proc/meminfo shows it (KiB).
#[derive(Clone, Copy, Debug)]
pub struct HostStats {
    pub available: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub name: String,
    pub from: u64,
    pub to: u64,
    pub why: &'static str,
}

#[derive(Clone, Debug)]
struct Levels {
    floor: u64,
    tight: u64,
    comfortable: u64,
    shares: u64,
    short: bool,
}

/// The guest's levels, or None when it is left alone this round (stale stats).
fn levels(g: &GuestStats, s: &Settings, h: &HostSettings) -> Option<Levels> {
    let (total, free, usable) = match (g.fresh, g.total, g.free, g.usable) {
        (true, Some(t), Some(f), Some(u)) if t > 0 => (t, f, u),
        _ => return None,
    };
    let ceiling = s.ceiling.of(g.maximum).min(g.maximum).max(1);
    let floor = s.floor.of(g.maximum).min(ceiling);
    let clamp = |v: u64| v.clamp(floor, ceiling);
    // the guest's own numbers are inside what it has now; the balloon takes from MemTotal
    let used = total.saturating_sub(free);
    let pinned = total.saturating_sub(usable);
    let tight_room = s.tight_headroom.of(g.maximum);
    let short = usable < tight_room || g.swapped_in > 0;
    let mut comfortable = used + s.headroom.of(g.maximum);
    if short {
        // a quarter of its size at least: a build that suddenly wants gigabytes must not meet its OOM killer
        // while the balloon catches up 512 MB a round
        let step = h.grow_step.of(g.maximum).max(g.current / 4);
        comfortable = comfortable.max(g.current + step);
    }
    let tight = clamp(pinned + tight_room);
    Some(Levels {
        floor,
        tight,
        comfortable: clamp(comfortable).max(tight),
        shares: s.shares.max(1) as u64,
        short,
    })
}

/// Gives each guest `base[i]`, then spreads `extra` by shares, none above `cap[i]` (water-filling).
fn spread(base: &[u64], cap: &[u64], shares: &[u64], mut extra: u64) -> Vec<u64> {
    let mut out = base.to_vec();
    let mut open: Vec<usize> = (0..out.len()).filter(|&i| cap[i] > out[i]).collect();
    while extra > 0 && !open.is_empty() {
        let total: u64 = open.iter().map(|&i| shares[i]).sum();
        let mut given = 0;
        let mut next = Vec::new();
        for &i in &open {
            let want = cap[i] - out[i];
            let part = ((extra as u128 * shares[i] as u128) / total as u128) as u64;
            let take = part
                .min(want)
                .max(if extra > given { 1.min(want) } else { 0 });
            out[i] += take;
            given += take;
            if out[i] < cap[i] {
                next.push(i);
            }
        }
        if given == 0 {
            break;
        }
        extra = extra.saturating_sub(given);
        open = next;
    }
    out
}

/// The balloon changes for this round. `settings(name)` gives each guest's settings; guests with
/// `manage = false` must be left out of `guests` by the caller.
pub fn plan(
    host: HostStats,
    guests: &[GuestStats],
    settings: impl Fn(&str) -> Settings,
    h: &HostSettings,
) -> Vec<Change> {
    let planned: Vec<(&GuestStats, Levels)> = guests
        .iter()
        .filter_map(|g| levels(g, &settings(&g.name), h).map(|l| (g, l)))
        .collect();
    if planned.is_empty() {
        return vec![];
    }
    let reserve = h.reserve.of(0);
    let budget =
        (host.available + planned.iter().map(|(g, _)| g.rss).sum::<u64>()).saturating_sub(reserve);

    let floor: Vec<u64> = planned.iter().map(|(_, l)| l.floor).collect();
    let tight: Vec<u64> = planned.iter().map(|(_, l)| l.tight).collect();
    let comfortable: Vec<u64> = planned.iter().map(|(_, l)| l.comfortable).collect();
    let shares: Vec<u64> = planned.iter().map(|(_, l)| l.shares).collect();
    let sum = |v: &[u64]| v.iter().sum::<u64>();

    let (targets, why) = if sum(&comfortable) <= budget {
        (comfortable.clone(), "room")
    } else if sum(&tight) <= budget {
        (
            spread(&tight, &comfortable, &shares, budget - sum(&tight)),
            "host short",
        )
    } else {
        (
            spread(&floor, &tight, &shares, budget.saturating_sub(sum(&floor))),
            "host very short",
        )
    };

    let mut out = Vec::new();
    for (i, (g, l)) in planned.iter().enumerate() {
        let step = h.shrink_step.of(g.maximum);
        let min_change = h.min_change.of(g.maximum);
        let mut to = targets[i];
        if to + step < g.current {
            to = g.current - step;
        }
        if let Some(v) = &g.virtio_mem {
            to = v.base + v.requested_for(to);
        }
        if to.abs_diff(g.current) < min_change {
            continue;
        }
        let why = if to > g.current {
            if l.short { "guest short" } else { "grow" }
        } else if why == "room" {
            "idle memory back to the host"
        } else {
            why
        };
        out.push(Change {
            name: g.name.clone(),
            from: g.current,
            to,
            why,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Amount;

    const G: u64 = 1024 * 1024;

    fn guest(
        name: &str,
        current: u64,
        maximum: u64,
        used: u64,
        usable: u64,
        rss: u64,
    ) -> GuestStats {
        GuestStats {
            name: name.into(),
            current,
            maximum,
            total: Some(current),
            free: Some(current - used),
            usable: Some(usable),
            swapped_in: 0,
            rss,
            fresh: true,
            virtio_mem: None,
        }
    }

    /// A guest with 2G of static RAM and a virtio-mem device of 6G in 2M blocks, `plugged` of it now.
    fn vmem_guest(plugged: u64, requested: u64, used: u64, usable: u64) -> GuestStats {
        let v = VirtioMem {
            alias: "virtiomem0".into(),
            base: 2 * G,
            size: 6 * G,
            block: 2 * 1024,
            requested,
            plugged,
        };
        GuestStats {
            virtio_mem: Some(v),
            ..guest("tv", 2 * G + plugged, 8 * G, used, usable, 2 * G + plugged)
        }
    }

    fn settings(_: &str) -> Settings {
        Settings {
            floor: Amount::KiB(2 * G),
            ceiling: Amount::Percent(100.0),
            headroom: Amount::KiB(G),
            tight_headroom: Amount::KiB(G / 4),
            shares: 100,
            manage: true,
        }
    }

    fn host() -> HostSettings {
        HostSettings {
            reserve: Amount::KiB(2 * G),
            shrink_step: Amount::KiB(64 * G),
            ..HostSettings::default()
        }
    }

    #[test]
    fn idle_guest_gives_memory_back() {
        // 20G balloon, uses 3G: comfortable is 4G
        let g = guest("b", 20 * G, 20 * G, 3 * G, 18 * G, 20 * G);
        let p = plan(HostStats { available: 4 * G }, &[g], settings, &host());
        assert_eq!(
            p,
            vec![Change {
                name: "b".into(),
                from: 20 * G,
                to: 4 * G,
                why: "idle memory back to the host"
            }]
        );
    }

    #[test]
    fn shrink_is_stepped() {
        let g = guest("b", 20 * G, 20 * G, 3 * G, 18 * G, 20 * G);
        let h = HostSettings {
            shrink_step: Amount::KiB(G),
            ..host()
        };
        let p = plan(HostStats { available: 4 * G }, &[g], settings, &h);
        assert_eq!(p[0].to, 19 * G);
    }

    #[test]
    fn short_guest_grows_when_the_host_has_room() {
        let mut g = guest("b", 4 * G, 20 * G, 3900 * 1024, 100 * 1024, 4 * G);
        g.swapped_in = 10;
        let p = plan(HostStats { available: 10 * G }, &[g], settings, &host());
        assert_eq!(p[0].why, "guest short");
        assert!(p[0].to >= 4 * G + 512 * 1024);
    }

    #[test]
    fn host_short_takes_from_the_one_with_slack_by_shares() {
        // budget = 1G avail + 8G + 8G rss - 2G reserve = 15G; comfortable 9G + 9G = 18G does not fit
        let a = guest("a", 8 * G, 16 * G, 8 * G, 2 * G, 8 * G); // hot cache: can drop to 6G+0.25G
        let b = guest("b", 8 * G, 16 * G, 8 * G, 2 * G, 8 * G);
        let p = plan(HostStats { available: G }, &[a, b], settings, &host());
        let total: u64 = p.iter().map(|c| c.to).sum();
        assert!(total <= 15 * G, "{p:?}");
        assert!(
            p.iter().all(|c| c.to >= 6 * G + G / 4),
            "never under tight here: {p:?}"
        );
        assert_eq!(p[0].to, p[1].to, "equal shares, equal cut");
    }

    #[test]
    fn floors_hold_when_the_host_is_very_short() {
        let a = guest("a", 8 * G, 16 * G, 8 * G, G / 2, 8 * G);
        let p = plan(HostStats { available: 0 }, &[a], settings, &host());
        // budget 6G, tight 7.75G: floor 2G plus the rest up to tight
        assert_eq!(p[0].to, 6 * G);
        assert_eq!(p[0].why, "host very short");
    }

    #[test]
    fn stale_stats_are_left_alone() {
        let mut g = guest("b", 20 * G, 20 * G, 3 * G, 18 * G, 20 * G);
        g.fresh = false;
        assert!(plan(HostStats { available: 0 }, &[g], settings, &host()).is_empty());
    }

    #[test]
    fn small_changes_are_skipped() {
        let g = guest("b", 4 * G, 20 * G, 3 * G, 3 * G, 4 * G);
        assert!(plan(HostStats { available: 10 * G }, &[g], settings, &host()).is_empty());
    }

    #[test]
    fn spread_by_shares_with_caps() {
        let out = spread(&[0, 0], &[10, 100], &[1, 1], 50);
        assert_eq!(out, vec![10, 40]);
    }

    #[test]
    fn virtio_mem_rounds_up_to_whole_blocks_within_the_device() {
        let v = vmem_guest(0, 0, 0, 0).virtio_mem.unwrap();
        assert_eq!(
            v.requested_for(G),
            0,
            "below the static RAM: nothing plugged"
        );
        assert_eq!(v.requested_for(2 * G + 1), 2 * 1024);
        assert_eq!(v.requested_for(3 * G + 3 * 1024), G + 4 * 1024);
        assert_eq!(v.requested_for(20 * G), 6 * G);
        let odd = VirtioMem {
            size: 6 * G + 1000,
            ..v
        };
        assert_eq!(
            odd.requested_for(20 * G),
            6 * G,
            "never past the last whole block"
        );
    }

    #[test]
    fn idle_virtio_mem_guest_unplugs_to_a_block_boundary() {
        // 2G + 4G plugged, uses 1.5G + 100K: comfortable is 2.5G + 100K, rounded up to the next 2M
        let g = vmem_guest(4 * G, 4 * G, 3 * G / 2 + 100, 6 * G);
        let p = plan(HostStats { available: 4 * G }, &[g], settings, &host());
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].from, 6 * G);
        assert_eq!(p[0].to, 5 * G / 2 + 2 * 1024);
        assert_eq!(p[0].why, "idle memory back to the host");
    }

    #[test]
    fn virtio_mem_guest_never_goes_below_its_static_ram() {
        // floor 1G under a 2G base, host very short: the static RAM stays, the device unplugs everything
        let g = vmem_guest(2 * G, 2 * G, 3 * G, G / 2);
        let s = |_: &str| Settings {
            floor: Amount::KiB(G),
            ..settings("tv")
        };
        let h = HostSettings {
            reserve: Amount::KiB(3 * G),
            ..host()
        };
        // budget 4G of QEMU - 3G reserve = 1G: under the static RAM
        let p = plan(HostStats { available: 0 }, &[g], s, &h);
        assert_eq!(p[0].to, 2 * G, "{p:?}");
    }

    #[test]
    fn short_virtio_mem_guest_plugs_more() {
        let mut g = vmem_guest(G, G, 3 * G - 100 * 1024, 100 * 1024);
        g.swapped_in = 5;
        let p = plan(HostStats { available: 10 * G }, &[g], settings, &host());
        assert_eq!(p[0].why, "guest short");
        assert!(p[0].to >= 3 * G + 512 * 1024, "{p:?}");
        assert_eq!((p[0].to - 2 * G) % (2 * 1024), 0, "whole blocks");
    }

    #[test]
    fn unplug_in_progress_plans_from_what_is_plugged() {
        // asked for 1G, still 3G plugged: the plan starts from 5G, not from 3G
        let g = vmem_guest(3 * G, G, 3 * G / 2, 4 * G);
        let p = plan(HostStats { available: 4 * G }, &[g], settings, &host());
        assert_eq!(p[0].from, 5 * G);
        // the same plan as before (2.5G): nothing that would undo the unplug
        assert_eq!(p[0].to, 5 * G / 2);
    }
}
