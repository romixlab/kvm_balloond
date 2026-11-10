//! The host's side: MemAvailable, the PSI memory trigger that wakes the daemon the moment the host stalls on
//! memory, and the QEMUs' oom_score_adj.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::Duration;

/// MemAvailable of /proc/meminfo, KiB.
pub fn mem_available() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    parse_meminfo(&text, "MemAvailable").context("no MemAvailable in /proc/meminfo")
}

pub fn parse_meminfo(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key)
            .then(|| v.split_whitespace().next()?.parse().ok())
            .flatten()
    })
}

/// A PSI trigger on /proc/pressure/memory (`some 150000 1000000`: 150 ms of stall within 1 s).
pub struct Pressure {
    file: File,
}

impl Pressure {
    pub fn open(trigger: &str) -> Result<Pressure> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/proc/pressure/memory")
            .context("opening /proc/pressure/memory (PSI)")?;
        file.write_all(format!("{trigger}\0").as_bytes())
            .with_context(|| format!("setting the PSI trigger {trigger:?}"))?;
        Ok(Pressure { file })
    }

    /// Waits up to `timeout`; true when the trigger fired.
    pub fn wait(&self, timeout: Duration) -> Result<bool> {
        let mut fd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(&mut fd, 1, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e).context("poll on the PSI trigger");
        }
        if fd.revents & libc::POLLERR != 0 {
            anyhow::bail!("the PSI trigger went away");
        }
        Ok(n > 0 && fd.revents & libc::POLLPRI != 0)
    }
}

/// Writes `adj` to the process's oom_score_adj when it differs.
pub fn set_oom_score_adj(pid: u32, adj: i32) -> Result<bool> {
    let path = format!("/proc/{pid}/oom_score_adj");
    let now: i32 = std::fs::read_to_string(&path)?.trim().parse().unwrap_or(0);
    if now == adj {
        return Ok(false);
    }
    std::fs::write(&path, adj.to_string()).with_context(|| format!("writing {path}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn meminfo() {
        let t = "MemTotal:       32520000 kB\nMemFree:  1000 kB\nMemAvailable:    6000000 kB\n";
        assert_eq!(super::parse_meminfo(t, "MemAvailable"), Some(6000000));
        assert_eq!(super::parse_meminfo(t, "Nope"), None);
    }
}
