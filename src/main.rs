//! kvm_balloond: lends memory between the KVM guests of one host through their balloons. See README.md.

mod config;
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
    /// left alone: a passed-through device pins its RAM, or `manage = false`
    skip: Option<&'static str>,
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
    loop {
        let started = Instant::now();
        if let Err(e) = round(&cfg, &mut known, dry, once) {
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

fn round(cfg: &Config, known: &mut HashMap<String, Known>, dry: bool, once: bool) -> Result<()> {
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
        let s = libvirt::stats(raw, k.last_swap_in, now, cfg.host.stats_period);
        k.last_swap_in = raw.fields.get("swap_in").copied();
        if once {
            println!(
                "{:<16} balloon {:>6} of {:>6}, guest uses {:>6}, could drop to {:>6}, QEMU holds {:>6}{}",
                s.name,
                size::show(s.current),
                size::show(s.maximum),
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
        if k.skip.is_none() {
            managed.push(s);
        }
    }
    let host = policy::HostStats {
        available: host::mem_available()?,
    };
    let changes = policy::plan(host, &managed, |n| cfg.guest(n), &cfg.host);
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
        match libvirt::setmem(&c.name, c.to) {
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
    let skip = if !settings.manage {
        Some("manage = false")
    } else {
        match libvirt::dumpxml(name) {
            Ok(xml) if libvirt::passthrough(&xml) => Some("passed-through device, RAM pinned"),
            Ok(_) => None,
            Err(e) => {
                eprintln!("{name}: {e:#}; left alone");
                Some("domain XML unreadable")
            }
        }
    };
    if !dry {
        if skip.is_none()
            && let Err(e) = libvirt::set_stats_period(name, cfg.host.stats_period)
        {
            eprintln!("{name}: setting the balloon stats period: {e:#}");
        }
        if let (Some(adj), Some(pid)) = (cfg.host.qemu_oom_score_adj, libvirt::qemu_pid(name)) {
            match host::set_oom_score_adj(pid, adj) {
                Ok(true) => eprintln!("{name}: QEMU {pid} oom_score_adj {adj}"),
                Ok(false) => {}
                Err(e) => eprintln!("{name}: oom_score_adj of QEMU {pid}: {e:#}"),
            }
        }
    }
    eprintln!(
        "{name}: {}",
        skip.map(|w| format!("left alone ({w})"))
            .unwrap_or_else(|| "managed".into())
    );
    Known {
        skip,
        last_swap_in: None,
    }
}
