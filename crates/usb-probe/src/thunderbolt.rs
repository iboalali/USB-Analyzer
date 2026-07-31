//! Reads `/sys/bus/thunderbolt` — USB4 and Thunderbolt routers, and retimers.
//!
//! This is a second, independent cable-information path, and on some hardware it
//! is the only one that works. `/sys/class/typec` exposes a cable's e-marker over
//! PD SOP', which many platforms never report; the machine this was developed
//! against never does. Retimers come from elsewhere entirely.
//!
//! A **retimer** is the signal-conditioning silicon inside an *active* cable.
//! Active cables are required to be e-marked and are the norm for Thunderbolt
//! and USB4 above roughly 0.8 m. When such a cable is attached, the kernel
//! enumerates its retimers with a vendor, device and NVM version — the same
//! "Cable Firmware Version" macOS shows in System Information. A firmware
//! version can only come from silicon inside the cable, so it is genuine cable
//! identity rather than an inference.
//!
//! Layout:
//!
//! ```text
//! /sys/bus/thunderbolt/devices/domain0            a controller domain
//! /sys/bus/thunderbolt/devices/0-0                a router (0-0 is the host)
//! /sys/bus/thunderbolt/devices/0-0/usb4_port1     a USB4 port on that router
//! /sys/bus/thunderbolt/devices/0-0:1.1            a retimer on port 1
//! ```

use std::path::Path;

use crate::model::{ThunderboltDomain, ThunderboltRouter, ThunderboltTopology, Retimer};
use crate::sysfs as fsx;

const TB_DEVICES: &str = "/sys/bus/thunderbolt/devices";

pub fn read() -> ThunderboltTopology {
    read_from(Path::new(TB_DEVICES))
}

pub fn read_from(dir: &Path) -> ThunderboltTopology {
    let mut domains = Vec::new();
    let mut routers = Vec::new();
    let mut retimers = Vec::new();

    for entry in fsx::list_dir(dir) {
        if !entry.is_dir() {
            continue;
        }
        let name = fsx::file_name(&entry);
        if name.starts_with("domain") {
            domains.push(ThunderboltDomain {
                security: fsx::read_attr(&entry, "security"),
                iommu_dma_protection: fsx::read_bool(&entry, "iommu_dma_protection"),
                deauthorization: fsx::read_bool(&entry, "deauthorization"),
                name,
            });
        } else if name.contains(':') {
            // `<domain>-<route>:<port>.<index>` — a retimer, i.e. active-cable
            // silicon. Its mere presence means an active cable is attached.
            retimers.push(Retimer {
                vendor: fsx::read_hex_u16(&entry, "vendor"),
                device: fsx::read_hex_u16(&entry, "device"),
                nvm_version: fsx::read_attr(&entry, "nvm_version"),
                nvm_authenticate: fsx::read_attr(&entry, "nvm_authenticate"),
                name,
            });
        } else if name.contains('-') {
            let uevent = fsx::read_attr(&entry, "uevent").unwrap_or_default();
            routers.push(ThunderboltRouter {
                // `0-0` is the host router; anything else is downstream.
                is_host: uevent.contains("USB4_TYPE=host") || name.ends_with("-0"),
                generation: fsx::read_u32(&entry, "generation"),
                usb4_version: uevent
                    .lines()
                    .find_map(|l| l.strip_prefix("USB4_VERSION="))
                    .map(str::to_string),
                vendor_name: fsx::read_attr(&entry, "vendor_name"),
                device_name: fsx::read_attr(&entry, "device_name"),
                unique_id: fsx::read_attr(&entry, "unique_id"),
                authorized: fsx::read_bool(&entry, "authorized"),
                rx_speed: fsx::read_attr(&entry, "rx_speed"),
                tx_speed: fsx::read_attr(&entry, "tx_speed"),
                rx_lanes: fsx::read_u32(&entry, "rx_lanes"),
                tx_lanes: fsx::read_u32(&entry, "tx_lanes"),
                nvm_version: fsx::read_attr(&entry, "nvm_version"),
                usb4_ports: fsx::list_subdirs(&entry)
                    .iter()
                    .map(|p| fsx::file_name(p))
                    .filter(|n| n.starts_with("usb4_port"))
                    .collect(),
                name,
            });
        }
    }

    domains.sort_by(|a, b| a.name.cmp(&b.name));
    routers.sort_by(|a, b| a.name.cmp(&b.name));
    retimers.sort_by(|a, b| a.name.cmp(&b.name));

    ThunderboltTopology {
        domains,
        routers,
        retimers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_host_routers_domains_and_retimers() {
        let base = std::env::temp_dir().join(format!("usbprobe-tb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        // Values transcribed from a real USB4 host router.
        let host = base.join("0-0");
        fs::create_dir_all(host.join("usb4_port2")).unwrap();
        fs::write(host.join("generation"), "4\n").unwrap();
        fs::write(host.join("authorized"), "1\n").unwrap();
        fs::write(
            host.join("unique_id"),
            "cb083804-6028-2062-ffff-ffffffffffff\n",
        )
        .unwrap();
        fs::write(
            host.join("uevent"),
            "DEVTYPE=thunderbolt_device\nUSB4_VERSION=1.0\nUSB4_TYPE=host\n",
        )
        .unwrap();

        let domain = base.join("domain0");
        fs::create_dir_all(&domain).unwrap();
        fs::write(domain.join("security"), "user\n").unwrap();
        fs::write(domain.join("iommu_dma_protection"), "1\n").unwrap();

        // An active cable's retimer.
        let retimer = base.join("0-0:1.1");
        fs::create_dir_all(&retimer).unwrap();
        fs::write(retimer.join("vendor"), "0x8087\n").unwrap();
        fs::write(retimer.join("device"), "0x15ee\n").unwrap();
        fs::write(retimer.join("nvm_version"), "1.20\n").unwrap();

        let tb = read_from(&base);

        assert_eq!(tb.routers.len(), 1);
        let r = &tb.routers[0];
        assert!(r.is_host);
        assert_eq!(r.generation, Some(4));
        assert_eq!(r.usb4_version.as_deref(), Some("1.0"));
        assert_eq!(r.usb4_ports, vec!["usb4_port2"]);
        assert!(r.unique_id.as_deref().unwrap().starts_with("cb083804"));

        assert_eq!(tb.domains.len(), 1);
        assert_eq!(tb.domains[0].security.as_deref(), Some("user"));
        assert_eq!(tb.domains[0].iommu_dma_protection, Some(true));

        assert_eq!(tb.retimers.len(), 1, "retimer must not be read as a router");
        assert_eq!(tb.retimers[0].nvm_version.as_deref(), Some("1.20"));
        assert_eq!(tb.retimers[0].vendor, Some(0x8087));
        assert!(tb.has_active_cable());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn absent_bus_yields_an_empty_topology() {
        let tb = read_from(Path::new("/nonexistent/thunderbolt"));
        assert!(tb.is_empty());
        assert!(!tb.has_active_cable());
    }

    /// A machine with USB4 but nothing attached: routers and domains, no
    /// retimers. This is the state of the development machine.
    #[test]
    fn host_only_topology_reports_no_active_cable() {
        let base = std::env::temp_dir().join(format!("usbprobe-tb2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for r in ["0-0", "1-0"] {
            let d = base.join(r);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("generation"), "4\n").unwrap();
            fs::write(d.join("uevent"), "USB4_VERSION=1.0\nUSB4_TYPE=host\n").unwrap();
        }
        let tb = read_from(&base);
        assert_eq!(tb.routers.len(), 2);
        assert!(tb.routers.iter().all(|r| r.is_host));
        assert!(!tb.has_active_cable());
        assert!(!tb.is_empty());

        let _ = fs::remove_dir_all(&base);
    }
}
