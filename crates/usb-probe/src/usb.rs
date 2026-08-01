//! Reads the USB device tree from `/sys/bus/usb/devices`.
//!
//! The class directory is a flat set of symlinks; the real hierarchy lives in
//! the symlink targets under `/sys/devices/...`. We resolve each link and use
//! path nesting to recover the parent/child relationships, which is more robust
//! than parsing the `3-5.1.2` naming convention by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::{HubPort, LinkSpeed, PhysicalLocation, UsbDevice, UsbInterface};
use crate::sysfs as fsx;

const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Read every bus (root hub) with its full device tree.
pub fn read_buses() -> Vec<UsbDevice> {
    read_buses_from(Path::new(USB_DEVICES))
}

pub fn read_buses_from(root: &Path) -> Vec<UsbDevice> {
    // Entries containing ':' are interfaces, not devices.
    let mut flat: BTreeMap<String, (PathBuf, PathBuf)> = BTreeMap::new();
    for entry in fsx::list_dir(root) {
        let name = fsx::file_name(&entry);
        if name.contains(':') {
            continue;
        }
        let Some(real) = fsx::canonicalize(&entry) else {
            continue;
        };
        flat.insert(name, (entry, real));
    }

    // Parent = nearest ancestor directory that is itself a known USB device.
    let mut children_of: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    let mut parent_of: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (name, (_, real)) in &flat {
        let parent = real
            .parent()
            .map(fsx::file_name)
            .filter(|p| flat.contains_key(p));
        parent_of.insert(name.clone(), parent.clone());
        children_of.entry(parent).or_default().push(name.clone());
    }

    let roots = children_of.get(&None).cloned().unwrap_or_default();
    roots
        .iter()
        .filter_map(|name| build(name, &flat, &children_of, &parent_of))
        .collect()
}

fn build(
    name: &str,
    flat: &BTreeMap<String, (PathBuf, PathBuf)>,
    children_of: &BTreeMap<Option<String>, Vec<String>>,
    parent_of: &BTreeMap<String, Option<String>>,
) -> Option<UsbDevice> {
    let (link, real) = flat.get(name)?;
    let mut dev = read_device(name, link, real);
    dev.parent = parent_of.get(name).cloned().flatten();
    dev.children = children_of
        .get(&Some(name.to_string()))
        .map(|kids| {
            kids.iter()
                .filter_map(|k| build(k, flat, children_of, parent_of))
                .collect()
        })
        .unwrap_or_default();
    Some(dev)
}

fn read_device(name: &str, link: &Path, real: &Path) -> UsbDevice {
    let version = fsx::read_attr(link, "version");
    let version_num = version.as_deref().and_then(|v| v.trim().parse::<f32>().ok());
    let bm_attributes = fsx::read_hex_u8(link, "bmAttributes");

    UsbDevice {
        sysfs_name: name.to_string(),
        sysfs_path: real.to_path_buf(),
        is_root_hub: name.starts_with("usb"),
        parent: None,
        busnum: fsx::read_u32(link, "busnum"),
        devnum: fsx::read_u32(link, "devnum"),

        id_vendor: fsx::read_hex_u16(link, "idVendor"),
        id_product: fsx::read_hex_u16(link, "idProduct"),
        manufacturer: fsx::read_attr(link, "manufacturer"),
        product: fsx::read_attr(link, "product"),
        serial: fsx::read_attr(link, "serial"),

        usb_version: version.map(|v| v.trim().to_string()),
        usb_version_num: version_num,
        speed: fsx::read_f64(link, "speed").map(LinkSpeed::from_mbps),
        rx_lanes: fsx::read_u32(link, "rx_lanes"),
        tx_lanes: fsx::read_u32(link, "tx_lanes"),

        max_power_ma: fsx::read_suffixed(link, "bMaxPower", "mA"),
        // bmAttributes bit 6 = self-powered, bit 5 = remote wakeup.
        self_powered: bm_attributes.map(|b| b & 0x40 != 0),
        remote_wakeup: bm_attributes.map(|b| b & 0x20 != 0),

        device_class: fsx::read_hex_u8(link, "bDeviceClass"),
        max_children: fsx::read_u32(link, "maxchild"),
        removable: fsx::read_attr(link, "removable"),
        authorized: fsx::read_flag(link, "authorized"),

        urbnum: fsx::read_u64(link, "urbnum"),
        // Runtime-PM accounting lives in a power/ subdirectory and is already
        // in milliseconds.
        active_duration_ms: fsx::read_u64(link.join("power"), "active_duration"),
        connected_duration_ms: fsx::read_u64(link.join("power"), "connected_duration"),
        runtime_suspended_ms: fsx::read_u64(link.join("power"), "runtime_suspended_time"),
        power_control: fsx::read_attr(link.join("power"), "control"),
        autosuspend_delay_ms: fsx::read_i64(link.join("power"), "autosuspend_delay_ms"),

        interfaces: read_interfaces(name, real),
        ports: read_hub_ports(name, real),
        children: Vec::new(),
        // Filled in after the tree is read — see `apply_overrides` in lib.rs.
        declared: None,
    }
}

/// Downstream port directories, which live inside the hub's own interface dir
/// and are named after the *hub*: `.../3-5:1.0/3-5-port1`.
fn read_hub_ports(dev_name: &str, real: &Path) -> Vec<HubPort> {
    let port_prefix = format!("{dev_name}-port");
    let mut out = Vec::new();
    for iface in fsx::list_subdirs(real) {
        for entry in fsx::list_subdirs(&iface) {
            let name = fsx::file_name(&entry);
            let Some(num) = name.strip_prefix(&port_prefix) else {
                continue;
            };
            out.push(HubPort {
                number: num.parse().ok(),
                state: fsx::read_attr(&entry, "state"),
                connect_type: fsx::read_attr(&entry, "connect_type"),
                over_current_count: fsx::read_u32(&entry, "over_current_count"),
                location: fsx::read_attr(&entry, "location"),
                physical_location: read_physical_location(&entry),
                // Authoritative Type-C association, when firmware provides it.
                connector: fsx::read_link_name(&entry, "connector"),
                child: fsx::read_link_name(&entry, "device"),
                name,
            });
        }
    }
    out.sort_by(|a, b| a.number.cmp(&b.number).then(a.name.cmp(&b.name)));
    out
}

/// Read an ACPI `_PLD`-derived `physical_location` directory.
pub(crate) fn read_physical_location(dir: &Path) -> Option<PhysicalLocation> {
    let loc = dir.join("physical_location");
    if !loc.is_dir() {
        return None;
    }
    let pl = PhysicalLocation {
        panel: fsx::read_attr(&loc, "panel"),
        vertical_position: fsx::read_attr(&loc, "vertical_position"),
        horizontal_position: fsx::read_attr(&loc, "horizontal_position"),
        dock: fsx::read_yesno(&loc, "dock"),
        lid: fsx::read_yesno(&loc, "lid"),
    };
    if pl.is_empty() {
        None
    } else {
        Some(pl)
    }
}

/// Interface directories live inside the device's real path, named `<dev>:<cfg>.<if>`.
fn read_interfaces(dev_name: &str, real: &Path) -> Vec<UsbInterface> {
    let prefix = format!("{dev_name}:");
    let mut out = Vec::new();
    for entry in fsx::list_subdirs(real) {
        let name = fsx::file_name(&entry);
        if !name.starts_with(&prefix) {
            continue;
        }
        out.push(UsbInterface {
            sysfs_name: name,
            number: fsx::read_hex_u8(&entry, "bInterfaceNumber").map(u32::from),
            class: fsx::read_hex_u8(&entry, "bInterfaceClass"),
            subclass: fsx::read_hex_u8(&entry, "bInterfaceSubClass"),
            protocol: fsx::read_hex_u8(&entry, "bInterfaceProtocol"),
            driver: fsx::read_link_name(&entry, "driver"),
            description: fsx::read_attr(&entry, "interface"),
        });
    }
    out
}

/// Human name for a USB class code, for display only.
pub fn class_name(code: u8) -> &'static str {
    match code {
        0x00 => "per-interface",
        0x01 => "audio",
        0x02 => "communications",
        0x03 => "HID",
        0x05 => "physical",
        0x06 => "image",
        0x07 => "printer",
        0x08 => "mass storage",
        0x09 => "hub",
        0x0a => "CDC data",
        0x0b => "smart card",
        0x0d => "content security",
        0x0e => "video",
        0x0f => "personal healthcare",
        0x10 => "audio/video",
        0x11 => "billboard",
        0x12 => "USB-C bridge",
        0x3c => "I3C",
        0xdc => "diagnostic",
        0xe0 => "wireless",
        0xef => "miscellaneous",
        0xfe => "application specific",
        0xff => "vendor specific",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a miniature sysfs tree: usb9 -> 9-1 -> 9-1.2, and check that the
    /// parent/child recovery works purely from path nesting.
    #[test]
    fn recovers_hierarchy_from_symlink_targets() {
        let base = std::env::temp_dir().join(format!("usbprobe-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let devices = base.join("devices");
        let class = base.join("class");
        let hub = devices.join("usb9");
        let child = hub.join("9-1");
        let grandchild = child.join("9-1.2");
        fs::create_dir_all(&grandchild).unwrap();
        fs::create_dir_all(&class).unwrap();

        for (dir, speed, version) in [
            (&hub, "10000", " 3.10"),
            (&child, "10000", " 3.20"),
            (&grandchild, "480", " 2.10"),
        ] {
            fs::write(dir.join("speed"), format!("{speed}\n")).unwrap();
            fs::write(dir.join("version"), format!("{version}\n")).unwrap();
            fs::write(dir.join("bMaxPower"), "224mA\n").unwrap();
            fs::write(dir.join("bmAttributes"), "e0\n").unwrap();
        }
        fs::write(child.join("tx_lanes"), "2\n").unwrap();

        for d in [&hub, &child, &grandchild] {
            let name = fsx::file_name(d);
            std::os::unix::fs::symlink(d, class.join(name)).unwrap();
        }

        let buses = read_buses_from(&class);
        assert_eq!(buses.len(), 1, "one root hub");
        let bus = &buses[0];
        assert_eq!(bus.sysfs_name, "usb9");
        assert!(bus.is_root_hub);
        assert_eq!(bus.children.len(), 1);

        let c = &bus.children[0];
        assert_eq!(c.sysfs_name, "9-1");
        assert_eq!(c.parent.as_deref(), Some("usb9"));
        assert_eq!(c.tx_lanes, Some(2));
        assert_eq!(c.usb_version_num, Some(3.20));
        assert_eq!(c.max_power_ma, Some(224));
        assert_eq!(c.self_powered, Some(true));
        assert_eq!(c.remote_wakeup, Some(true));

        let g = &c.children[0];
        assert_eq!(g.sysfs_name, "9-1.2");
        assert_eq!(g.parent.as_deref(), Some("9-1"));
        // The interesting case: claims USB 2.1, linked at 480 -> not a downshift.
        assert!(!g.claims_superspeed());
        assert!(g.linked_below_superspeed());
        // The child claims 3.20 but linked at 10000 -> single-lane, not downshift.
        assert!(c.claims_superspeed());
        assert!(!c.linked_below_superspeed());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn speed_labels_match_marketing_names() {
        assert_eq!(LinkSpeed::from_mbps(1.5).short(), "1.5M");
        assert_eq!(LinkSpeed::from_mbps(480.0).short(), "480M");
        assert_eq!(LinkSpeed::from_mbps(5000.0).short(), "5G");
        assert!(LinkSpeed::from_mbps(10000.0).label.contains("Gen 2x1"));
        assert!(LinkSpeed::from_mbps(20000.0).label.contains("Gen 2x2"));
    }
}
