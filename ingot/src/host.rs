//! Whole-host usage on Linux, from /proc and statvfs. CPU is a rate, so it
//! needs the previous reading; the first call after start reports 0.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use copper_api::Usage;

pub struct HostUsage {
    disks_dir: PathBuf,
    /// (busy jiffies, total jiffies) at the last reading.
    last_cpu: Mutex<Option<(u64, u64)>>,
}

impl HostUsage {
    pub fn new(disks_dir: PathBuf) -> Self {
        Self { disks_dir, last_cpu: Mutex::new(None) }
    }

    pub fn read(&self) -> Option<Usage> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let (busy, total) = cpu_jiffies(&stat)?;
        let cpu_permille = {
            let mut last = self.last_cpu.lock().unwrap();
            let rate = match *last {
                Some((b0, t0)) if total > t0 => ((busy.saturating_sub(b0)) * 1000 / (total - t0)) as u32,
                _ => 0,
            };
            *last = Some((busy, total));
            rate
        };
        let (mem_total, mem_used) = memory_bytes(&meminfo)?;
        let (disk_total, disk_used) = filesystem_bytes(&self.disks_dir).unwrap_or((0, 0));
        Some(Usage { cpu_permille, memory_used_bytes: mem_used, memory_total_bytes: mem_total, disk_used_bytes: disk_used, disk_total_bytes: disk_total })
    }
}

/// Busy and total jiffies from the aggregate `cpu` line of /proc/stat.
fn cpu_jiffies(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line.split_whitespace().skip(1).filter_map(|f| f.parse().ok()).collect();
    let total: u64 = fields.iter().take(8).sum(); // user..steal; guest time is already in user
    let idle = fields.get(3)? + fields.get(4).unwrap_or(&0); // idle + iowait
    Some((total - idle, total))
}

/// (total, used) where used excludes reclaimable page cache.
fn memory_bytes(meminfo: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = meminfo.lines().find(|l| l.starts_with(name))?;
        Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    Some((total, total.saturating_sub(available)))
}

/// (total, used) of the filesystem holding `path`.
pub fn filesystem_bytes(path: &Path) -> Option<(u64, u64)> {
    let probe = if path.exists() { path } else { path.parent()? };
    let c = std::ffi::CString::new(probe.to_string_lossy().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // field widths differ between platforms
    let (frsize, blocks, avail) = (st.f_frsize as u64, st.f_blocks as u64, st.f_bavail as u64);
    let total = blocks * frsize;
    Some((total, total.saturating_sub(avail * frsize)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_files() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(cpu_jiffies(stat), Some((150, 1000)));
        let meminfo = "MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:   12000000 kB\n";
        assert_eq!(memory_bytes(meminfo), Some((16_000_000 * 1024, 4_000_000 * 1024)));
    }
}
