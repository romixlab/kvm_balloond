//! libvirt through `virsh -c qemu:///system` with fixed argument lists: one `domstats --balloon` a round for
//! every running guest, `dumpxml` once per guest start (passed-through devices), `dommemstat --period` once
//! per guest start (so the balloon stats stay fresh) and `setmem --live` for a change. A guest with a virtio-mem
//! device is set with `update-memory-device --requested-size --live` instead, and its `dumpxml` is read every
//! round for what the device has plugged.

use crate::policy::{GuestStats, VirtioMem};
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

/// The guest's virtio-mem devices from its live XML, each with the static RAM (the sum of the NUMA cells).
pub fn virtio_mem(xml: &str) -> Result<Vec<VirtioMem>> {
    let doc = roxmltree::Document::parse(xml).context("parsing the domain XML")?;
    let base: u64 = doc
        .descendants()
        .filter(|n| n.has_tag_name("cell") && n.parent().is_some_and(|p| p.has_tag_name("numa")))
        .filter_map(|c| kib(c.attribute("memory")?, c.attribute("unit")))
        .sum();
    let mut out = Vec::new();
    for dev in doc.descendants().filter(|n| {
        n.has_tag_name("memory")
            && n.attribute("model") == Some("virtio-mem")
            && n.parent().is_some_and(|p| p.has_tag_name("devices"))
    }) {
        let target = child(dev, "target").context("virtio-mem without <target>")?;
        let size = |tag: &str| child(target, tag).and_then(|n| kib(n.text()?, n.attribute("unit")));
        let alias = child(dev, "alias")
            .and_then(|a| a.attribute("name"))
            .context("virtio-mem without an alias (not running?)")?;
        let requested = size("requested").context("virtio-mem without <requested>")?;
        out.push(VirtioMem {
            alias: alias.to_string(),
            base,
            size: size("size").context("virtio-mem without <size>")?,
            block: size("block").context("virtio-mem without <block>")?,
            requested,
            // libvirt fills <current> from QEMU's size-change events; before the first one it is what was asked
            plugged: size("current").unwrap_or(requested),
        });
    }
    Ok(out)
}

fn child<'a, 'i>(parent: roxmltree::Node<'a, 'i>, tag: &str) -> Option<roxmltree::Node<'a, 'i>> {
    parent.children().find(|n| n.has_tag_name(tag))
}

/// A libvirt number with its unit attribute, in KiB (libvirt's default unit).
fn kib(value: &str, unit: Option<&str>) -> Option<u64> {
    let v: u64 = value.trim().parse().ok()?;
    let bytes: u64 = match unit.unwrap_or("KiB") {
        "b" | "bytes" => 1,
        "KB" => 1000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1000 * 1000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1000 * 1000 * 1000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1000 * 1000 * 1000 * 1000,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    Some(v.checked_mul(bytes)? / 1024)
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

/// Asks the guest's virtio-mem device to plug or unplug to `kib` (a whole number of blocks).
pub fn set_virtio_mem(name: &str, alias: &str, kib: u64) -> Result<()> {
    virsh(&[
        "update-memory-device",
        name,
        "--alias",
        alias,
        "--requested-size",
        &format!("{kib}KiB"),
        "--live",
    ])
    .map(|_| ())
}

/// The QEMU pid libvirt wrote for the guest.
pub fn qemu_pid(name: &str) -> Option<u32> {
    std::fs::read_to_string(format!("/run/libvirt/qemu/{name}.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What the policy needs from one guest's fields; `last_swap_in` is the counter seen the round before. With a
/// virtio-mem device the guest's size is its static RAM plus what the device has plugged now, not the balloon's:
/// the balloon only reports the guest's own numbers then.
pub fn stats(
    raw: &Raw,
    last_swap_in: Option<u64>,
    now_secs: u64,
    period: u32,
    virtio_mem: Option<VirtioMem>,
) -> GuestStats {
    let f = |k: &str| raw.fields.get(k).copied();
    let swap_in = f("swap_in").unwrap_or(0);
    let fresh = match f("last-update") {
        Some(t) if t > 0 => now_secs.saturating_sub(t) <= (period as u64 * 3).max(6),
        _ => false,
    };
    let (current, maximum) = match &virtio_mem {
        Some(v) => (v.base + v.plugged, v.base + v.size),
        None => (f("current").unwrap_or(0), f("maximum").unwrap_or(0)),
    };
    GuestStats {
        name: raw.name.clone(),
        current,
        maximum,
        total: f("available"),
        free: f("unused"),
        usable: f("usable"),
        swapped_in: last_swap_in.map(|l| swap_in.saturating_sub(l)).unwrap_or(0),
        rss: f("rss").unwrap_or(0),
        fresh,
        virtio_mem,
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
        let s = stats(&r[0], Some(0), 1794300002, 2, None);
        assert!(s.fresh);
        assert_eq!(s.current, 20971520);
        assert_eq!(s.free, Some(4000000));
        assert_eq!(s.usable, Some(15000000));
        let s = stats(&r[1], None, 1794300002, 2, None);
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

    /// alma-1tv's live XML after the change, cut to what matters (libvirt 11.10 writes every size in KiB).
    const TV: &str = r#"<domain type='kvm' id='7'>
  <name>alma-1tv</name>
  <maxMemory slots='1' unit='KiB'>16777216</maxMemory>
  <memory unit='KiB'>8388608</memory>
  <currentMemory unit='KiB'>4194304</currentMemory>
  <cpu mode='host-passthrough'>
    <numa>
      <cell id='0' cpus='0-3' memory='2097152' unit='KiB'/>
    </numa>
  </cpu>
  <devices>
    <hostdev mode='subsystem' type='pci' managed='yes'>
      <source><address domain='0x0000' bus='0x06' slot='0x00' function='0x0'/></source>
    </hostdev>
    <memory model='virtio-mem'>
      <target>
        <size unit='KiB'>6291456</size>
        <node>0</node>
        <block unit='KiB'>2048</block>
        <requested unit='KiB'>1048576</requested>
        <current unit='KiB'>3145728</current>
        <address base='0x140000000'/>
      </target>
      <alias name='virtiomem0'/>
    </memory>
    <memballoon model='virtio'><alias name='balloon0'/></memballoon>
  </devices>
</domain>"#;

    #[test]
    fn virtio_mem_from_the_live_xml() {
        let v = virtio_mem(TV).unwrap();
        assert_eq!(
            v,
            vec![VirtioMem {
                alias: "virtiomem0".into(),
                base: 2 * 1024 * 1024,
                size: 6 * 1024 * 1024,
                block: 2048,
                requested: 1024 * 1024,
                plugged: 3 * 1024 * 1024,
            }]
        );
        assert!(passthrough(TV));
        assert!(
            virtio_mem("<domain><devices><memballoon model='virtio'/></devices></domain>")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn virtio_mem_units_and_no_current_yet() {
        let xml = "<domain><cpu><numa><cell id='0' memory='1' unit='GiB'/><cell id='1' memory='1024' unit='MiB'/></numa></cpu><devices><memory model='virtio-mem'><target><size unit='GiB'>4</size><block unit='MiB'>2</block><requested unit='b'>1073741824</requested></target><alias name='virtiomem1'/></memory></devices></domain>";
        let v = &virtio_mem(xml).unwrap()[0];
        assert_eq!(v.base, 2 * 1024 * 1024);
        assert_eq!(v.block, 2048);
        assert_eq!(v.plugged, v.requested, "no <current> yet: what was asked");
        assert_eq!(v.requested, 1024 * 1024);
    }

    #[test]
    fn virtio_mem_guest_size_is_what_is_plugged() {
        // unplugging: asked for 1G, 3G still plugged; the balloon says 4G (libvirt's own view), the guest has 5G
        let r = parse_domstats(
            "Domain: 'alma-1tv'\n  balloon.current=4194304\n  balloon.maximum=8388608\n  balloon.last-update=1794300000\n",
        );
        let v = virtio_mem(TV).unwrap().pop();
        let s = stats(&r[0], None, 1794300001, 2, v);
        assert_eq!(s.current, 5 * 1024 * 1024);
        assert_eq!(s.maximum, 8 * 1024 * 1024);
    }
}
