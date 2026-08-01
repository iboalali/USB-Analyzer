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

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// Counters for one block device, given its sysfs directory.
///
/// `/sys/class/block/<name>` works as well as `/sys/block/<name>`, which is why
/// this takes a path rather than a name.
pub fn stats_at(dir: &Path) -> Option<BlockStats> {
    read_stats(dir)
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

// ---------------------------------------------------------------------------
// What is in use, and therefore must not be interrupted
// ---------------------------------------------------------------------------

/// A reason a disk may not be taken off the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// The kernel name of the thing actually in use. Rarely the disk itself:
    /// usually a partition, sometimes a device-mapper node stacked above one.
    pub via: String,
    pub kind: HoldKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "where")]
pub enum HoldKind {
    Mounted(String),
    Swap,
}

impl Hold {
    /// One clause, for a refusal message.
    pub fn describe(&self) -> String {
        match &self.kind {
            HoldKind::Mounted(at) => format!("{} is mounted at {at}", self.via),
            HoldKind::Swap => format!("{} is in use as swap", self.via),
        }
    }
}

/// Every disk currently backing a mounted filesystem or an active swap area,
/// mapped to the reasons why.
///
/// This is the check that stands between a disruptive probe and someone's data,
/// so it resolves the whole stack rather than comparing names. A LUKS volume on
/// a USB stick appears in `/proc/self/mounts` as `/dev/mapper/backup`, which
/// shares no substring with `sdb` — following `slaves/` links down to the
/// physical disks is the only way to connect the two. Missing that link would
/// mean yanking a mounted encrypted volume off the bus.
///
/// Read fresh at the moment of the decision, never from a snapshot: a
/// filesystem can be mounted between capture and probe.
pub fn holders() -> BTreeMap<String, Vec<Hold>> {
    holders_in(Path::new("/"))
}

pub fn holders_in(root: &Path) -> BTreeMap<String, Vec<Hold>> {
    let mut out: BTreeMap<String, Vec<Hold>> = BTreeMap::new();
    for (node, kind) in in_use_nodes(root) {
        let Some(name) = kernel_name(root, &node) else {
            continue;
        };
        for disk in base_disks(root, &name, 0) {
            let hold = Hold {
                via: name.clone(),
                kind: kind.clone(),
            };
            let holds = out.entry(disk).or_default();
            if !holds.contains(&hold) {
                holds.push(hold);
            }
        }
    }
    out
}

/// The `/dev/...` nodes named by `/proc/self/mounts` and `/proc/swaps`.
///
/// Sources that are not device nodes — `tmpfs`, `cgroup2`, a bind mount's
/// original path — are skipped, since they cannot be a disk.
fn in_use_nodes(root: &Path) -> Vec<(String, HoldKind)> {
    let mut out = Vec::new();

    if let Some(mounts) = fsx::read_str(root.join("proc/self/mounts")) {
        for line in mounts.lines() {
            let mut f = line.split_whitespace();
            let (Some(src), Some(at)) = (f.next(), f.next()) else {
                continue;
            };
            if src.starts_with("/dev/") {
                out.push((src.to_string(), HoldKind::Mounted(unescape(at))));
            }
        }
    }

    if let Some(swaps) = fsx::read_str(root.join("proc/swaps")) {
        // First line is a header.
        for line in swaps.lines().skip(1) {
            if let Some(src) = line.split_whitespace().next() {
                if src.starts_with("/dev/") {
                    out.push((src.to_string(), HoldKind::Swap));
                }
            }
        }
    }

    out
}

/// `\040` and friends: the kernel octal-escapes whitespace in mount points.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The kernel name behind a `/dev` path, following symlinks — a mount source is
/// often `/dev/disk/by-uuid/...` rather than the node itself.
fn kernel_name(root: &Path, node: &str) -> Option<String> {
    let path = root.join(node.trim_start_matches('/'));
    let real = fsx::canonicalize(&path).unwrap_or(path);
    let name = fsx::file_name(&real);
    (!name.is_empty()).then_some(name)
}

/// Walk down from a device to the physical disks underneath it.
///
/// Three cases: a stacked device (dm, md, bcache) names its members in
/// `slaves/`; a partition's parent is the directory that contains it; anything
/// else is already a disk.
fn base_disks(root: &Path, name: &str, depth: u8) -> Vec<String> {
    // Stacks nest — dm-crypt over LVM over md is ordinary — but a cycle would
    // be a kernel bug, and recursing forever on one would be ours.
    if depth > 8 {
        return vec![name.to_string()];
    }
    let dir = root.join("sys/class/block").join(name);

    let slaves: Vec<String> = fsx::list_dir(dir.join("slaves"))
        .iter()
        .map(|p| fsx::file_name(p))
        .collect();
    if !slaves.is_empty() {
        let mut out: Vec<String> = slaves
            .iter()
            .flat_map(|s| base_disks(root, s, depth + 1))
            .collect();
        out.sort();
        out.dedup();
        return out;
    }

    if dir.join("partition").exists() {
        // `/sys/class/block/sda1` links to `.../block/sda/sda1`, so the parent
        // directory is the whole disk. Exact, where stripping trailing digits
        // would only be a guess — and a wrong one for `mmcblk0` or `nvme0n1`.
        if let Some(parent) = fsx::canonicalize(&dir).as_deref().and_then(Path::parent) {
            let disk = fsx::file_name(parent);
            if !disk.is_empty() && disk != "block" {
                return vec![disk];
            }
        }
    }

    vec![name.to_string()]
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

    /// The check that guards every disruptive probe, against the shapes that
    /// would defeat a naive one: a partition, an encrypted volume, and a mount
    /// point with a space in it.
    #[test]
    fn a_mounted_stack_resolves_down_to_the_physical_disk() {
        let root = std::env::temp_dir().join(format!("usbprobe-hold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc/self")).unwrap();
        let class = root.join("sys/class/block");
        let devices = root.join("sys/devices/blk");
        fs::create_dir_all(&class).unwrap();

        // sda holds a mounted partition; sdb is whole-disk LUKS under dm-0;
        // sdc holds swap; sdd is idle and must stay probeable.
        let layout = [
            ("sda", vec!["sda1"]),
            ("sdb", vec!["sdb1"]),
            ("sdc", vec![]),
            ("sdd", vec![]),
        ];
        for (disk, parts) in layout {
            fs::create_dir_all(devices.join(disk)).unwrap();
            std::os::unix::fs::symlink(devices.join(disk), class.join(disk)).unwrap();
            for p in parts {
                fs::create_dir_all(devices.join(disk).join(p)).unwrap();
                fs::write(devices.join(disk).join(p).join("partition"), "1\n").unwrap();
                std::os::unix::fs::symlink(devices.join(disk).join(p), class.join(p)).unwrap();
            }
        }
        // dm-0 declares its member rather than naming it in any recognisable way.
        fs::create_dir_all(class.join("dm-0/slaves/sdb1")).unwrap();
        fs::create_dir_all(root.join("dev/mapper")).unwrap();
        std::os::unix::fs::symlink(root.join("dev/dm-0"), root.join("dev/mapper/backup")).unwrap();
        fs::write(root.join("dev/dm-0"), b"").unwrap();

        fs::write(
            root.join("proc/self/mounts"),
            "tmpfs /run tmpfs rw 0 0\n\
             /dev/sda1 / ext4 rw,relatime 0 0\n\
             /dev/mapper/backup /media/My\\040Backup ext4 rw 0 0\n\
             sysfs /sys sysfs rw 0 0\n",
        )
        .unwrap();
        fs::write(
            root.join("proc/swaps"),
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
             /dev/sdc\tpartition\t8388604\t\t0\t\t-2\n",
        )
        .unwrap();

        let held = holders_in(&root);
        assert_eq!(
            held.keys().collect::<Vec<_>>(),
            vec!["sda", "sdb", "sdc"],
            "sdd is idle and must not appear: {held:?}"
        );
        assert_eq!(held["sda"][0].describe(), "sda1 is mounted at /");
        // The whole point: nothing in "/dev/mapper/backup" says "sdb".
        assert_eq!(held["sdb"][0].via, "dm-0");
        assert_eq!(
            held["sdb"][0].kind,
            HoldKind::Mounted("/media/My Backup".into())
        );
        assert_eq!(held["sdc"][0].kind, HoldKind::Swap);

        let _ = fs::remove_dir_all(&root);
    }

    /// On any running system the root filesystem is mounted, so this cannot
    /// come back empty — and if it did, every disruptive probe would think the
    /// machine's own disk was free to interrupt.
    #[test]
    fn the_live_machine_reports_at_least_its_own_root_disk() {
        let held = holders();
        assert!(!held.is_empty(), "no mounted disk found on a running system");
        assert!(held.values().flatten().any(|h| h.kind == HoldKind::Mounted("/".into())));
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
