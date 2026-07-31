//! Reads `/sys/class/typec` — ports, what's attached, and the cable.
//!
//! Layout, using `port0` as an example:
//!
//! ```text
//! /sys/class/typec/port0            the local port
//! /sys/class/typec/port0/port0.0    an alternate mode of the local port
//! /sys/class/typec/port0-partner    the attached device   (only while attached)
//! /sys/class/typec/port0-partner/identity/   its Discover Identity VDOs
//! /sys/class/typec/port0-cable      the cable             (only if e-marked)
//! /sys/class/typec/port0-plug0      the cable's plug(s)
//! ```
//!
//! `port0-cable` is the interesting one and the least reliable: it appears only
//! when the cable carries an e-marker chip **and** the port controller reports
//! SOP' data up to the kernel. A cheap unmarked cable has nothing to report, and
//! some firmware never surfaces it even for good cables.

use std::path::{Path, PathBuf};

use crate::model::{AltMode, Cable, Partner, PhysicalLocation, Plug, TypecPort};
use crate::pd;
use crate::sysfs as fsx;
use crate::vdo::{self, IdentityContext, PartialIdentity};

const TYPEC_CLASS: &str = "/sys/class/typec";

/// Read every Type-C port, with partner/cable/PD state attached.
pub fn read_ports() -> Vec<TypecPort> {
    let supplies = pd::read_power_supplies();
    read_ports_from(Path::new(TYPEC_CLASS), &supplies)
}

pub fn read_ports_from(class: &Path, supplies: &[pd::RawPowerSupply]) -> Vec<TypecPort> {
    let entries = fsx::list_dir(class);
    let port_dirs: Vec<PathBuf> = entries
        .iter()
        .filter(|p| port_index(&fsx::file_name(p)).is_some())
        .cloned()
        .collect();
    let port_count = port_dirs.len();

    port_dirs
        .iter()
        .filter_map(|dir| {
            let name = fsx::file_name(dir);
            let index = port_index(&name)?;
            Some(read_port(class, dir, &name, index, supplies, port_count))
        })
        .collect()
}

/// `port0` -> 0. Rejects `port0-partner`, `port0-cable`, `port0.1`.
fn port_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("port")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn read_port(
    class: &Path,
    dir: &Path,
    name: &str,
    index: u32,
    supplies: &[pd::RawPowerSupply],
    port_count: usize,
) -> TypecPort {
    let pd_revision = fsx::read_attr(dir, "usb_power_delivery_revision");

    TypecPort {
        name: name.to_string(),
        index,
        sysfs_path: fsx::canonicalize(dir).unwrap_or_else(|| dir.to_path_buf()),

        data_role: fsx::read_role(dir, "data_role"),
        power_role: fsx::read_role(dir, "power_role"),
        preferred_role: fsx::read_attr(dir, "preferred_role"),
        port_type: fsx::read_role(dir, "port_type"),
        power_operation_mode: fsx::read_attr(dir, "power_operation_mode"),
        usb_capability: fsx::read_role(dir, "usb_capability"),
        vconn_source: fsx::read_yesno(dir, "vconn_source"),
        orientation: fsx::read_attr(dir, "orientation"),
        pd_revision: pd_revision.clone(),
        typec_revision: fsx::read_attr(dir, "usb_typec_revision"),
        supported_accessory_modes: fsx::read_attr(dir, "supported_accessory_modes")
            .map(|s| {
                s.split_whitespace()
                    .filter(|t| *t != "none")
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        physical_location: read_physical_location(dir),

        alt_modes: read_alt_modes(dir, name),
        local_pd: pd::read_pd_link(dir),
        partner: read_partner(class, name),
        cable: read_cable(class, name, pd_revision.as_deref()),
        plugs: read_plugs(class, name),
        power_supply: pd::match_port(index, supplies, port_count),
    }
}

/// `physical_location` is a directory of ACPI `_PLD`-derived hints.
fn read_physical_location(dir: &Path) -> Option<PhysicalLocation> {
    crate::usb::read_physical_location(dir)
}

/// Alternate modes are child directories named `<owner>.<n>`.
fn read_alt_modes(dir: &Path, owner: &str) -> Vec<AltMode> {
    let prefix = format!("{owner}.");
    let mut out = Vec::new();
    for entry in fsx::list_dir(dir) {
        let name = fsx::file_name(&entry);
        if !name.starts_with(&prefix) || !entry.is_dir() {
            continue;
        }
        let svid = fsx::read_hex_u16(&entry, "svid");
        out.push(AltMode {
            sysfs_name: name,
            svid,
            svid_name: svid.and_then(vdo::svid_name).map(str::to_string),
            mode: fsx::read_u32(&entry, "mode"),
            vdo: fsx::read_vdo(&entry, "vdo"),
            active: fsx::read_yesno(&entry, "active"),
            description: fsx::read_attr(&entry, "description"),
        });
    }
    out
}

fn read_partner(class: &Path, port: &str) -> Option<Partner> {
    let dir = class.join(format!("{port}-partner"));
    if !dir.exists() {
        return None;
    }
    let name = fsx::file_name(&dir);
    let pd_revision = fsx::read_attr(&dir, "usb_power_delivery_revision");
    Some(Partner {
        kind: fsx::read_attr(&dir, "type"),
        // Spelled `yes`/`no`, not `1`/`0`.
        supports_pd: fsx::read_bool(&dir, "supports_usb_power_delivery"),
        accessory_mode: fsx::read_attr(&dir, "accessory_mode").filter(|m| m != "none"),
        num_alt_modes: fsx::read_i64(&dir, "number_of_alternate_modes"),
        identity: read_identity(&dir, IdentityContext::Partner, pd_revision.as_deref()),
        alt_modes: read_alt_modes(&dir, &name),
        pd: pd::read_pd_link(&dir),
        pd_revision,
        sysfs_name: name,
    })
}

fn read_cable(class: &Path, port: &str, port_pd_revision: Option<&str>) -> Option<Cable> {
    let dir = class.join(format!("{port}-cable"));
    if !dir.exists() {
        return None;
    }
    // The cable's own PD revision is what governs its VDO layout; fall back to
    // the port's when the driver doesn't expose it.
    let pd_revision = fsx::read_attr(&dir, "usb_power_delivery_revision");
    let decode_rev = pd_revision.as_deref().or(port_pd_revision);
    Some(Cable {
        sysfs_name: fsx::file_name(&dir),
        kind: fsx::read_attr(&dir, "type"),
        plug_type: fsx::read_attr(&dir, "plug_type"),
        identity: read_identity(&dir, IdentityContext::Cable, decode_rev),
        pd_revision,
    })
}

fn read_plugs(class: &Path, port: &str) -> Vec<Plug> {
    let mut out = Vec::new();
    for n in 0..2 {
        let dir = class.join(format!("{port}-plug{n}"));
        if !dir.exists() {
            continue;
        }
        let name = fsx::file_name(&dir);
        out.push(Plug {
            num_alt_modes: fsx::read_i64(&dir, "number_of_alternate_modes"),
            alt_modes: read_alt_modes(&dir, &name),
            sysfs_name: name,
        });
    }
    out
}

fn read_identity(
    dir: &Path,
    ctx: IdentityContext,
    pd_revision: Option<&str>,
) -> Option<crate::model::Identity> {
    let id = dir.join("identity");
    if !id.is_dir() {
        return None;
    }
    let partial = PartialIdentity {
        id_header: fsx::read_vdo(&id, "id_header"),
        cert_stat: fsx::read_vdo(&id, "cert_stat"),
        product: fsx::read_vdo(&id, "product"),
        product_type_vdo1: fsx::read_vdo(&id, "product_type_vdo1"),
        product_type_vdo2: fsx::read_vdo(&id, "product_type_vdo2"),
        product_type_vdo3: fsx::read_vdo(&id, "product_type_vdo3"),
    };
    if partial.is_empty() {
        return None;
    }
    Some(partial.finish(ctx, pd_revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn port_index_only_matches_bare_ports() {
        assert_eq!(port_index("port0"), Some(0));
        assert_eq!(port_index("port12"), Some(12));
        assert_eq!(port_index("port0-partner"), None);
        assert_eq!(port_index("port0-cable"), None);
        assert_eq!(port_index("port0-plug0"), None);
        assert_eq!(port_index("port0.1"), None);
        assert_eq!(port_index("port"), None);
    }

    /// A full attached-with-e-marked-cable scenario, which is what we cannot
    /// produce on a bare machine but must handle correctly.
    #[test]
    fn reads_attached_port_with_emarked_cable() {
        let base =
            std::env::temp_dir().join(format!("usbprobe-typec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let port = base.join("port0");
        let altmode = port.join("port0.0");
        let partner = base.join("port0-partner");
        let partner_id = partner.join("identity");
        let cable = base.join("port0-cable");
        let cable_id = cable.join("identity");
        let plug = base.join("port0-plug0");
        for d in [&altmode, &partner_id, &cable_id, &plug] {
            fs::create_dir_all(d).unwrap();
        }

        fs::write(port.join("data_role"), "host [device]\n").unwrap();
        fs::write(port.join("power_role"), "source [sink]\n").unwrap();
        fs::write(port.join("power_operation_mode"), "usb_power_delivery\n").unwrap();
        fs::write(port.join("usb_capability"), "usb2 [usb3]\n").unwrap();
        fs::write(port.join("usb_power_delivery_revision"), "3.0\n").unwrap();
        fs::write(port.join("vconn_source"), "yes\n").unwrap();
        fs::write(port.join("supported_accessory_modes"), "none\n").unwrap();

        fs::write(altmode.join("svid"), "ff01\n").unwrap();
        fs::write(altmode.join("mode"), "1\n").unwrap();
        fs::write(altmode.join("vdo"), "0x001c1c43\n").unwrap();
        fs::write(altmode.join("active"), "yes\n").unwrap();

        fs::write(partner.join("type"), "Source\n").unwrap();
        fs::write(partner.join("supports_usb_power_delivery"), "1\n").unwrap();
        fs::write(partner.join("usb_power_delivery_revision"), "3.0\n").unwrap();
        fs::write(partner_id.join("id_header"), "0x1c0004b4\n").unwrap();
        fs::write(partner_id.join("product"), "0x00120100\n").unwrap();

        fs::write(cable.join("type"), "passive\n").unwrap();
        fs::write(cable.join("plug_type"), "type-c\n").unwrap();
        fs::write(cable.join("usb_power_delivery_revision"), "3.0\n").unwrap();
        // Passive cable, 5 A, 10 Gbps, ~1 m.
        fs::write(cable_id.join("id_header"), "0x180005ac\n").unwrap();
        fs::write(cable_id.join("product_type_vdo1"), "0x21082042\n").unwrap();

        fs::write(plug.join("number_of_alternate_modes"), "0\n").unwrap();

        let ports = read_ports_from(&base, &[]);
        assert_eq!(ports.len(), 1, "partner/cable dirs must not count as ports");
        let p = &ports[0];
        assert_eq!(p.index, 0);
        assert!(p.is_attached());
        assert!(p.pd_contract_active());
        assert!(p.supports_usb3());
        assert!(p.supported_accessory_modes.is_empty(), "'none' is filtered");
        assert_eq!(p.vconn_source, Some(true));

        assert_eq!(p.alt_modes.len(), 1);
        assert_eq!(
            p.alt_modes[0].svid_name.as_deref(),
            Some("DisplayPort Alt Mode (VESA)")
        );

        let partner = p.partner.as_ref().unwrap();
        assert_eq!(partner.supports_pd, Some(true));
        let pid = partner.identity.as_ref().unwrap();
        assert_eq!(pid.decoded.vendor_id, Some(0x04b4));
        assert_eq!(pid.decoded.product_id, Some(0x0012));

        let cable = p.cable.as_ref().unwrap();
        assert_eq!(cable.kind.as_deref(), Some("passive"));
        let cid = cable.identity.as_ref().unwrap();
        assert_eq!(cid.decoded.cable_current_ma, Some(5000));
        assert_eq!(cid.decoded.product_type.as_deref(), Some("Passive Cable"));
        assert_eq!(
            cid.decoded.cable_max_speed.as_deref(),
            Some("USB 3.2 / USB4 Gen 2 (10 Gbps)")
        );
        // Raw VDO is preserved alongside the decode.
        assert_eq!(cid.product_type_vdo1.unwrap().raw, 0x2108_2042);
        assert_eq!(cid.product_type_vdo1.unwrap().hex.to_string(), "0x21082042");

        assert_eq!(p.plugs.len(), 1);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn unattached_port_has_no_partner_or_cable() {
        let base =
            std::env::temp_dir().join(format!("usbprobe-typec-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let port = base.join("port0");
        fs::create_dir_all(&port).unwrap();
        fs::write(port.join("power_operation_mode"), "default\n").unwrap();

        let ports = read_ports_from(&base, &[]);
        assert_eq!(ports.len(), 1);
        assert!(!ports[0].is_attached());
        assert!(ports[0].cable.is_none());
        assert!(!ports[0].pd_contract_active());

        let _ = fs::remove_dir_all(&base);
    }
}
