//! libvirt through `virsh -c qemu:///system` with fixed argument lists: one `domstats --balloon` a round for
//! every running guest, `dumpxml` once per guest start (passed-through devices), `dommemstat --period` once
//! per guest start (so the balloon stats stay fresh) and `setmem --live` for a change.

use crate::policy::GuestStats;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::process::Command;

const URI: &str = "qemu:///system";

fn virsh(args: &[&str]) -> Result<String> {
    let out = Command::new("virsh")
        .arg("-c")
        .arg(URI)
        .args(args)
        .output()
        .context("running virsh")?;
    if !out.status.success() {
        bail!(
            "virsh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One guest of `domstats`: its name and its `balloon.*` fields.
#[derive(Debug, Default)]
pub struct Raw {
    pub name: String,
    pub fields: HashMap<String, u64>,
}

/// `virsh domstats --balloon --list-running`'s text: `Domain: 'NAME'` then `  balloon.KEY=VALUE` lines.
pub fn parse_domstats(text: &str) -> Vec<Raw> {
    let mut out: Vec<Raw> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Domain: ") {
            out.push(Raw {
                name: rest.trim_matches(|c| c == '\'' || c == '"').to_string(),
                ..Raw::default()
            });
        } else if let (Some(cur), Some((k, v))) = (out.last_mut(), line.split_once('='))
            && let (Some(k), Ok(v)) = (k.strip_prefix("balloon."), v.parse::<u64>())
        {
            cur.fields.insert(k.to_string(), v);
        }
    }
    out
}

pub fn domstats() -> Result<Vec<Raw>> {
    Ok(parse_domstats(&virsh(&[
        "domstats",
        "--balloon",
        "--list-running",
    ])?))
}

/// The guest has a device passed through (VFIO): its RAM is pinned for DMA, a balloon can't take any.
pub fn passthrough(xml: &str) -> bool {
    let compact: String = xml
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace('"', "'");
    compact.contains("<hostdevmode='subsystem'type='pci'")
        || compact.contains("<hostdevmode='subsystem'type='mdev'")
}

pub fn dumpxml(name: &str) -> Result<String> {
    virsh(&["dumpxml", name])
}

pub fn set_stats_period(name: &str, secs: u32) -> Result<()> {
    virsh(&["dommemstat", name, "--period", &secs.to_string(), "--live"]).map(|_| ())
}

pub fn setmem(name: &str, kib: u64) -> Result<()> {
    virsh(&["setmem", name, &format!("{kib}KiB"), "--live"]).map(|_| ())
}

/// The QEMU pid libvirt wrote for the guest.
pub fn qemu_pid(name: &str) -> Option<u32> {
    std::fs::read_to_string(format!("/run/libvirt/qemu/{name}.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What the policy needs from one guest's fields; `last_swap_in` is the counter seen the round before.
pub fn stats(raw: &Raw, last_swap_in: Option<u64>, now_secs: u64, period: u32) -> GuestStats {
    let f = |k: &str| raw.fields.get(k).copied();
    let swap_in = f("swap_in").unwrap_or(0);
    let fresh = match f("last-update") {
        Some(t) if t > 0 => now_secs.saturating_sub(t) <= (period as u64 * 3).max(6),
        _ => false,
    };
    GuestStats {
        name: raw.name.clone(),
        current: f("current").unwrap_or(0),
        maximum: f("maximum").unwrap_or(0),
        total: f("available"),
        free: f("unused"),
        usable: f("usable"),
        swapped_in: last_swap_in.map(|l| swap_in.saturating_sub(l)).unwrap_or(0),
        rss: f("rss").unwrap_or(0),
        fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domstats_text() {
        let t = "Domain: 'alma-1b'\n  balloon.current=20971520\n  balloon.maximum=27262976\n  balloon.swap_in=0\n  balloon.unused=4000000\n  balloon.available=20500000\n  balloon.usable=15000000\n  balloon.last-update=1794300000\n  balloon.rss=17000000\n\nDomain: 'git-2'\n  balloon.current=1048576\n  balloon.maximum=1048576\n";
        let r = parse_domstats(t);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "alma-1b");
        let s = stats(&r[0], Some(0), 1794300002, 2);
        assert!(s.fresh);
        assert_eq!(s.current, 20971520);
        assert_eq!(s.free, Some(4000000));
        assert_eq!(s.usable, Some(15000000));
        let s = stats(&r[1], None, 1794300002, 2);
        assert!(
            !s.fresh,
            "no last-update: the balloon driver doesn't answer"
        );
    }

    #[test]
    fn passthrough_found() {
        assert!(passthrough(
            "<devices>\n <hostdev mode='subsystem' type='pci' managed='yes'>\n"
        ));
        assert!(passthrough(r#"<hostdev mode="subsystem" type="pci">"#));
        assert!(!passthrough("<hostdev mode='subsystem' type='usb'>"));
        assert!(!passthrough("<disk type='volume'>"));
    }
}
