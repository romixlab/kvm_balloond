//! kvm_balloond: lends memory between the KVM guests of one host through their balloons. See README.md.

mod config;
mod damp;
mod host;
mod libvirt;
mod policy;
mod size;

use anyhow::Result;
use config::Config;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str = "\
kvm_balloond: lends memory between the KVM guests of this host through their balloons

Usage: kvm_balloond [--config PATH] [--dry-run] [--once]

  --config PATH  the settings (default /etc/kvm_balloond.toml; missing = defaults)
  --dry-run      plan and log, change nothing
  --once         one round: print the guests and the plan, then exit (implies --dry-run)
";

/// What the daemon remembers of a running guest between rounds.
struct Known {
    /// left alone: a passed-through device pins its RAM and there is no virtio-mem device, or `manage = false`
    skip: Option<&'static str>,
    /// set through its virtio-mem device instead of the balloon
    virtio_mem: bool,
    last_swap_in: Option<u64>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("kvm_balloond: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut path = PathBuf::from(config::DEFAULT_PATH);
    let (mut dry, mut once) = (false, false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => {
                path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?
            }
            "--dry-run" => dry = true,
            "--once" => (dry, once) = (true, true),
            "--version" | "-V" => {
                println!("kvm_balloond {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{HELP}"),
        }
    }
    let cfg = Config::load(&path)?;
    let pressure = if once {
        None
    } else {
        match host::Pressure::open(&cfg.host.psi_trigger) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("no PSI trigger, every {:?} only: {e:#}", cfg.host.interval);
                None
            }
        }
    };
    eprintln!(
        "kvm_balloond {} {}: reserve {}, every {:?} or on memory pressure",
        env!("CARGO_PKG_VERSION"),
        if dry { "(dry run)" } else { "" },
        size::show(cfg.host.reserve.of(0)),
        cfg.host.interval
    );
    let mut known: HashMap<String, Known> = HashMap::new();
    let mut damper = damp::Damper::default();
    let start = Instant::now();
    if once {
        // a first quiet round sets the guests' stats period, then the stats need one period to be fresh
        for raw in libvirt::domstats()? {
            let k = first_seen(&cfg, &raw.name, true);
            known.insert(raw.name, k);
        }
        std::thread::sleep(Duration::from_secs(cfg.host.stats_period as u64 + 1));
    }
    loop {
        let started = Instant::now();
        if let Err(e) = round(&cfg, &mut known, &mut damper, start.elapsed(), dry, once) {
            eprintln!("round failed: {e:#}");
        }
        if once {
            return Ok(());
        }
        let wait = cfg.host.interval.saturating_sub(started.elapsed());
        match &pressure {
            Some(p) => {
                if p.wait(wait)? {
                    // a burst of stalls fires many times: let the last change take effect first
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            None => std::thread::sleep(wait),
        }
    }
}

fn round(
    cfg: &Config,
    known: &mut HashMap<String, Known>,
    damper: &mut damp::Damper,
    elapsed: Duration,
    dry: bool,
    once: bool,
) -> Result<()> {
    let raws = libvirt::domstats()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let running: HashSet<&str> = raws.iter().map(|r| r.name.as_str()).collect();
    known.retain(|name, _| running.contains(name.as_str()));

    let mut managed = Vec::new();
    for raw in &raws {
        let k = known
            .entry(raw.name.clone())
            .or_insert_with(|| first_seen(cfg, &raw.name, dry));
        // a virtio-mem guest's size is what its device has plugged now: read every round
        let (vmem, vmem_failed) = match k.virtio_mem {
            true => match libvirt::dumpxml(&raw.name).and_then(|x| libvirt::virtio_mem(&x)) {
                Ok(mut v) if v.len() == 1 => (v.pop(), None),
                Ok(v) => (None, Some(format!("{} virtio-mem devices", v.len()))),
                Err(e) => (None, Some(format!("{e:#}"))),
            },
            false => (None, None),
        };
        let s = libvirt::stats(raw, k.last_swap_in, now, cfg.host.stats_period, vmem);
        k.last_swap_in = raw.fields.get("swap_in").copied();
        if let Some(e) = &vmem_failed {
            eprintln!("{}: virtio-mem: {e}; left alone this round", raw.name);
        }
        if once {
            let size = match &s.virtio_mem {
                Some(v) => format!(
                    "virtio-mem {:>6} + {:>6} of {:>6}{}",
                    size::show(v.base),
                    size::show(v.plugged),
                    size::show(v.size),
                    if v.plugged == v.requested {
                        String::new()
                    } else {
                        format!(" (asked {})", size::show(v.requested))
                    }
                ),
                None => format!(
                    "balloon {:>6} of {:>6}",
                    size::show(s.current),
                    size::show(s.maximum)
                ),
            };
            println!(
                "{:<16} {size}, guest uses {:>6}, could drop to {:>6}, QEMU holds {:>6}{}",
                s.name,
                s.total
                    .zip(s.free)
                    .map(|(t, f)| size::show(t - f.min(t)))
                    .unwrap_or("?".into()),
                s.total
                    .zip(s.usable)
                    .map(|(t, u)| size::show(t - u.min(t)))
                    .unwrap_or("?".into()),
                size::show(s.rss),
                match (k.skip, s.fresh) {
                    (Some(why), _) => format!("  [left alone: {why}]"),
                    (None, false) => "  [no fresh balloon stats]".into(),
                    _ => String::new(),
                }
            );
        }
        if k.skip.is_none() && vmem_failed.is_none() {
            managed.push(s);
        }
    }
    let host = policy::HostStats {
        available: host::mem_available()?,
    };
    let mut changes = policy::plan(host, &managed, |n| cfg.guest(n), &cfg.host);
    // --once shows the plan itself; the daemon makes only what the damping lets through
    if !once {
        let names: Vec<&str> = managed.iter().map(|g| g.name.as_str()).collect();
        let s = damp::Settings {
            shrink_settle: cfg.host.shrink_settle,
            settle_timeout: cfg.host.settle_timeout,
            tolerance: cfg.host.min_change.of(0),
        };
        changes = damper.filter(elapsed, changes, &names, s);
    }
    if once {
        println!(
            "host MemAvailable {}, reserve {}",
            size::show(host.available),
            size::show(cfg.host.reserve.of(0))
        );
        if changes.is_empty() {
            println!("plan: nothing to change");
        }
    }
    for c in changes {
        let line = format!(
            "{}: {} -> {} ({})",
            c.name,
            size::show(c.from),
            size::show(c.to),
            c.why
        );
        if dry {
            println!("would set {line}");
            continue;
        }
        let vmem = managed
            .iter()
            .find(|g| g.name == c.name)
            .and_then(|g| g.virtio_mem.as_ref());
        let done = match vmem {
            Some(v) => libvirt::set_virtio_mem(&c.name, &v.alias, v.requested_for(c.to)),
            None => libvirt::setmem(&c.name, c.to),
        };
        match done {
            Ok(()) => eprintln!("set {line}"),
            Err(e) => eprintln!("setting {line} failed: {e:#}"),
        }
    }
    Ok(())
}

/// A guest seen for the first time (or again after a restart): whether to leave it alone, its stats period,
/// its QEMU's oom_score_adj.
fn first_seen(cfg: &Config, name: &str, dry: bool) -> Known {
    let settings = cfg.guest(name);
    let how = if !settings.manage {
        Err("manage = false")
    } else {
        match libvirt::dumpxml(name)
            .and_then(|x| Ok((libvirt::passthrough(&x), libvirt::virtio_mem(&x)?)))
        {
            Ok((passthrough, vmem)) => how_to_set(passthrough, vmem.len()),
            Err(e) => {
                eprintln!("{name}: {e:#}; left alone");
                Err("domain XML unreadable")
            }
        }
    };
    let (skip, virtio_mem) = match how {
        Ok(v) => (None, v),
        Err(why) => (Some(why), false),
    };
    // the stats period only says how often the guest reports, so a dry run sets it too: without it there is
    // nothing fresh to plan with
    if skip.is_none()
        && let Err(e) = libvirt::set_stats_period(name, cfg.host.stats_period)
    {
        eprintln!("{name}: setting the balloon stats period: {e:#}");
    }
    if !dry && let (Some(adj), Some(pid)) = (cfg.host.qemu_oom_score_adj, libvirt::qemu_pid(name)) {
        match host::set_oom_score_adj(pid, adj) {
            Ok(true) => eprintln!("{name}: QEMU {pid} oom_score_adj {adj}"),
            Ok(false) => {}
            Err(e) => eprintln!("{name}: oom_score_adj of QEMU {pid}: {e:#}"),
        }
    }
    eprintln!(
        "{name}: {}",
        match (skip, virtio_mem) {
            (Some(w), _) => format!("left alone ({w})"),
            (None, true) => "managed through virtio-mem".into(),
            (None, false) => "managed".into(),
        }
    );
    Known {
        skip,
        virtio_mem,
        last_swap_in: None,
    }
}

/// How a managed guest's memory is set: Ok(true) through its virtio-mem device, Ok(false) through its balloon,
/// Err(why) not at all.
fn how_to_set(passthrough: bool, virtio_mem_devices: usize) -> Result<bool, &'static str> {
    match (passthrough, virtio_mem_devices) {
        (_, 1) => Ok(true),
        (_, n) if n > 1 => Err("several virtio-mem devices"),
        (true, _) => Err("passed-through device, RAM pinned, no virtio-mem"),
        (false, _) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::how_to_set;

    #[test]
    fn passed_through_guests_are_set_through_virtio_mem_or_left_alone() {
        assert_eq!(how_to_set(false, 0), Ok(false), "the balloon");
        assert_eq!(how_to_set(true, 1), Ok(true), "VFIO with virtio-mem");
        assert_eq!(
            how_to_set(false, 1),
            Ok(true),
            "virtio-mem without VFIO too"
        );
        assert!(how_to_set(true, 0).is_err(), "VFIO alone: RAM pinned");
        assert!(how_to_set(false, 2).is_err());
    }
}
