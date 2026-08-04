//! Cycling a hub port to catch a link that only *sometimes* works.
//!
//! Every other probe answers a question about now. This one is the only thing
//! here that can find intermittency: a cable that trains SuperSpeed sixteen
//! times out of twenty is failing in a way no single reading can reveal,
//! because each individual reading looks fine. Twenty attempts and a
//! distribution is the whole method.
//!
//! # How the port is cycled
//!
//! Writing `1` then `0` to the hub port's `disable` attribute. The alternative
//! is `USBDEVFS_RESET`, which needs `ioctl` and therefore libc — refused here
//! for the same reason it is refused in [`crate::throughput`].
//!
//! The port is found by reading the device's `port` symlink rather than by
//! deriving a path from its name. Two things fall out of that for free: hub
//! interface numbering never has to be guessed at, and a **root hub has no
//! `port` symlink**, so asking to cycle one — which would drop every device on
//! that bus at once — fails at the first step instead of needing a special
//! case.
//!
//! # What cannot be guaranteed
//!
//! The port is restored by a guard whose `Drop` runs on every normal and error
//! path, and on a panic. It does **not** run if the process is killed: handling
//! `SIGINT` would need a signal handler, which needs libc. The disabled window
//! is therefore kept to a fraction of a second, and the exact command to undo a
//! stuck port is printed before anything happens. That is the honest limit —
//! see [`PortRef::recovery_command`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{ReenumerationCycle, ReenumerationRun, Snapshot};
use crate::sysfs as fsx;

/// How long the port is held down. Long enough for the hub to notice, short
/// enough that the unprotected window barely exists.
const DOWN: Duration = Duration::from_millis(150);

/// How long to wait for the device to come back before calling the cycle a
/// failure. Generous: a slow enclosure spinning up can take seconds.
const RETURN_TIMEOUT: Duration = Duration::from_secs(6);

/// Polling interval while waiting for re-enumeration.
const POLL: Duration = Duration::from_millis(25);

const SYSFS_DEVICES: &str = "/sys/bus/usb/devices";

/// A downstream hub port, and the file that switches it off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRef {
    /// Resolved port directory, e.g. `.../usb4/4-0:1.0/usb4-port1`.
    pub dir: PathBuf,
    pub disable: PathBuf,
    /// Short name for display, e.g. `usb4-port1`.
    pub label: String,
}

impl PortRef {
    /// What to run by hand if this process dies with the port still down.
    ///
    /// Printed before cycling starts rather than after something goes wrong,
    /// because after something goes wrong is exactly when it cannot be printed.
    pub fn recovery_command(&self) -> String {
        format!("echo 0 | sudo tee {}", self.disable.display())
    }
}

/// The hub port a device is plugged into, via its own `port` symlink.
///
/// `None` for a root hub, which has no upstream port — and cycling one would
/// mean dropping every device on the bus.
pub fn port_for(sysfs_name: &str) -> Option<PortRef> {
    port_for_in(Path::new(SYSFS_DEVICES), sysfs_name)
}

pub fn port_for_in(devices: &Path, sysfs_name: &str) -> Option<PortRef> {
    let dir = fsx::canonicalize(devices.join(sysfs_name).join("port"))?;
    let disable = dir.join("disable");
    if !disable.exists() {
        return None;
    }
    Some(PortRef {
        label: fsx::file_name(&dir),
        dir,
        disable,
    })
}

/// Take the port down and up, repeatedly, recording what came back each time.
///
/// The port is restored before returning whatever happens — including when a
/// cycle fails partway and when the device never reappears.
pub fn cycle(
    port: &PortRef,
    sysfs_name: &str,
    cycles: usize,
    cancel: &crate::cancel::Cancel,
) -> ReenumerationRun {
    cycle_in(Path::new(SYSFS_DEVICES), port, sysfs_name, cycles, cancel)
}

pub fn cycle_in(
    devices: &Path,
    port: &PortRef,
    sysfs_name: &str,
    cycles: usize,
    cancel: &crate::cancel::Cancel,
) -> ReenumerationRun {
    let device_dir = devices.join(sysfs_name);
    let mut run = ReenumerationRun {
        device: sysfs_name.to_string(),
        port: port.label.clone(),
        port_path: port.dir.clone(),
        requested_cycles: cycles,
        cycles: Vec::new(),
        stopped: false,
        error: None,
    };

    // Armed for the whole run, not per cycle: if anything below panics or
    // returns early, the port still comes back up.
    let _guard = PortGuard {
        disable: &port.disable,
    };

    for index in 0..cycles {
        // Checked *between* cycles and never inside one. A cycle writes `1` and
        // then `0` to the port; stopping between those two writes would leave the
        // port disabled, which is the one outcome this probe must never produce.
        // Cooperation buys exactly that guarantee.
        if cancel.stopped() {
            run.stopped = true;
            break;
        }
        run.cycles
            .push(one_cycle(&port.disable, &device_dir, index));
    }
    run
}

fn one_cycle(disable: &Path, device_dir: &Path, index: usize) -> ReenumerationCycle {
    let mut cycle = ReenumerationCycle {
        index,
        ..Default::default()
    };

    if let Err(e) = std::fs::write(disable, "1") {
        cycle.error = Some(format!("could not disable the port: {e}"));
        return cycle;
    }
    std::thread::sleep(DOWN);
    if let Err(e) = std::fs::write(disable, "0") {
        cycle.error = Some(format!("could not re-enable the port: {e}"));
        return cycle;
    }

    let started = Instant::now();
    loop {
        // `speed` appearing is the signal, not the directory: sysfs publishes
        // the device node before enumeration has finished, so waiting on the
        // directory alone would read a link rate that is not settled yet.
        if let Some(mbps) = fsx::read_f64(device_dir, "speed") {
            cycle.returned = true;
            cycle.returned_after_ms = started.elapsed().as_millis() as u64;
            cycle.speed_mbps = Some(mbps);
            cycle.rx_lanes = fsx::read_u32(device_dir, "rx_lanes");
            cycle.tx_lanes = fsx::read_u32(device_dir, "tx_lanes");
            return cycle;
        }
        if started.elapsed() >= RETURN_TIMEOUT {
            cycle.returned_after_ms = started.elapsed().as_millis() as u64;
            cycle.error = Some(format!(
                "the device did not come back within {:.0}s",
                RETURN_TIMEOUT.as_secs_f64()
            ));
            return cycle;
        }
        std::thread::sleep(POLL);
    }
}

/// Puts the port back however the run ends.
///
/// Covers returns, `?`, and panics. Not signals — see the module docs.
struct PortGuard<'a> {
    disable: &'a Path,
}

impl Drop for PortGuard<'_> {
    fn drop(&mut self) {
        // Writing 0 to an already-enabled port is a no-op, so this is safe to
        // do unconditionally rather than tracking whether we left it down.
        let _ = std::fs::write(self.disable, "0");
    }
}

/// Devices in a subtree whose loss would take away the user's ability to stop
/// this, or to notice that it went wrong.
///
/// Input devices are the case worth being absolute about: disable the port a
/// keyboard is on and the interrupt key is gone too.
pub fn input_devices(snap: &Snapshot, sysfs_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for dev in snap.subtree(sysfs_name) {
        for iface in &dev.interfaces {
            // Class 3 is HID. Boot protocol 1 is a keyboard and 2 a mouse;
            // 0 covers everything else that speaks HID, which is still likely
            // to be something the user is holding.
            if iface.class != Some(0x03) {
                continue;
            }
            let kind = match iface.protocol {
                Some(1) => "a keyboard",
                Some(2) => "a pointing device",
                _ => "a human interface device",
            };
            out.push(format!("{} ({}) is {kind}", dev.label(), dev.sysfs_name));
            break;
        }
    }
    out
}

/// Things in a subtree that will drop when the port goes down, and come back
/// afterwards.
///
/// Not refusals — warnings, listed in the confirmation so that the second yes
/// is given knowing what it costs. A network interface here may be carrying
/// the session asking for the probe.
pub fn side_effects(snap: &Snapshot, sysfs_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let subtree = snap.subtree(sysfs_name);

    for dev in &subtree {
        let disks: Vec<&str> = snap
            .storage_on(dev)
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        if !disks.is_empty() {
            out.push(format!(
                "{} carries {} — any read or write in flight will fail",
                dev.sysfs_name,
                disks.join(", ")
            ));
        }
    }

    for (interface, dev) in up_network_interfaces(&subtree) {
        out.push(format!(
            "{interface} on {dev} is up — if this session is running over it, it will drop"
        ));
    }
    out.dedup();
    out
}

/// Network interfaces that are up and hang off one of these devices.
///
/// Read from `/sys/class/net` at the moment of asking rather than from the
/// snapshot: an interface can come up between capture and probe.
fn up_network_interfaces(subtree: &[&crate::model::UsbDevice]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in fsx::list_dir("/sys/class/net") {
        let name = fsx::file_name(&entry);
        let Some(device) = fsx::canonicalize(entry.join("device")) else {
            continue;
        };
        let state = fsx::read_attr(&entry, "operstate").unwrap_or_default();
        if state == "down" {
            continue;
        }
        if let Some(dev) = subtree.iter().find(|d| device.starts_with(&d.sysfs_path)) {
            out.push((name, dev.sysfs_name.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsbInterface;
    use crate::test_support as ts;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("usbprobe-reenum-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// Build a fake sysfs with the same two levels of indirection the real one
    /// has: `/sys/bus/usb/devices/4-1` is a symlink into `/sys/devices`, and
    /// the device's `port` link is relative to where it *really* lives. A flat
    /// fixture resolves to the wrong place and proves nothing.
    fn fake_sysfs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = scratch(tag);
        let real = root.join("sys/devices/usb4");
        let devices = root.join("sys/bus/usb/devices");
        fs::create_dir_all(real.join("4-0:1.0/usb4-port1")).unwrap();
        fs::create_dir_all(real.join("4-1")).unwrap();
        fs::create_dir_all(&devices).unwrap();

        let disable = real.join("4-0:1.0/usb4-port1/disable");
        fs::write(&disable, "0\n").unwrap();
        std::os::unix::fs::symlink("../4-0:1.0/usb4-port1", real.join("4-1/port")).unwrap();
        std::os::unix::fs::symlink(&real, devices.join("usb4")).unwrap();
        std::os::unix::fs::symlink(real.join("4-1"), devices.join("4-1")).unwrap();

        (root, devices, disable)
    }

    /// A device finds its port through its own symlink, and a root hub — which
    /// has none — finds nothing. That second half is the safety property: a
    /// root hub carries every device on its bus, so cycling one would drop them
    /// all at once.
    #[test]
    fn a_port_is_found_by_symlink_and_a_root_hub_has_none() {
        let (root, devices, _) = fake_sysfs("port");

        let port = port_for_in(&devices, "4-1").unwrap();
        assert_eq!(port.label, "usb4-port1");
        assert!(port.disable.ends_with("usb4-port1/disable"));
        assert!(port.recovery_command().contains("echo 0"));

        // No `port` symlink: nothing to cycle, and nothing special to say.
        assert_eq!(port_for_in(&devices, "usb4"), None);
        assert_eq!(port_for_in(&devices, "4-9"), None);

        let _ = fs::remove_dir_all(&root);
    }

    /// The port must come back up even when the device never does — and the
    /// guard, not the happy path, is what has to guarantee it.
    /// A stop asked for before the run starts must take no cycles at all, mark
    /// the run, and still leave the port up.
    ///
    /// The port matters more than the count. A probe that could be interrupted
    /// mid-cycle would leave a device switched off, which is why the check sits
    /// between cycles and never inside one.
    #[test]
    fn a_stop_takes_effect_between_cycles_and_leaves_the_port_up() {
        let (root, devices, disable) = fake_sysfs("cancelled");
        let port = port_for_in(&devices, "4-1").unwrap();

        let cancel = crate::cancel::Cancel::never();
        cancel.stop();
        // Twenty requested; the timeout is six seconds per cycle, so if the stop
        // were ignored this test would take two minutes rather than fail fast.
        let run = cycle_in(&devices, &port, "4-1", 20, &cancel);

        assert!(run.cycles.is_empty(), "no cycle should have been attempted");
        assert!(run.stopped, "and the run must say it was cut short");
        assert_eq!(run.requested_cycles, 20, "what was asked for is kept");
        assert_eq!(
            fs::read_to_string(&disable).unwrap().trim(),
            "0",
            "the port must be enabled whatever happened"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_port_is_restored_even_when_the_device_never_returns() {
        let (root, devices, disable) = fake_sysfs("restore");
        let port = port_for_in(&devices, "4-1").unwrap();
        // No `speed` file is ever created, so every cycle times out. One cycle
        // only: the timeout is deliberately generous.
        let run = cycle_in(&devices, &port, "4-1", 1, &crate::cancel::Cancel::never());

        assert_eq!(run.cycles.len(), 1);
        assert!(!run.cycles[0].returned);
        assert!(run.cycles[0]
            .error
            .as_ref()
            .unwrap()
            .contains("did not come back"));
        assert_eq!(run.failures(), 1);
        assert_eq!(
            fs::read_to_string(&disable).unwrap().trim(),
            "0",
            "the guard must leave the port enabled"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A panic mid-run must not leave a port down.
    #[test]
    fn the_guard_restores_on_panic() {
        let root = scratch("panic");
        fs::create_dir_all(&root).unwrap();
        let disable = root.join("disable");
        fs::write(&disable, "0\n").unwrap();

        let result = std::panic::catch_unwind(|| {
            let _guard = PortGuard { disable: &disable };
            fs::write(&disable, "1").unwrap();
            panic!("something went wrong mid-cycle");
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&disable).unwrap().trim(), "0");

        let _ = fs::remove_dir_all(&root);
    }

    /// Anything the user might be typing on is named, at any depth.
    #[test]
    fn input_devices_are_found_anywhere_in_the_subtree() {
        let mut snap = ts::empty_snapshot();
        let mut bus = ts::root_hub("usb3", 480.0);
        let mut hub = ts::device("3-5", " 2.10", 480.0, Some("usb3"));

        let mut kbd = ts::device("3-5.1", " 2.00", 12.0, Some("3-5"));
        kbd.product = Some("Compact Keyboard".into());
        kbd.interfaces.push(UsbInterface {
            sysfs_name: "3-5.1:1.0".into(),
            number: Some(0),
            class: Some(0x03),
            subclass: Some(1),
            protocol: Some(1),
            driver: Some("usbhid".into()),
            description: None,
        });

        let mut stick = ts::device("3-5.2", " 2.10", 480.0, Some("3-5"));
        stick.interfaces.push(UsbInterface {
            sysfs_name: "3-5.2:1.0".into(),
            number: Some(0),
            class: Some(0x08),
            subclass: Some(6),
            protocol: Some(0x50),
            driver: Some("usb-storage".into()),
            description: None,
        });

        hub.children.push(kbd);
        hub.children.push(stick);
        bus.children.push(hub);
        snap.buses.push(bus);

        // Cycling the hub takes the keyboard with it, two levels down.
        let found = input_devices(&snap, "3-5");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("a keyboard"), "{found:?}");
        assert!(found[0].contains("3-5.1"), "{found:?}");

        // The storage device alone is not an input device.
        assert!(input_devices(&snap, "3-5.2").is_empty());
        // And the keyboard is found when named directly.
        assert_eq!(input_devices(&snap, "3-5.1").len(), 1);
    }
}
