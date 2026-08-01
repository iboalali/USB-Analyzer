//! Builders for the rule-engine tests.
//!
//! The interesting diagnostic cases — a 3 A cable throttling a 100 W charger, a
//! SuperSpeed drive falling back to 480 Mbps — cannot be produced on demand from
//! a real machine, so the rules are tested against synthetic snapshots.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::*;
use crate::vdo::{IdentityContext, PartialIdentity};

pub fn empty_snapshot() -> Snapshot {
    Snapshot {
        captured_at_unix_ms: 0,
        host: Host {
            kernel_release: Some("6.17.0-test".into()),
            product_name: Some("Test Machine".into()),
            sys_vendor: Some("TEST".into()),
            typec_drivers: vec!["typec_ucsi".into()],
        },
        buses: Vec::new(),
        ports: Vec::new(),
        thunderbolt: ThunderboltTopology::default(),
        block_devices: Vec::new(),
        batteries: Vec::new(),
        displays: Vec::new(),
        mains_online: None,
        uptime_s: None,
        orphan_pd: Vec::new(),
        kernel_log: KernelLog {
            source: KernelLogSource::Journalctl,
            note: None,
            events: Vec::new(),
        },
    }
}

pub fn root_hub(name: &str, mbps: f64) -> UsbDevice {
    let version = if mbps > 480.0 { " 3.10" } else { " 2.00" };
    root_hub_version(name, mbps, version)
}

pub fn root_hub_version(name: &str, mbps: f64, version: &str) -> UsbDevice {
    let mut d = device(name, version, mbps, None);
    d.is_root_hub = true;
    d.device_class = Some(0x09);
    d
}

pub fn device(name: &str, version: &str, mbps: f64, parent: Option<&str>) -> UsbDevice {
    UsbDevice {
        sysfs_name: name.to_string(),
        sysfs_path: PathBuf::from(format!("/sys/devices/test/{name}")),
        is_root_hub: false,
        parent: parent.map(str::to_string),
        busnum: None,
        devnum: None,
        id_vendor: Some(0x1234),
        id_product: Some(0x5678),
        manufacturer: Some("Test".into()),
        product: Some(format!("Device {name}")),
        serial: None,
        usb_version: Some(version.trim().to_string()),
        usb_version_num: version.trim().parse().ok(),
        speed: Some(LinkSpeed::from_mbps(mbps)),
        rx_lanes: Some(1),
        tx_lanes: Some(1),
        max_power_ma: Some(100),
        self_powered: Some(false),
        remote_wakeup: Some(false),
        device_class: Some(0x00),
        max_children: Some(0),
        removable: Some("removable".into()),
        authorized: Some(true),
        // Runtime-PM accounting absent by default; with_runtime_pm() adds it.
        urbnum: None,
        active_duration_ms: None,
        connected_duration_ms: None,
        runtime_suspended_ms: None,
        power_control: None,
        autosuspend_delay_ms: None,
        interfaces: Vec::new(),
        ports: Vec::new(),
        children: Vec::new(),
    }
}

/// Attach runtime-PM accounting to a device.
///
/// Real values from a Goodix fingerprint reader: active 72 s of 12.3 h
/// connected, 99.8% suspended, `control=auto`, 2000 ms delay — which is what
/// manufactures its 21 "reset" log lines. Its Bluetooth neighbour, `control=on`
/// and never suspended, logs none.
pub fn with_runtime_pm(
    mut d: UsbDevice,
    control: &str,
    suspended_fraction: f64,
    delay_ms: i64,
) -> UsbDevice {
    let connected_ms = 44_392_551u64;
    d.connected_duration_ms = Some(connected_ms);
    d.runtime_suspended_ms = Some((connected_ms as f64 * suspended_fraction) as u64);
    d.active_duration_ms = Some((connected_ms as f64 * (1.0 - suspended_fraction)) as u64);
    d.power_control = Some(control.to_string());
    d.autosuspend_delay_ms = Some(delay_ms);
    d.urbnum = Some(567);
    d
}

pub fn hub_port(name: &str, over_current_count: u32) -> HubPort {
    HubPort {
        name: name.to_string(),
        number: name
            .rsplit("-port")
            .next()
            .and_then(|n| n.parse().ok()),
        state: Some("enabled".into()),
        connect_type: Some("hotplug".into()),
        over_current_count: Some(over_current_count),
        location: Some("0x80000001".into()),
        physical_location: None,
        connector: None,
        child: None,
    }
}

pub fn reset_log(device: &str, count: usize) -> KernelLog {
    KernelLog {
        source: KernelLogSource::Journalctl,
        note: None,
        events: (0..count)
            .map(|i| KernelEvent {
                kind: EventKind::DeviceReset,
                severity: Severity::Low,
                device: Some(device.to_string()),
                port: None,
                errno: None,
                monotonic_s: None,
                timestamp: Some(format!("[{i:>6}.000000]")),
                text: format!("usb {device}: reset full-speed USB device number 2 using xhci_hcd"),
            })
            .collect(),
    }
}

/// A Type-C port with nothing plugged in.
pub fn idle_port() -> TypecPort {
    TypecPort {
        name: "port0".into(),
        index: 0,
        sysfs_path: PathBuf::from("/sys/devices/test/typec/port0"),
        data_role: Some(RoleField::parse("host [device]")),
        power_role: Some(RoleField::parse("source [sink]")),
        preferred_role: None,
        port_type: None,
        power_operation_mode: Some("default".into()),
        usb_capability: Some(RoleField::parse("usb2 [usb3]")),
        vconn_source: Some(false),
        orientation: None,
        pd_revision: Some("2.0".into()),
        typec_revision: Some("1.0".into()),
        supported_accessory_modes: Vec::new(),
        physical_location: None,
        alt_modes: Vec::new(),
        local_pd: Some(local_sink_pd()),
        partner: None,
        cable: None,
        plugs: Vec::new(),
        power_supply: Some(PortPowerSupply {
            name: "ucsi-source-psy-TEST1".into(),
            online: Some(false),
            voltage_now_mv: Some(0),
            voltage_min_mv: Some(5000),
            voltage_max_mv: Some(0),
            current_now_ma: Some(0),
            current_max_ma: Some(0),
            usb_type: Some(RoleField::parse("[C] PD PD_PPS")),
        }),
    }
}

/// A charger attached and negotiated.
///
/// `offer_mw` is the best PDO the charger advertises, `cable_current_ma` is the
/// cable's e-marker rating (`None` for an unmarked cable), and the contract pair
/// is what actually ended up in effect.
pub fn charging_port(
    offer_mw: u32,
    cable_current_ma: Option<u32>,
    contract_v_mv: u32,
    contract_i_ma: u32,
) -> TypecPort {
    let mut port = idle_port();
    port.power_operation_mode = Some("usb_power_delivery".into());
    port.pd_revision = Some("3.0".into());

    // Express the offer as a 20 V PDO, the usual top rail.
    let offer_current = (offer_mw / 20).max(1);
    port.partner = Some(Partner {
        sysfs_name: "port0-partner".into(),
        kind: Some("Source".into()),
        supports_pd: Some(true),
        accessory_mode: None,
        pd_revision: Some("3.0".into()),
        num_alt_modes: Some(0),
        identity: None,
        alt_modes: Vec::new(),
        pd: Some(PowerDelivery {
            name: "pd1".into(),
            revision: Some("3.0".into()),
            source_capabilities: vec![
                fixed_pdo(1, 5000, 3000, PdoRole::Source),
                fixed_pdo(2, 20_000, offer_current, PdoRole::Source),
            ],
            sink_capabilities: Vec::new(),
        }),
    });

    port.cable = cable_current_ma.map(|ma| Cable {
        sysfs_name: "port0-cable".into(),
        kind: Some("passive".into()),
        plug_type: Some("type-c".into()),
        pd_revision: Some("3.0".into()),
        identity: Some(
            PartialIdentity {
                id_header: Some(Vdo::new(0b011 << 27)),
                product_type_vdo1: Some(Vdo::new(cable_vdo1(ma))),
                ..Default::default()
            }
            .finish(IdentityContext::Cable, Some("3.0")),
        ),
    });

    port.power_supply = Some(PortPowerSupply {
        name: "ucsi-source-psy-TEST1".into(),
        online: Some(true),
        // now/now is the contract. The *_max fields deliberately carry the
        // inconsistent values real UCSI hardware reports, so any code that
        // mistakes them for contract limits fails these tests.
        voltage_now_mv: Some(contract_v_mv),
        current_now_ma: Some(contract_i_ma),
        voltage_min_mv: Some(5000),
        voltage_max_mv: Some(13_200),
        current_max_ma: Some(3560),
        usb_type: Some(RoleField::parse("[C] PD PD_PPS")),
    });

    port
}

/// A real 100 W laptop charger, transcribed field-for-field from a ThinkPad
/// P16s reading. This is the regression fixture for two shipped false positives:
///
/// * The PPS APDO multiplies out to 21 V x 5 A = 105 W but carries
///   `pps_power_limited = 1`, so 105 W is not deliverable.
/// * The `power_supply` node reports `voltage_max` = 13.2 V and
///   `current_max` = 3.56 A while the *contract* is `voltage_now` x
///   `current_now` = 20 V x 5 A = 100 W. Reading the `*_max` pair as the
///   contract yielded "negotiated only 47 W from a supply offering 105 W" and
///   told the user to buy a cable.
///
/// The controller also reports no `-cable` node and a `0.0` partner PD revision
/// even though PD is active.
pub fn laptop_charger_port_100w() -> TypecPort {
    let mut port = idle_port();
    port.power_operation_mode = Some("usb_power_delivery".into());
    port.data_role = Some(RoleField::parse("host [device]"));
    port.power_role = Some(RoleField::parse("source [sink]"));

    port.partner = Some(Partner {
        sysfs_name: "port0-partner".into(),
        kind: None,
        supports_pd: Some(true),
        accessory_mode: None,
        pd_revision: Some("0.0".into()),
        num_alt_modes: Some(0),
        identity: None,
        alt_modes: Vec::new(),
        pd: Some(PowerDelivery {
            name: "pd1".into(),
            revision: None,
            source_capabilities: vec![
                fixed_pdo(1, 5000, 3000, PdoRole::Source),
                fixed_pdo(2, 9000, 3000, PdoRole::Source),
                fixed_pdo(3, 12_000, 3000, PdoRole::Source),
                fixed_pdo(4, 15_000, 3000, PdoRole::Source),
                fixed_pdo(5, 20_000, 5000, PdoRole::Source),
                Pdo {
                    index: 6,
                    kind: PdoKind::ProgrammableSupply,
                    role: PdoRole::Source,
                    voltage_mv: None,
                    min_voltage_mv: Some(3300),
                    max_voltage_mv: Some(21_000),
                    current_ma: Some(5000),
                    power_mw_field: None,
                    flags: BTreeMap::from([("pps_power_limited".to_string(), true)]),
                    peak_current: None,
                    fast_role_swap_current: None,
                },
            ],
            sink_capabilities: Vec::new(),
        }),
    });

    port.cable = None;
    port.power_supply = Some(PortPowerSupply {
        name: "ucsi-source-psy-USBC000:001".into(),
        online: Some(true),
        voltage_now_mv: Some(20_000),
        current_now_ma: Some(5000),
        voltage_min_mv: Some(5000),
        voltage_max_mv: Some(13_200),
        current_max_ma: Some(3560),
        usb_type: Some(RoleField::parse("C [PD] PD_PPS")),
    });
    port
}

/// The official Lenovo 65 W charger, transcribed from a real reading. Differs
/// from [`laptop_charger_port_100w`] in two ways that matter:
///
/// * Its PPS APDOs are **not** power-limited, and top out at 63 W — below the
///   65 W fixed PDO. So the maximum comes from the fixed rail either way, which
///   exercises the opposite side of the power-limited branch.
/// * The contract is 3.25 A: above the 3 A unmarked-cable limit, but close enough
///   that a captive cable is a plausible explanation. Guards against
///   over-claiming "5 A e-marked".
///
/// `current_max` reads 5.72 A against a 3.25 A contract, and `voltage_max` reads
/// 13.4 V against 20 V — a second independent confirmation that the `*_max`
/// fields are not contract limits.
pub fn official_charger_port_65w() -> TypecPort {
    let mut port = idle_port();
    port.name = "port1".into();
    port.index = 1;
    port.power_operation_mode = Some("usb_power_delivery".into());
    port.data_role = Some(RoleField::parse("host [device]"));
    port.power_role = Some(RoleField::parse("source [sink]"));

    let pps = |index: u32, lo: u32, hi: u32, ma: u32| Pdo {
        index,
        kind: PdoKind::ProgrammableSupply,
        role: PdoRole::Source,
        voltage_mv: None,
        min_voltage_mv: Some(lo),
        max_voltage_mv: Some(hi),
        current_ma: Some(ma),
        power_mw_field: None,
        // Not power-limited on this charger.
        flags: BTreeMap::from([("pps_power_limited".to_string(), false)]),
        peak_current: None,
        fast_role_swap_current: None,
    };

    port.partner = Some(Partner {
        sysfs_name: "port1-partner".into(),
        kind: None,
        supports_pd: Some(true),
        accessory_mode: None,
        pd_revision: Some("0.0".into()),
        num_alt_modes: Some(0),
        identity: None,
        alt_modes: Vec::new(),
        pd: Some(PowerDelivery {
            name: "pd2".into(),
            revision: None,
            source_capabilities: vec![
                fixed_pdo(1, 5000, 3000, PdoRole::Source),
                fixed_pdo(2, 9000, 3000, PdoRole::Source),
                fixed_pdo(3, 15_000, 3000, PdoRole::Source),
                fixed_pdo(4, 20_000, 3250, PdoRole::Source),
                pps(5, 5000, 11_000, 3000),
                pps(6, 5000, 21_000, 3000),
            ],
            sink_capabilities: Vec::new(),
        }),
    });

    port.cable = None;
    port.power_supply = Some(PortPowerSupply {
        name: "ucsi-source-psy-USBC000:002".into(),
        online: Some(true),
        voltage_now_mv: Some(20_000),
        current_now_ma: Some(3250),
        voltage_min_mv: Some(5000),
        voltage_max_mv: Some(13_400),
        current_max_ma: Some(5720),
        usb_type: Some(RoleField::parse("C [PD] PD_PPS")),
    });
    port
}

/// A non-PD device attached while *this machine supplies power* — a watch
/// charger, a small accessory, anything with a captive cable.
///
/// Values mirror a real UCSI reading: the partner reports
/// `supports_usb_power_delivery = no` and a `0.0` PD revision, there is no
/// `-cable` node, and the `ucsi-source-psy` node shows `online = 0` with
/// `current_now = 0` even though 5 V is being supplied — because that node
/// describes incoming power.
pub fn sourcing_port_non_pd() -> TypecPort {
    let mut port = idle_port();
    port.name = "port1".into();
    port.index = 1;
    port.data_role = Some(RoleField::parse("[host] device"));
    port.power_role = Some(RoleField::parse("[source] sink"));
    port.power_operation_mode = Some("3.0A".into());
    port.partner = Some(Partner {
        sysfs_name: "port1-partner".into(),
        kind: None,
        supports_pd: Some(false),
        accessory_mode: None,
        pd_revision: Some("0.0".into()),
        num_alt_modes: None,
        identity: None,
        alt_modes: Vec::new(),
        pd: None,
    });
    port.cable = None;
    port.power_supply = Some(PortPowerSupply {
        name: "ucsi-source-psy-TEST2".into(),
        online: Some(false),
        voltage_now_mv: Some(5000),
        voltage_min_mv: Some(5000),
        voltage_max_mv: Some(5000),
        current_now_ma: Some(0),
        current_max_ma: Some(3000),
        usb_type: Some(RoleField::parse("[C] PD PD_PPS")),
    });
    port
}

/// A receptacle: one physical socket exposed as a USB 2.0 half and a SuperSpeed
/// half on different buses, sharing an ACPI `_PLD` location token.
///
/// `slow_child` / `fast_child` name the device attached to each half. Values
/// mirror the real 0x80000001 pair (usb5-port1 480M + usb6-port1 10000M).
pub fn receptacle(
    location: &str,
    slow_child: Option<&str>,
    fast_child: Option<&str>,
) -> (UsbDevice, UsbDevice) {
    let mut slow_hub = root_hub("usb5", 480.0);
    let mut fast_hub = root_hub("usb6", 10_000.0);
    slow_hub.ports.push(site("usb5-port1", location, slow_child));
    fast_hub.ports.push(site("usb6-port1", location, fast_child));
    (slow_hub, fast_hub)
}

fn site(name: &str, location: &str, child: Option<&str>) -> HubPort {
    HubPort {
        name: name.to_string(),
        number: Some(1),
        state: Some(if child.is_some() {
            "configured".into()
        } else {
            "not attached".into()
        }),
        connect_type: Some("hotplug".into()),
        over_current_count: Some(0),
        location: Some(location.to_string()),
        physical_location: Some(PhysicalLocation {
            panel: Some("left".into()),
            vertical_position: Some("center".into()),
            horizontal_position: Some("left".into()),
            dock: Some(false),
            lid: Some(false),
        }),
        connector: None,
        child: child.map(str::to_string),
    }
}

/// A device with one interface of the given class, so `has_interface_class`
/// works. `version`/`mbps` let a caller build the fallback state.
pub fn device_with_class(
    name: &str,
    version: &str,
    mbps: f64,
    parent: Option<&str>,
    class: u8,
) -> UsbDevice {
    let mut d = device(name, version, mbps, parent);
    d.interfaces.push(UsbInterface {
        sysfs_name: format!("{name}:1.0"),
        number: Some(0),
        class: Some(class),
        subclass: Some(0x06),
        protocol: Some(0x50),
        driver: Some("usb-storage".into()),
        description: None,
    });
    d
}

/// A Type-C port sinking with no PD contract — a charger connected through
/// something that prevents PD negotiation, or a supply that has none.
///
/// Mirrors a real reading: a 45 W PD charger reduced to a 15 W Type-C
/// advertisement, with the partner reporting `supports_usb_power_delivery = no`
/// and no source capabilities exposed at all.
pub fn sinking_port_no_pd(mode: &str) -> TypecPort {
    let mut port = idle_port();
    port.power_operation_mode = Some(mode.to_string());
    port.power_role = Some(RoleField::parse("source [sink]"));
    port.data_role = Some(RoleField::parse("host [device]"));
    port.partner = Some(Partner {
        sysfs_name: "port0-partner".into(),
        kind: None,
        supports_pd: Some(false),
        accessory_mode: None,
        pd_revision: Some("0.0".into()),
        num_alt_modes: None,
        identity: None,
        alt_modes: Vec::new(),
        // No contract means the supply advertises nothing at all.
        pd: None,
    });
    port.power_supply = Some(PortPowerSupply {
        name: "ucsi-source-psy-USBC000:001".into(),
        online: Some(true),
        voltage_now_mv: Some(0),
        current_now_ma: Some(0),
        voltage_min_mv: Some(5000),
        voltage_max_mv: Some(13_400),
        current_max_ma: Some(5720),
        usb_type: Some(RoleField::parse("C [PD] PD_PPS")),
    });
    port
}

/// Kernel events from a SuperSpeed uplink failing, transcribed from a real
/// Anker USB-C hub whose built-in cable had a loose connection — the one case in
/// this project with an independently confirmed physical cause.
///
/// `trained` selects the two outcomes that call for opposite advice:
/// * `true`  — retries, then a successful Gen 2x1 train, then a `-110` timeout.
///   The SuperSpeed pairs exist; the connection is intermittent. Defective.
/// * `false` — retries and nothing else. No SuperSpeed wiring in the path.
///   Wrong cable, nothing broken.
pub fn ss_uplink_failure_events(bus_num: u32, retries: usize, trained: bool) -> Vec<KernelEvent> {
    let bus = format!("usb{bus_num}");
    let mut events: Vec<KernelEvent> = (0..retries)
        .map(|i| KernelEvent {
            kind: EventKind::CableSuspect,
            severity: Severity::High,
            device: Some(bus.clone()),
            port: Some(format!("{bus}-port1")),
            errno: None,
            monotonic_s: Some(1000.0 + i as f64),
            timestamp: Some(format!("[{i:>6}.000000]")),
            text: format!("usb {bus}-port1: Cannot enable. Maybe the USB cable is bad?"),
        })
        .collect();

    if trained {
        events.push(KernelEvent {
            kind: EventKind::DeviceEnumerating,
            severity: Severity::Info,
            device: Some(format!("{bus_num}-1")),
            port: None,
            errno: None,
            monotonic_s: Some(1100.0),
            timestamp: None,
            text: format!(
                "usb {bus_num}-1: new SuperSpeed Plus Gen 2x1 USB device number 7 using xhci_hcd"
            ),
        });
        events.push(KernelEvent {
            kind: EventKind::EnumerationFailure,
            severity: Severity::High,
            device: Some(format!("{bus_num}-1")),
            port: None,
            errno: Some(-110),
            monotonic_s: Some(1101.0),
            timestamp: None,
            text: format!("usb {bus_num}-1: device descriptor read/all, error -110"),
        });
    }
    events
}

/// Failures logged against a device path that never reached sysfs.
///
/// The real sequence, from an Anker hub whose built-in cable is loose:
/// two `Device not responding to setup address` lines, then
/// `device not accepting address N, error -71`.
///
/// `port` is deliberately left unset: these date the phantom by its ancestry,
/// not by a port name, which is the case the ancestor filter has to handle.
pub fn phantom_failure_events(device: &str, at_s: f64) -> Vec<KernelEvent> {
    let mut events: Vec<KernelEvent> = (0..2)
        .map(|i| KernelEvent {
            kind: EventKind::EnumerationFailure,
            severity: Severity::High,
            device: Some(device.to_string()),
            port: None,
            errno: None,
            monotonic_s: Some(at_s + i as f64 * 0.2),
            timestamp: None,
            text: format!("usb {device}: Device not responding to setup address."),
        })
        .collect();
    events.push(KernelEvent {
        kind: EventKind::EnumerationFailure,
        severity: Severity::High,
        device: Some(device.to_string()),
        port: None,
        errno: Some(-71),
        monotonic_s: Some(at_s + 0.5),
        timestamp: None,
        text: format!("usb {device}: device not accepting address 27, error -71"),
    });
    events
}

/// A device with a known attach time, expressed as seconds before `uptime_s`.
pub fn attached_ago(mut d: UsbDevice, seconds_ago: f64) -> UsbDevice {
    d.connected_duration_ms = Some((seconds_ago * 1000.0) as u64);
    d
}

/// A DRM connector, as `/sys/class/drm` would report it.
pub fn connector(name: &str, status: &str, enabled: bool) -> DisplayConnector {
    DisplayConnector {
        name: format!("card1-{name}"),
        connector: name.to_string(),
        connector_id: Some(100),
        status: Some(status.to_string()),
        enabled: Some(enabled),
        dpms: Some(if enabled { "On".into() } else { "Off".into() }),
        modes: Vec::new(),
        display: None,
    }
}

/// An alternate mode as the partner reports it, with `active` meaning what it
/// says — unlike a local port's copy.
pub fn partner_alt_mode(svid: u16, active: bool) -> AltMode {
    AltMode {
        sysfs_name: format!("port0-partner.{svid:x}"),
        svid: Some(svid),
        svid_name: Some("DisplayPort Alt Mode (VESA)".into()),
        mode: Some(1),
        vdo: None,
        active: Some(active),
        description: None,
    }
}

/// A device presenting a USB Billboard interface — a USB-C device's own
/// declaration that an Alternate Mode it requested could not be entered.
/// Modelled on the Anker hub's `291a:8383`.
pub fn billboard_device(name: &str, parent: Option<&str>) -> UsbDevice {
    let mut d = device_with_class(name, " 2.01", 480.0, parent, 0x11);
    d.manufacturer = Some("Anker".into());
    d.product = Some("Anker USB-C HUB Device".into());
    d.id_vendor = Some(0x291a);
    d.id_product = Some(0x8383);
    d.interfaces[0].driver = None;
    d
}

/// A passive Type-C cable VDO1: 10 Gbps, ~1 m, 20 V, with the given current
/// rating. Speed is deliberately *not* limiting so current-rating tests isolate
/// the current rule.
fn cable_vdo1(current_ma: u32) -> u32 {
    let current_bits = match current_ma {
        5000 => 0b10,
        _ => 0b01,
    };
    let plug_type_c = 0b10 << 18;
    let latency_1m = 0b0001 << 13;
    let speed_10g = 0b010;
    plug_type_c | latency_1m | (current_bits << 5) | speed_10g
}

/// This machine's own sink capabilities: 5 V/3 A fixed plus 9-20 V/5 A variable.
fn local_sink_pd() -> PowerDelivery {
    PowerDelivery {
        name: "pd0".into(),
        revision: Some("2.0".into()),
        source_capabilities: Vec::new(),
        sink_capabilities: vec![
            fixed_pdo(1, 5000, 3000, PdoRole::Sink),
            Pdo {
                index: 2,
                kind: PdoKind::VariableSupply,
                role: PdoRole::Sink,
                voltage_mv: None,
                min_voltage_mv: Some(9000),
                max_voltage_mv: Some(20_000),
                current_ma: Some(5000),
                power_mw_field: None,
                flags: BTreeMap::new(),
                peak_current: None,
                fast_role_swap_current: None,
            },
        ],
    }
}

fn fixed_pdo(index: u32, mv: u32, ma: u32, role: PdoRole) -> Pdo {
    Pdo {
        index,
        kind: PdoKind::FixedSupply,
        role,
        voltage_mv: Some(mv),
        min_voltage_mv: None,
        max_voltage_mv: None,
        current_ma: Some(ma),
        power_mw_field: None,
        flags: BTreeMap::new(),
        peak_current: None,
        fast_role_swap_current: None,
    }
}
