//! Block devices and their live I/O counters.
//!
//! This is what turns "the link negotiated 480 Mbps" into "you are moving
//! 34 MB/s", which is the number a user actually feels. `/sys/block/*/stat` is
//! world-readable, so real throughput needs no privileges at all — only two
//! reads and the time between them.
//!
//! Attribution to a USB device is by sysfs path containment: a block device
//! whose canonical path lies under a USB device's path is attached through it.
//! That is exact, not a heuristic, because it follows the same device hierarchy
//! the kernel built.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::{BlockDevice, BlockStats, Throughput};
use crate::sysfs as fsx;

const SYS_BLOCK: &str = "/sys/block";

/// `stat` reports in 512-byte units regardless of the device's logical block
/// size — a kernel convention, not a property of the hardware.
const STAT_SECTOR_BYTES: u64 = 512;

/// Read every real block device, with cumulative counters. Instant.
pub fn read() -> Vec<BlockDevice> {
    read_from(Path::new(SYS_BLOCK))
}

pub fn read_from(dir: &Path) -> Vec<BlockDevice> {
    let mut out = Vec::new();
    for entry in fsx::list_dir(dir) {
        let name = fsx::file_name(&entry);
        // Virtual devices are not storage anyone plugged in.
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("md")
        {
            continue;
        }
        let Some(real) = fsx::canonicalize(&entry) else {
            continue;
        };
        let size_sectors = fsx::read_u64(&entry, "size").unwrap_or(0);
        out.push(BlockDevice {
            name,
            sysfs_path: real,
            model: fsx::read_attr(entry.join("device"), "model"),
            vendor: fsx::read_attr(entry.join("device"), "vendor"),
            // `size` is always in 512-byte units too.
            size_bytes: size_sectors.checked_mul(STAT_SECTOR_BYTES),
            rotational: fsx::read_flag(entry.join("queue"), "rotational"),
            removable: fsx::read_flag(&entry, "removable"),
            stats: read_stats(&entry),
            throughput: None,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Read, wait, read again, and compute live throughput.
///
/// The wait is the measurement — there is no way to get an instantaneous rate
/// from cumulative counters. Callers that already poll (watch mode) should diff
/// consecutive snapshots instead of paying for this delay.
pub fn sample(window: Duration) -> Vec<BlockDevice> {
    let first = read();
    std::thread::sleep(window);
    let mut second = read();
    for dev in &mut second {
        if let Some(prev) = first.iter().find(|d| d.name == dev.name) {
            dev.throughput = match (&dev.stats, &prev.stats) {
                (Some(now), Some(before)) => now.delta(before),
                _ => None,
            };
        }
    }
    second
}

/// `reads_completed reads_merged sectors_read ms_reading writes_completed
///  writes_merged sectors_written ms_writing ios_in_flight ...`
fn read_stats(dir: &Path) -> Option<BlockStats> {
    let raw = fsx::read_attr(dir, "stat")?;
    let f: Vec<u64> = raw
        .split_whitespace()
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    // Older kernels report 11 fields, newer ones 15 or 17. Only the first nine
    // are needed and they have never moved.
    if f.len() < 9 {
        return None;
    }
    Some(BlockStats {
        read_ios: f[0],
        sectors_read: f[2],
        ms_reading: f[3],
        write_ios: f[4],
        sectors_written: f[6],
        ms_writing: f[7],
        ios_in_flight: f[8],
        sampled_at_unix_ms: now_ms(),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Block devices attached through a given USB device, by path containment.
pub fn attached_to<'a>(blocks: &'a [BlockDevice], usb_path: &Path) -> Vec<&'a BlockDevice> {
    blocks
        .iter()
        .filter(|b| b.sysfs_path.starts_with(usb_path))
        .collect()
}

impl BlockStats {
    /// Bytes per second between two samples, or `None` if time did not advance.
    pub fn delta(&self, earlier: &BlockStats) -> Option<Throughput> {
        let ms = self
            .sampled_at_unix_ms
            .checked_sub(earlier.sampled_at_unix_ms)
            .filter(|ms| *ms > 0)?;
        let secs = ms as f64 / 1000.0;
        let bytes = |now: u64, before: u64| {
            now.saturating_sub(before) as f64 * STAT_SECTOR_BYTES as f64 / secs
        };
        Some(Throughput {
            read_bps: bytes(self.sectors_read, earlier.sectors_read),
            write_bps: bytes(self.sectors_written, earlier.sectors_written),
            interval_ms: ms,
        })
    }

    pub fn total_read_bytes(&self) -> u64 {
        self.sectors_read.saturating_mul(STAT_SECTOR_BYTES)
    }

    pub fn total_written_bytes(&self) -> u64 {
        self.sectors_written.saturating_mul(STAT_SECTOR_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn parses_a_real_stat_line_and_skips_virtual_devices() {
        let base = std::env::temp_dir().join(format!("usbprobe-blk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        let sdc = base.join("sdc");
        fs::create_dir_all(sdc.join("queue")).unwrap();
        fs::create_dir_all(sdc.join("device")).unwrap();
        // 17-field form from a real kernel.
        fs::write(
            sdc.join("stat"),
            " 3591703   705544 130185220   508905  2117143  4999796 256260138 16075957        0   541417 16637854        0        0        0        0    86485    52992\n",
        )
        .unwrap();
        fs::write(sdc.join("size"), "1953525168\n").unwrap();
        fs::write(sdc.join("removable"), "0\n").unwrap();
        fs::write(sdc.join("queue/rotational"), "1\n").unwrap();
        fs::write(sdc.join("device/model"), "ST1000LM024 HN-M101MBB\n").unwrap();

        for virt in ["loop0", "zram0", "dm-0"] {
            fs::create_dir_all(base.join(virt)).unwrap();
        }

        let blocks = read_from(&base);
        assert_eq!(blocks.len(), 1, "virtual devices must be skipped");
        let b = &blocks[0];
        assert_eq!(b.name, "sdc");
        assert_eq!(b.rotational, Some(true));
        assert_eq!(b.model.as_deref(), Some("ST1000LM024 HN-M101MBB"));
        // 1953525168 * 512 = 1.0 TB
        assert_eq!(b.size_bytes, Some(1_000_204_886_016));

        let s = b.stats.as_ref().unwrap();
        assert_eq!(s.sectors_read, 130_185_220);
        assert_eq!(s.sectors_written, 256_260_138);
        assert_eq!(s.ios_in_flight, 0);
        assert_eq!(s.total_read_bytes(), 130_185_220 * 512);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn computes_throughput_between_two_samples() {
        let before = BlockStats {
            read_ios: 0,
            sectors_read: 1000,
            ms_reading: 0,
            write_ios: 0,
            sectors_written: 500,
            ms_writing: 0,
            ios_in_flight: 0,
            sampled_at_unix_ms: 1_000,
        };
        let after = BlockStats {
            sectors_read: 1000 + 200_000, // 100 MB in 1 s
            sectors_written: 500 + 20_000, // 10 MB in 1 s
            sampled_at_unix_ms: 2_000,
            ..before
        };
        let t = after.delta(&before).unwrap();
        assert_eq!(t.interval_ms, 1000);
        assert!((t.read_bps - 102_400_000.0).abs() < 1.0);
        assert!((t.write_bps - 10_240_000.0).abs() < 1.0);
        assert!((t.total_bps() - 112_640_000.0).abs() < 1.0);

        // Time not advancing must not divide by zero.
        assert!(after.delta(&after).is_none());
        // Counters resetting (device re-enumerated) must not underflow.
        assert!(before.delta(&after).is_none());
    }

    #[test]
    fn attribution_is_by_path_containment() {
        let blocks = vec![BlockDevice {
            name: "sdc".into(),
            sysfs_path: PathBuf::from("/sys/devices/pci0000:00/usb4/4-1/4-1:1.0/host1/block/sdc"),
            model: None,
            vendor: None,
            size_bytes: None,
            rotational: None,
            removable: None,
            stats: None,
            throughput: None,
        }];
        assert_eq!(
            attached_to(&blocks, Path::new("/sys/devices/pci0000:00/usb4/4-1")).len(),
            1
        );
        // A sibling device must not claim it.
        assert!(attached_to(&blocks, Path::new("/sys/devices/pci0000:00/usb4/4-2")).is_empty());
    }
}
