//! Sizes as people write them in the config (`512M`, `2G`, `1.5G`, `25%`), all held in KiB like libvirt's
//! balloon numbers.

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer};

/// An amount of memory in KiB, or a share of the guest's maximum (`25%`), resolved per guest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Amount {
    KiB(u64),
    Percent(f64),
}

impl Amount {
    /// The amount for a guest whose balloon maximum is `max_kib`.
    pub fn of(self, max_kib: u64) -> u64 {
        match self {
            Amount::KiB(k) => k,
            Amount::Percent(p) => (max_kib as f64 * p / 100.0) as u64,
        }
    }
}

/// `"512M"`, `"2G"`, `"1.5G"`, `"64K"`, a plain number (KiB) or `"25%"`.
pub fn parse(s: &str) -> Result<Amount> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        let p: f64 = p
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("not a percentage: {s:?}"))?;
        if !(0.0..=100.0).contains(&p) {
            bail!("a percentage from 0 to 100: {s:?}");
        }
        return Ok(Amount::Percent(p));
    }
    let (num, mult) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let m = match c.to_ascii_uppercase() {
                'K' => 1.0,
                'M' => 1024.0,
                'G' => 1024.0 * 1024.0,
                'T' => 1024.0 * 1024.0 * 1024.0,
                _ => bail!("unknown unit in {s:?} (K, M, G, T or %)"),
            };
            (&s[..i], m)
        }
        _ => (s, 1.0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("not a size: {s:?}"))?;
    if n < 0.0 {
        bail!("a size can't be negative: {s:?}");
    }
    Ok(Amount::KiB((n * mult) as u64))
}

/// A KiB amount for people: `1.5G`, `512M`.
pub fn show(kib: u64) -> String {
    let g = kib as f64 / (1024.0 * 1024.0);
    if g >= 1.0 {
        format!("{g:.1}G")
    } else {
        format!("{}M", kib / 1024)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            N(u64),
        }
        match Raw::deserialize(d)? {
            Raw::S(s) => parse(&s).map_err(serde::de::Error::custom),
            Raw::N(n) => Ok(Amount::KiB(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse("512M").unwrap(), Amount::KiB(512 * 1024));
        assert_eq!(parse("2G").unwrap(), Amount::KiB(2 * 1024 * 1024));
        assert_eq!(parse("1.5g").unwrap(), Amount::KiB(1536 * 1024));
        assert_eq!(parse("100").unwrap(), Amount::KiB(100));
        assert_eq!(parse("25%").unwrap().of(8 * 1024 * 1024), 2 * 1024 * 1024);
        assert!(parse("12X").is_err());
        assert!(parse("120%").is_err());
        assert_eq!(show(1536 * 1024), "1.5G");
        assert_eq!(show(300 * 1024), "300M");
    }
}
