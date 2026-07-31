//! Power Delivery capabilities and the live contract.
//!
//! Two different things live here, and keeping them apart matters for
//! diagnosis:
//!
//! * `/sys/class/usb_power_delivery/pdN` holds **capabilities** — the PDO lists
//!   each side advertises ("I can supply 20 V at 5 A", "I would like 15 V").
//! * `/sys/class/power_supply/<psy>` holds the **live contract** — the voltage
//!   and current actually in effect right now.
//!
//! A charging complaint is almost always a gap between those two.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::{Pdo, PdoKind, PdoRole, PortPowerSupply, PowerDelivery};
use crate::sysfs as fsx;

const PD_CLASS: &str = "/sys/class/usb_power_delivery";
const POWER_SUPPLY_CLASS: &str = "/sys/class/power_supply";

/// Read one `pdN` object.
pub fn read_pd(path: &Path) -> PowerDelivery {
    PowerDelivery {
        name: fsx::file_name(path),
        revision: fsx::read_attr(path, "revision"),
        source_capabilities: read_caps(&path.join("source-capabilities"), PdoRole::Source),
        sink_capabilities: read_caps(&path.join("sink-capabilities"), PdoRole::Sink),
    }
}

/// Every PD object on the system, keyed by name (`pd0`, `pd1`, ...).
pub fn read_all() -> BTreeMap<String, PowerDelivery> {
    read_all_from(Path::new(PD_CLASS))
}

pub fn read_all_from(class: &Path) -> BTreeMap<String, PowerDelivery> {
    fsx::list_dir(class)
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| (fsx::file_name(&p), read_pd(&p)))
        .collect()
}

/// Resolve a `usb_power_delivery` symlink on a port/partner into a PD object.
pub fn read_pd_link(dir: &Path) -> Option<PowerDelivery> {
    let target = fsx::canonicalize(dir.join("usb_power_delivery"))?;
    if target.is_dir() {
        Some(read_pd(&target))
    } else {
        None
    }
}

/// Capability directories contain one subdir per PDO, named `<index>:<kind>`.
fn read_caps(dir: &Path, role: PdoRole) -> Vec<Pdo> {
    let mut out = Vec::new();
    for entry in fsx::list_dir(dir) {
        if !entry.is_dir() {
            continue;
        }
        let name = fsx::file_name(&entry);
        let Some((idx, kind)) = name.split_once(':') else {
            continue;
        };
        // `power` is the runtime-PM subdirectory, not a PDO.
        let Ok(index) = idx.parse::<u32>() else {
            continue;
        };
        out.push(read_pdo(&entry, index, PdoKind::from_sysfs(kind), role));
    }
    out.sort_by_key(|p| p.index);
    out
}

fn read_pdo(dir: &Path, index: u32, kind: PdoKind, role: PdoRole) -> Pdo {
    // Sources advertise `maximum_current`; sinks request `operational_current`.
    let current_ma = fsx::read_suffixed(dir, "maximum_current", "mA")
        .or_else(|| fsx::read_suffixed(dir, "operational_current", "mA"));
    let power_mw = fsx::read_suffixed(dir, "maximum_power", "mW")
        .or_else(|| fsx::read_suffixed(dir, "operational_power", "mW"));

    Pdo {
        index,
        kind,
        role,
        voltage_mv: fsx::read_suffixed(dir, "voltage", "mV"),
        min_voltage_mv: fsx::read_suffixed(dir, "minimum_voltage", "mV"),
        max_voltage_mv: fsx::read_suffixed(dir, "maximum_voltage", "mV"),
        current_ma,
        power_mw_field: power_mw,
        peak_current: fsx::read_u32(dir, "peak_current"),
        fast_role_swap_current: fsx::read_u32(dir, "fast_role_swap_current"),
        flags: read_flags(dir),
    }
}

/// Sweep up every `0`/`1` attribute as a named flag, so new kernel fields show
/// up without a code change.
fn read_flags(dir: &Path) -> BTreeMap<String, bool> {
    let mut flags = BTreeMap::new();
    for entry in fsx::list_dir(dir) {
        if entry.is_dir() {
            continue;
        }
        let name = fsx::file_name(&entry);
        if name == "uevent" {
            continue;
        }
        if let Some(v) = fsx::read_flag(dir, &name) {
            flags.insert(name, v);
        }
    }
    flags
}

// ---------------------------------------------------------------------------
// Live contract via the power_supply class
// ---------------------------------------------------------------------------

/// A USB-type power supply plus its sysfs identity, before port matching.
#[derive(Debug, Clone)]
pub struct RawPowerSupply {
    pub name: String,
    pub path: PathBuf,
    pub data: PortPowerSupply,
}

/// All `type=USB` power supplies. On UCSI systems there is one per Type-C
/// connector; on `tcpm` systems there is typically one per port too.
pub fn read_power_supplies() -> Vec<RawPowerSupply> {
    read_power_supplies_from(Path::new(POWER_SUPPLY_CLASS))
}

pub fn read_power_supplies_from(class: &Path) -> Vec<RawPowerSupply> {
    let mut out = Vec::new();
    for entry in fsx::list_dir(class) {
        if !entry.is_dir() {
            continue;
        }
        if fsx::read_attr(&entry, "type").as_deref() != Some("USB") {
            continue;
        }
        let name = fsx::file_name(&entry);
        out.push(RawPowerSupply {
            data: PortPowerSupply {
                name: name.clone(),
                online: fsx::read_bool(&entry, "online"),
                // power_supply reports microvolts and microamps.
                voltage_now_mv: fsx::read_u32(&entry, "voltage_now").map(|v| v / 1000),
                current_now_ma: fsx::read_u32(&entry, "current_now").map(|v| v / 1000),
                voltage_min_mv: fsx::read_u32(&entry, "voltage_min").map(|v| v / 1000),
                voltage_max_mv: fsx::read_u32(&entry, "voltage_max").map(|v| v / 1000),
                current_max_ma: fsx::read_u32(&entry, "current_max").map(|v| v / 1000),
                usb_type: fsx::read_role(&entry, "usb_type"),
            },
            name,
            path: entry,
        });
    }
    out
}

/// Read every battery, plus whether a mains supply is online.
///
/// Batteries report either `energy_*` (µWh) or `charge_*` (µAh) depending on the
/// firmware; the second form is converted using the design voltage so callers
/// get watt-hours either way.
pub fn read_batteries() -> (Vec<crate::model::Battery>, Option<bool>) {
    read_batteries_from(Path::new(POWER_SUPPLY_CLASS))
}

pub fn read_batteries_from(class: &Path) -> (Vec<crate::model::Battery>, Option<bool>) {
    let mut batteries = Vec::new();
    let mut mains = None;

    for entry in fsx::list_dir(class) {
        if !entry.is_dir() {
            continue;
        }
        match fsx::read_attr(&entry, "type").as_deref() {
            Some("Mains") => {
                // Any online mains supply counts.
                let online = fsx::read_bool(&entry, "online");
                mains = match (mains, online) {
                    (Some(true), _) => Some(true),
                    (_, Some(v)) => Some(v),
                    (m, None) => m,
                };
            }
            Some("Battery") => {
                let uv = |n: &str| fsx::read_u64(&entry, n).map(|v| v as f64 / 1e6);
                let design_v = fsx::read_u64(&entry, "voltage_min_design")
                    .map(|v| v as f64 / 1e6)
                    .filter(|v| *v > 0.0);
                // charge_* is in µAh; multiplying by the design voltage yields Wh.
                let via_charge = |n: &str| match (uv(n), design_v) {
                    (Some(ah), Some(v)) => Some(ah * v),
                    _ => None,
                };

                batteries.push(crate::model::Battery {
                    name: fsx::file_name(&entry),
                    status: fsx::read_attr(&entry, "status"),
                    capacity_pct: fsx::read_u32(&entry, "capacity"),
                    energy_now_wh: uv("energy_now").or_else(|| via_charge("charge_now")),
                    energy_full_wh: uv("energy_full").or_else(|| via_charge("charge_full")),
                    energy_full_design_wh: uv("energy_full_design")
                        .or_else(|| via_charge("charge_full_design")),
                    power_now_w: uv("power_now"),
                    voltage_now_v: uv("voltage_now"),
                    cycle_count: fsx::read_u32(&entry, "cycle_count"),
                });
            }
            _ => {}
        }
    }
    batteries.sort_by(|a, b| a.name.cmp(&b.name));
    (batteries, mains)
}

/// Associate a power supply with a Type-C port.
///
/// The UCSI driver names its supplies `ucsi-source-psy-<devname><connector>`
/// with a **1-based** connector number, so `port0` pairs with the supply whose
/// trailing number is 1. When that yields nothing and there is exactly one
/// candidate for one port (the usual `tcpm` shape), pair them directly.
pub fn match_port(
    port_index: u32,
    supplies: &[RawPowerSupply],
    port_count: usize,
) -> Option<PortPowerSupply> {
    let want = port_index + 1;
    if let Some(s) = supplies
        .iter()
        .find(|s| ucsi_connector_index(&s.name) == Some(want))
    {
        return Some(s.data.clone());
    }
    if port_count == 1 && supplies.len() == 1 {
        return Some(supplies[0].data.clone());
    }
    None
}

/// Highest plausible Type-C connector count on one controller. Guards against
/// reading an unrelated number out of a driver-chosen name.
const MAX_CONNECTORS: u32 = 64;

/// The 1-based connector index encoded in a UCSI power-supply name:
/// `ucsi-source-psy-USBC000:001` -> 1.
///
/// Deliberately restricted to UCSI names. Other backends embed arbitrary digits
/// in theirs — `tcpm-source-psy-i2c-fusb302` would otherwise read as connector
/// 302 and could be matched to the wrong port.
fn ucsi_connector_index(name: &str) -> Option<u32> {
    if !name.starts_with("ucsi-") {
        return None;
    }
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let n: u32 = digits.parse().ok()?;
    (1..=MAX_CONNECTORS).contains(&n).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("usbprobe-pd-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn extracts_ucsi_connector_index() {
        assert_eq!(ucsi_connector_index("ucsi-source-psy-USBC000:001"), Some(1));
        assert_eq!(ucsi_connector_index("ucsi-source-psy-USBC000:002"), Some(2));
        assert_eq!(ucsi_connector_index("ucsi-source-psy-USBC000:0012"), Some(12));
        // Non-UCSI backends embed unrelated digits; never read those as an index.
        assert_eq!(ucsi_connector_index("tcpm-source-psy-i2c-fusb302"), None);
        // Implausible index, e.g. a name we misread.
        assert_eq!(ucsi_connector_index("ucsi-source-psy-x999"), None);
    }

    #[test]
    fn single_port_single_supply_pairs_without_an_index() {
        let supplies = vec![RawPowerSupply {
            name: "tcpm-source-psy-i2c-fusb302".into(),
            path: PathBuf::from("/sys/class/power_supply/tcpm-source-psy-i2c-fusb302"),
            data: PortPowerSupply {
                name: "tcpm-source-psy-i2c-fusb302".into(),
                online: Some(true),
                voltage_now_mv: Some(9000),
                voltage_min_mv: Some(5000),
                voltage_max_mv: Some(9000),
                current_now_ma: Some(1000),
                current_max_ma: Some(3000),
                usb_type: None,
            },
        }];
        assert!(match_port(0, &supplies, 1).is_some());
        // With two ports the pairing is ambiguous, so refuse to guess.
        assert!(match_port(0, &supplies, 2).is_none());
    }

    /// Mirrors the real layout observed on a ThinkPad: a fixed 5 V / 3 A sink
    /// PDO plus a 9-20 V variable sink PDO.
    #[test]
    fn reads_sink_capabilities_with_unit_suffixes() {
        let base = scratch("caps");
        let sink = base.join("pd0/sink-capabilities");
        let fixed = sink.join("1:fixed_supply");
        let var = sink.join("2:variable_supply");
        // The runtime-PM dir must be ignored, not parsed as a PDO.
        fs::create_dir_all(sink.join("power")).unwrap();
        fs::create_dir_all(&fixed).unwrap();
        fs::create_dir_all(&var).unwrap();
        fs::write(base.join("pd0/revision"), "2.0\n").unwrap();

        fs::write(fixed.join("voltage"), "5000mV\n").unwrap();
        fs::write(fixed.join("operational_current"), "3000mA\n").unwrap();
        fs::write(fixed.join("higher_capability"), "1\n").unwrap();
        fs::write(fixed.join("dual_role_power"), "0\n").unwrap();

        fs::write(var.join("minimum_voltage"), "9000mV\n").unwrap();
        fs::write(var.join("maximum_voltage"), "20000mV\n").unwrap();
        fs::write(var.join("operational_current"), "5000mA\n").unwrap();

        let pd = read_pd(&base.join("pd0"));
        assert_eq!(pd.revision.as_deref(), Some("2.0"));
        assert_eq!(pd.sink_capabilities.len(), 2, "the power/ dir is not a PDO");

        let f = &pd.sink_capabilities[0];
        assert_eq!(f.kind, PdoKind::FixedSupply);
        assert_eq!(f.voltage_mv, Some(5000));
        assert_eq!(f.current_ma, Some(3000));
        assert_eq!(f.power_mw(), Some(15_000));
        assert_eq!(f.flags.get("higher_capability"), Some(&true));
        assert_eq!(f.flags.get("dual_role_power"), Some(&false));

        let v = &pd.sink_capabilities[1];
        assert_eq!(v.kind, PdoKind::VariableSupply);
        assert_eq!(v.min_voltage_mv, Some(9000));
        assert_eq!(v.max_voltage_mv, Some(20_000));
        // 20 V x 5 A = 100 W, the peak this machine will accept.
        assert_eq!(v.power_mw(), Some(100_000));
        assert_eq!(pd.max_sink_power_mw(), Some(100_000));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_power_supply_and_converts_micro_units() {
        let base = scratch("psy");
        let psy = base.join("ucsi-source-psy-USBC000:001");
        fs::create_dir_all(&psy).unwrap();
        fs::write(psy.join("type"), "USB\n").unwrap();
        fs::write(psy.join("online"), "1\n").unwrap();
        // Values as a real UCSI controller reports them: now/now is the
        // contract, and the *_max fields are a range that can sit *below* it.
        fs::write(psy.join("voltage_now"), "20000000\n").unwrap();
        fs::write(psy.join("current_now"), "5000000\n").unwrap();
        fs::write(psy.join("voltage_min"), "5000000\n").unwrap();
        fs::write(psy.join("voltage_max"), "13200000\n").unwrap();
        fs::write(psy.join("current_max"), "3560000\n").unwrap();
        fs::write(psy.join("usb_type"), "[C] PD PD_PPS\n").unwrap();
        // A battery must not be picked up.
        let bat = base.join("BAT0");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("type"), "Battery\n").unwrap();

        let supplies = read_power_supplies_from(&base);
        assert_eq!(supplies.len(), 1);
        let d = &supplies[0].data;
        assert_eq!(d.online, Some(true));
        assert_eq!(d.voltage_now_mv, Some(20_000));
        assert_eq!(d.current_now_ma, Some(5000));
        // The contract is now x now = 100 W, not max x max = 47 W.
        assert_eq!(d.contract_power_mw(), Some(100_000));
        assert_eq!(d.voltage_range_mv(), Some((5000, 13_200)));
        assert!(d.contract_requires_5a_cable());
        assert_eq!(d.usb_type.as_ref().unwrap().current.as_deref(), Some("C"));

        // port0 -> connector 1
        assert!(match_port(0, &supplies, 2).is_some());
        assert!(match_port(1, &supplies, 2).is_none());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn zero_contract_reads_as_no_contract() {
        // An unplugged port reports zeros rather than absent files.
        let d = PortPowerSupply {
            name: "x".into(),
            online: Some(false),
            voltage_now_mv: Some(0),
            voltage_min_mv: Some(5000),
            voltage_max_mv: Some(0),
            current_now_ma: Some(0),
            current_max_ma: Some(0),
            usb_type: None,
        };
        assert_eq!(d.contract_power_mw(), None);
    }
}
