//! What this computer has: physical memory and processor count. Podman
//! machine sizing ([`crate::podman::machine_spec`]), onboarding's model
//! choice and `doctor` read them.

/// Physical memory in bytes: `hw.memsize` on macOS, `MemTotal` in
/// `/proc/meminfo` on Linux (inside WSL2, the memory WSL gives the Linux VM).
/// `None` when it cannot be read.
pub fn memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_memsize(&String::from_utf8_lossy(&output.stdout))
    } else {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

/// The processors this process may use (at least 1).
pub fn cpu_count() -> u64 {
    std::thread::available_parallelism()
        .map(|count| count.get() as u64)
        .unwrap_or(1)
}

/// `sysctl -n hw.memsize`: a byte count.
pub fn parse_memsize(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

/// `MemTotal:  16337456 kB` in `/proc/meminfo`, in bytes.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let mut words = line["MemTotal:".len()..].split_whitespace();
    let value: u64 = words.next()?.parse().ok()?;
    let multiplier = match words.next() {
        Some("kB") | Some("KB") | Some("kb") => 1024,
        Some("MB") | Some("mB") => 1024 * 1024,
        None => 1,
        Some(_) => return None,
    };
    value.checked_mul(multiplier).filter(|bytes| *bytes > 0)
}

/// `bytes` of memory as people say it for a computer: whole gigabytes
/// (binary, the way a 48 GB Mac reports 48 × 2³⁰ bytes).
pub fn memory_gb(bytes: u64) -> u64 {
    (bytes + (1 << 29)) >> 30
}

/// A computer with `bytes` of memory is one sold with at least `gb` GB.
/// Linux's `MemTotal` leaves out what the kernel and firmware reserve (a
/// 48 GB computer reports about 47 GiB), so up to 6% less counts.
pub fn at_least_gb(bytes: u64, gb: u64) -> bool {
    bytes as f64 >= (gb << 30) as f64 * 0.94
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_reads_from_sysctl_and_meminfo() {
        assert_eq!(parse_memsize("51539607552\n"), Some(51_539_607_552));
        assert_eq!(parse_memsize("0"), None);
        assert_eq!(parse_memsize("lots"), None);
        let meminfo = "MemFree:  100 kB\nMemTotal:       8094140 kB\nSwapTotal: 0 kB\n";
        assert_eq!(parse_meminfo(meminfo), Some(8_094_140 * 1024));
        assert_eq!(parse_meminfo("MemTotal: 12 parsecs"), None);
        assert_eq!(parse_meminfo("Nothing: 1 kB"), None);
        assert_eq!(memory_gb(51_539_607_552), 48);
        assert_eq!(memory_gb(8_094_140 * 1024), 8);
        assert_eq!(memory_gb(17_179_869_184), 16);
    }

    /// A 48 GiB Linux computer's MemTotal (about 49.2 million kB) is in the
    /// 48 GB tier; a 32 GB one is not.
    #[test]
    fn memory_tiers_allow_what_linux_reserves() {
        let linux_48 = 49_200_000 * 1024;
        assert_eq!(memory_gb(linux_48), 47);
        assert!(at_least_gb(linux_48, 48));
        assert!(at_least_gb(48 << 30, 48));
        assert!(!at_least_gb(32 << 30, 48));
        assert!(at_least_gb(98_400_000 * 1024, 96));
        assert!(!at_least_gb(64 << 30, 96));
    }

    #[test]
    fn this_computer_reports_its_memory_and_processors() {
        assert!(memory_bytes().is_some_and(|bytes| bytes >= 1 << 30));
        assert!(cpu_count() >= 1);
    }
}
