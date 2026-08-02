//! What a link *achieves*, as opposed to what it negotiated.
//!
//! Everything else in this crate reads state the kernel already decided. A
//! marginal cable usually negotiates full speed and only fails under load, so
//! the one question passive reading cannot answer is what happens when data
//! actually moves. This module moves some.
//!
//! # Why a block read and not usbfs
//!
//! The obvious way to load a link is to claim the device through
//! `/dev/bus/usb/BBB/DDD` and issue raw SCSI over bulk-only transport. That
//! needs `ioctl`, which needs libc, which this crate does not have and has
//! already refused once — see [`crate::caps`] on why the binary usbmon API is
//! out of reach. There is no portable pure-Rust `ioctl`: inline assembly would
//! work on x86_64 and break on aarch64, which is precisely where a USB
//! diagnostic tool earns its keep.
//!
//! A sequential read of the block device is bottlenecked by exactly the same
//! chain — cable, bridge, media — and costs nothing in dependencies. It also
//! changes nothing and detaches no driver, so unlike the usbfs approach it can
//! run against a **mounted** drive, which is the state a drive is in when
//! somebody complains that it is slow.
//!
//! What it gives up: it cannot load a device that is not storage, and the block
//! layer's own overhead is inside the number. Neither matters for the question
//! being asked.
//!
//! # O_DIRECT is not optional, and not free of traps
//!
//! Without it the page cache answers most reads and the measurement reports
//! several GB/s over a 480 Mbps link — a confidently wrong number, the exact
//! failure this tool exists to avoid. Two things can go wrong with it:
//!
//! 1. **The constant is architecture-specific.** `O_DIRECT` is `0o40000` on
//!    x86 and ARM but `0o400000` on PowerPC and different again elsewhere.
//!    Linux ignores unknown bits in `open` flags silently, so a wrong value
//!    does not fail — it just quietly reads from cache.
//! 2. **The filesystem may not support it.** Not an issue for block devices,
//!    but worth not assuming.
//!
//! Both are caught the same way: by proving direct I/O is in force rather than
//! trusting that the flag took. See [`direct_io_is_in_force`].

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use crate::model::{BlockStats, Snapshot, ThroughputSample};

/// `O_DIRECT` for architectures where the value is known.
///
/// `None` means this build must not attempt the measurement at all. Guessing
/// would not produce an error — Linux drops unrecognised `open` flags without
/// complaint — it would produce a page-cache number presented as a link
/// measurement.
pub const O_DIRECT: Option<i32> = o_direct();

const fn o_direct() -> Option<i32> {
    #[cfg(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv32",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64",
    ))]
    {
        Some(0o0040000)
    }
    #[cfg(any(target_arch = "powerpc", target_arch = "powerpc64"))]
    {
        Some(0o0400000)
    }
    #[cfg(not(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv32",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
    )))]
    {
        None
    }
}

/// Direct I/O demands the buffer address, file offset and length all be
/// aligned. 4096 is a superset of every logical block size in practice.
const ALIGN: usize = 4096;

/// Per-read size. Large enough that per-request overhead disappears, small
/// enough that a 5 GB/s NVMe behind a bridge still yields several samples.
const CHUNK: usize = 4 * 1024 * 1024;

/// `stat` counts in 512-byte units, whatever the device's real block size.
const STAT_SECTOR_BYTES: u64 = 512;

/// Read a block device as fast as it will go, for a bounded time.
///
/// Read-only and non-destructive: it reads from offset zero forward and writes
/// nothing. Nothing is unmounted, no driver is detached, and the device stays
/// on the bus throughout.
pub fn measure(name: &str, window: Duration) -> io::Result<ThroughputSample> {
    let Some(flag) = O_DIRECT else {
        return Err(io::Error::other(
            "O_DIRECT's value is not known for this architecture, and reading without it \
             would measure the page cache rather than the link",
        ));
    };
    let node = PathBuf::from("/dev").join(name);

    let file = OpenOptions::new().read(true).custom_flags(flag).open(&node)?;
    if !direct_io_is_in_force(&file) {
        return Err(io::Error::other(format!(
            "{} accepted O_DIRECT but did not honour it — any number from here would be the \
             page cache, not the device",
            node.display()
        )));
    }

    // Counters either side of the window, so traffic that was not ours can be
    // subtracted out. A drive that is busy serving something else measures
    // slow for a reason that has nothing to do with its cable.
    let sysfs = Path::new("/sys/class/block").join(name);
    let before = crate::block::stats_at(&sysfs);

    let mut buf = AlignedBuf::new(CHUNK)
        .ok_or_else(|| io::Error::other("could not allocate an aligned read buffer"))?;

    let started = Instant::now();
    let mut offset: u64 = 0;
    let mut error = None;

    while started.elapsed() < window {
        match file.read_at(buf.as_mut_slice(), offset) {
            Ok(0) => break,
            Ok(n) => {
                offset += n as u64;
                // A short read means the end of the device. Continuing would
                // leave the offset unaligned, which direct I/O rejects.
                if n < CHUNK {
                    break;
                }
            }
            Err(e) => {
                // Worth keeping rather than discarding: a read error partway
                // through a healthy-looking drive is itself the diagnosis.
                error = Some(format!("read failed at offset {offset}: {e}"));
                break;
            }
        }
    }

    let elapsed = started.elapsed();
    let after = crate::block::stats_at(&sysfs);
    let contended_bytes = other_traffic(before, after, offset);

    Ok(ThroughputSample {
        device: name.to_string(),
        bytes_read: offset,
        elapsed_ms: elapsed.as_millis() as u64,
        bytes_per_second: (elapsed.as_secs_f64() > 0.0 && offset > 0)
            .then(|| offset as f64 / elapsed.as_secs_f64()),
        contended_bytes,
        error,
    })
}

/// Bytes the device served to somebody else while we were measuring.
///
/// Returns `None` when the counters could not be read at both ends, so
/// "nobody else was reading" is never claimed on the strength of a missing
/// file.
fn other_traffic(before: Option<BlockStats>, after: Option<BlockStats>, ours: u64) -> Option<u64> {
    let (before, after) = (before?, after?);
    let total = after
        .sectors_read
        .checked_sub(before.sectors_read)?
        .saturating_mul(STAT_SECTOR_BYTES);
    // Our own reads are in that total. Read-ahead and rounding can make the
    // kernel's count slightly exceed ours, so this floors at zero rather than
    // reporting a negative amount of contention.
    Some(total.saturating_sub(ours))
}

/// Prove direct I/O is actually in force.
///
/// Setting the flag is not proof of anything: Linux discards `open` flag bits
/// it does not recognise without an error, so a value that is right for x86 and
/// wrong for this machine looks exactly like success. The decisive test is a
/// deliberately misaligned read — one byte at offset one. Direct I/O rejects it
/// with `EINVAL`; buffered I/O returns the byte. If that read succeeds, the
/// page cache is serving us and no number from this file means anything.
fn direct_io_is_in_force(file: &File) -> bool {
    let mut one = [0u8; 1];
    file.read_at(&mut one, 1).is_err()
}

/// Every USB-attached disk worth measuring, narrowed to a target if given.
///
/// Only USB-attached devices: measuring the machine's internal NVMe would
/// answer a question nobody asked and take the wall-clock to do it.
pub fn targets(snap: &Snapshot, only: Option<&[String]>) -> Vec<String> {
    snap.block_devices
        .iter()
        .filter(|b| b.is_usb_attached())
        .filter(|b| only.is_none_or(|names| names.contains(&b.name)))
        .map(|b| b.name.clone())
        .collect()
}

/// A heap buffer aligned for direct I/O.
///
/// `Vec<u8>` cannot promise the alignment direct I/O requires — its allocation
/// is only aligned to `u8` — so the allocation is made by hand. Zeroed rather
/// than raw so no uninitialised memory is ever exposed as `&mut [u8]`.
struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize) -> Option<Self> {
        let layout = Layout::from_size_align(size, ALIGN).ok()?;
        // SAFETY: the layout has a non-zero size, which is `alloc_zeroed`'s
        // one requirement. A null return is handled by `NonNull::new`.
        let ptr = unsafe { alloc_zeroed(layout) };
        NonNull::new(ptr).map(|ptr| Self { ptr, layout })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the pointer came from a successful allocation of exactly
        // this layout, the memory was zeroed so every byte is initialised, and
        // `&mut self` makes this the only live reference to it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: same pointer and same layout as the allocation, freed once.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_buffer_is_aligned_zeroed_and_the_right_size() {
        let mut buf = AlignedBuf::new(CHUNK).unwrap();
        assert_eq!(buf.ptr.as_ptr() as usize % ALIGN, 0, "direct I/O needs this");
        let slice = buf.as_mut_slice();
        assert_eq!(slice.len(), CHUNK);
        assert!(slice.iter().all(|b| *b == 0));
        slice[CHUNK - 1] = 0xAA;
        assert_eq!(buf.as_mut_slice()[CHUNK - 1], 0xAA);
    }

    /// The check that decides whether any measurement can be believed.
    ///
    /// An ordinary file opened without the flag must be reported as *not*
    /// direct, because a misaligned read of it succeeds. Getting this backwards
    /// would let the page cache be reported as link throughput.
    #[test]
    fn buffered_io_is_never_mistaken_for_direct() {
        let path = std::env::temp_dir().join(format!("usbprobe-odirect-{}", std::process::id()));
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0u8; 8192]).unwrap();
        drop(f);

        let buffered = File::open(&path).unwrap();
        assert!(
            !direct_io_is_in_force(&buffered),
            "a misaligned read succeeded, so this file is not on direct I/O"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The positive half of the same check, and the only thing that proves the
    /// `O_DIRECT` constant is right for the architecture this is built for.
    ///
    /// A wrong value would not fail: Linux drops `open` flag bits it does not
    /// recognise without a word, and the reads would quietly come from the page
    /// cache. So the flag is set on a real file and the misaligned read must
    /// then be *rejected*. Skips where the filesystem has no direct I/O — tmpfs
    /// does not — rather than failing for the wrong reason.
    #[test]
    fn direct_io_is_detected_when_it_is_actually_on() {
        let Some(flag) = O_DIRECT else {
            return; // Nothing to prove on an architecture we refuse to guess at.
        };
        let path = std::env::temp_dir().join(format!("usbprobe-direct-{}", std::process::id()));
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0u8; 65536]).unwrap();
        drop(f);

        if let Ok(direct) = OpenOptions::new().read(true).custom_flags(flag).open(&path) {
            assert!(
                direct_io_is_in_force(&direct),
                "O_DIRECT was accepted but a misaligned read still succeeded — the constant \
                 is wrong for this architecture and every measurement would be the page cache"
            );
            // An aligned read through the same handle must still work, or the
            // measurement loop could never make progress.
            let mut buf = AlignedBuf::new(ALIGN).unwrap();
            assert_eq!(direct.read_at(buf.as_mut_slice(), 0).unwrap(), ALIGN);
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Contention is only claimed when both counter reads succeeded, and never
    /// as a negative number.
    #[test]
    fn contention_subtracts_our_own_reads() {
        let base = BlockStats {
            read_ios: 0,
            sectors_read: 1_000,
            ms_reading: 0,
            write_ios: 0,
            sectors_written: 0,
            ms_writing: 0,
            ios_in_flight: 0,
            sampled_at_unix_ms: 0,
        };
        // 200_000 sectors = 100 MB total, of which 90 MB was ours.
        let after = BlockStats {
            sectors_read: 1_000 + 200_000,
            ..base
        };
        assert_eq!(other_traffic(Some(base), Some(after), 90_000_000), Some(12_400_000));

        // Read-ahead can make the kernel's count trail ours; that is not
        // negative contention.
        assert_eq!(other_traffic(Some(base), Some(after), 200_000_000), Some(0));
        // A missing counter is unknown, not zero.
        assert_eq!(other_traffic(None, Some(after), 0), None);
        assert_eq!(other_traffic(Some(base), None, 0), None);
    }

    /// Only USB storage, and only the named disk when one was named.
    #[test]
    fn targets_are_usb_disks_and_respect_a_filter() {
        let mut snap = crate::test_support::empty_snapshot();
        for (name, path) in [
            ("sda", "/sys/devices/pci/usb4/4-1/host1/block/sda"),
            ("sdb", "/sys/devices/pci/usb5/5-1/host2/block/sdb"),
            ("nvme0n1", "/sys/devices/pci/nvme/block/nvme0n1"),
        ] {
            snap.block_devices.push(crate::model::BlockDevice {
                name: name.into(),
                sysfs_path: PathBuf::from(path),
                model: None,
                vendor: None,
                size_bytes: None,
                rotational: None,
                removable: None,
                stats: None,
                throughput: None,
                scsi: None,
                scsi_delta: None,
            });
        }
        assert_eq!(targets(&snap, None), ["sda", "sdb"], "internal disks are not ours");
        assert_eq!(targets(&snap, Some(&["sdb".to_string()])), ["sdb"]);
        assert!(targets(&snap, Some(&["nvme0n1".to_string()])).is_empty());
    }

    /// The whole thing against the real machine, when there is a disk to read
    /// and the privilege to read it. Skips rather than fails otherwise, since
    /// neither is true in CI.
    #[test]
    fn measuring_a_real_disk_produces_a_plausible_rate() {
        let snap = crate::capture(crate::Options::default());
        let Some(name) = targets(&snap, None).into_iter().next() else {
            return;
        };
        let Ok(sample) = measure(&name, Duration::from_millis(300)) else {
            // Not root, almost certainly.
            return;
        };
        assert!(sample.bytes_read > 0, "{sample:?}");
        let bps = sample.bytes_per_second.unwrap();
        // Loose bounds on purpose. The point is to catch a page-cache number,
        // which would be tens of GB/s, not to assert a speed.
        assert!(bps > 100e3, "implausibly slow, likely broken: {bps}");
        assert!(bps < 3e9, "implausibly fast — is this the page cache? {bps}");
    }
}
