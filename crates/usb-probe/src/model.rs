//! Serializable data model.
//!
//! This is the stable contract between the probe layer and any front end (CLI,
//! native UI, JSON consumer). Everything is owned data with no borrows and no
//! handles to sysfs, so a `Snapshot` can be moved across threads, cached, or
//! diffed against a later one.
//!
//! Almost every field is `Option`: which sysfs attributes exist depends on the
//! kernel version, the Type-C driver (`tcpm` vs `ucsi` vs vendor), and on the
//! platform firmware. Absent is the normal case, not an error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One complete read of the system's USB state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub captured_at_unix_ms: u64,
    pub host: Host,
    /// Root hubs, each with its device tree in `children`.
    pub buses: Vec<UsbDevice>,
    pub ports: Vec<TypecPort>,
    /// USB4 / Thunderbolt routers and active-cable retimers.
    pub thunderbolt: ThunderboltTopology,
    /// PD objects not reachable from a port (kept so nothing is silently dropped).
    pub orphan_pd: Vec<PowerDelivery>,
    pub kernel_log: KernelLog,
}

impl Snapshot {
    /// Depth-first walk of every USB device on every bus.
    pub fn devices(&self) -> Vec<&UsbDevice> {
        let mut out = Vec::new();
        for bus in &self.buses {
            bus.walk(&mut out);
        }
        out
    }

    pub fn device(&self, sysfs_name: &str) -> Option<&UsbDevice> {
        self.devices()
            .into_iter()
            .find(|d| d.sysfs_name == sysfs_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub kernel_release: Option<String>,
    pub product_name: Option<String>,
    pub sys_vendor: Option<String>,
    /// Type-C backends seen in `/proc/modules` (`typec_ucsi`, `tcpm`, ...).
    pub typec_drivers: Vec<String>,
}

// ---------------------------------------------------------------------------
// USB devices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbDevice {
    /// Kernel name, e.g. `usb3` for a root hub or `3-5.1` for a device.
    pub sysfs_name: String,
    pub sysfs_path: PathBuf,
    pub is_root_hub: bool,
    pub parent: Option<String>,
    pub busnum: Option<u32>,
    pub devnum: Option<u32>,

    pub id_vendor: Option<u16>,
    pub id_product: Option<u16>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,

    /// `version` attribute, e.g. "3.20" — what this device *claims* to support.
    pub usb_version: Option<String>,
    /// `usb_version` parsed to a comparable number (3.20 -> 3.20).
    pub usb_version_num: Option<f32>,
    /// The link rate actually negotiated at enumeration.
    pub speed: Option<LinkSpeed>,
    pub rx_lanes: Option<u32>,
    pub tx_lanes: Option<u32>,

    /// `bMaxPower` in mA — what the active configuration asks the bus for.
    pub max_power_ma: Option<u32>,
    pub self_powered: Option<bool>,
    pub remote_wakeup: Option<bool>,

    pub device_class: Option<u8>,
    pub max_children: Option<u32>,
    pub removable: Option<String>,
    pub authorized: Option<bool>,

    /// Cumulative URBs submitted to this device — a rough activity level.
    pub urbnum: Option<u64>,
    /// Runtime-PM accounting, in milliseconds. The ratio between these is what
    /// distinguishes a device being cycled by power management from one whose
    /// link is genuinely marginal.
    pub active_duration_ms: Option<u64>,
    pub connected_duration_ms: Option<u64>,
    pub runtime_suspended_ms: Option<u64>,
    /// `auto` (runtime PM may suspend it) or `on` (kept awake).
    pub power_control: Option<String>,
    pub autosuspend_delay_ms: Option<i64>,

    pub interfaces: Vec<UsbInterface>,
    /// Downstream ports, for hubs and root hubs. Carries the per-port
    /// over-current counter, which is a measured hardware fault signal.
    pub ports: Vec<HubPort>,
    pub children: Vec<UsbDevice>,
}

impl UsbDevice {
    fn walk<'a>(&'a self, out: &mut Vec<&'a UsbDevice>) {
        out.push(self);
        for c in &self.children {
            c.walk(out);
        }
    }

    pub fn label(&self) -> String {
        match (&self.manufacturer, &self.product) {
            (Some(m), Some(p)) => format!("{m} {p}"),
            (None, Some(p)) => p.clone(),
            (Some(m), None) => m.clone(),
            (None, None) => match (self.id_vendor, self.id_product) {
                (Some(v), Some(p)) => format!("{v:04x}:{p:04x}"),
                _ => self.sysfs_name.clone(),
            },
        }
    }

    pub fn vid_pid(&self) -> Option<String> {
        Some(format!("{:04x}:{:04x}", self.id_vendor?, self.id_product?))
    }

    /// True when the device's own descriptors claim SuperSpeed or better.
    ///
    /// **This cannot detect a USB 3 device that has fallen back to USB 2.0.** A
    /// USB 3 device carries separate descriptor sets for SuperSpeed and
    /// High-Speed operation, so once it falls back it reports `bcdUSB 2.10` and
    /// stops claiming USB 3 — exactly when you would want to know. Verified on
    /// one drive, same cable, two sockets:
    ///
    /// ```text
    /// USB-A socket:      version 3.00, speed 5000, bMaxPower 144mA
    /// USB 2.0 adapter:   version 2.10, speed  480, bMaxPower 100mA
    /// ```
    ///
    /// So this is only useful where a device links slow while *still* reporting
    /// 3.x, which happens when a SuperSpeed-capable path is bandwidth-limited
    /// upstream. For the fallback case use the port topology instead — see
    /// `SS_HALF_IDLE` in [`crate::diag`], which reads the receptacle rather than
    /// the device and therefore has no such blind spot.
    pub fn claims_superspeed(&self) -> bool {
        self.usb_version_num.is_some_and(|v| v >= 3.0)
    }

    /// True when the negotiated link is USB 2.0 or slower.
    pub fn linked_below_superspeed(&self) -> bool {
        self.speed.as_ref().is_some_and(|s| s.mbps <= 480.0)
    }

    /// True when any interface reports this USB class code.
    ///
    /// Needed because `bDeviceClass` is usually `0x00` ("see interfaces"), so the
    /// device-level field cannot be relied on to identify what something is.
    pub fn has_interface_class(&self, class: u8) -> bool {
        self.interfaces.iter().any(|i| i.class == Some(class))
    }

    /// True when the device is soldered down or on an internal header, so no
    /// user-serviceable cable is involved. Webcams, fingerprint readers and
    /// Bluetooth radios are all internal; telling someone to swap their cable
    /// would be nonsense. `unknown` is treated as external, since assuming a
    /// cable exists is the safer error.
    pub fn is_internal(&self) -> bool {
        self.removable.as_deref() == Some("fixed")
    }

    /// Fraction of connected time spent runtime-suspended, 0.0..=1.0.
    pub fn suspend_ratio(&self) -> Option<f64> {
        let connected = self.connected_duration_ms.filter(|c| *c > 0)? as f64;
        Some((self.runtime_suspended_ms? as f64 / connected).clamp(0.0, 1.0))
    }

    /// True when runtime power management is free to suspend this device and is
    /// in fact doing so nearly all the time.
    ///
    /// This is what separates a reset storm caused by power management from one
    /// caused by a bad connection. A device suspended 99% of its connected life
    /// with a short autosuspend delay is *expected* to log resets on every wake;
    /// a device that never suspends and still resets has a real problem.
    pub fn autosuspend_churn(&self) -> bool {
        self.power_control.as_deref() == Some("auto")
            && self.suspend_ratio().is_some_and(|r| r > 0.9)
    }
}

/// A downstream port on a hub or root hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubPort {
    /// e.g. `usb3-port5` or `3-5-port1`.
    pub name: String,
    pub number: Option<u32>,
    /// `not attached` | `powered` | `enabled` | `configured` | `suspended` | ...
    pub state: Option<String>,
    /// `hotplug` | `hardwired` | `not used` — firmware's claim about the port.
    pub connect_type: Option<String>,
    /// Hardware over-current trip counter. Non-zero is a real electrical fault.
    pub over_current_count: Option<u32>,
    /// ACPI `_PLD` group/token, e.g. `0x80000001`. Ports sharing a value are the
    /// same physical receptacle (typically a USB 2.0 and a SuperSpeed half).
    pub location: Option<String>,
    pub physical_location: Option<PhysicalLocation>,
    /// Type-C port this receptacle belongs to, when firmware provides the link.
    /// Authoritative when present; absent on most consumer hardware.
    pub connector: Option<String>,
    /// `sysfs_name` of the device currently attached here.
    pub child: Option<String>,
}

/// Where a receptacle physically is, from ACPI `_PLD`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalLocation {
    pub panel: Option<String>,
    pub vertical_position: Option<String>,
    pub horizontal_position: Option<String>,
    pub dock: Option<bool>,
    pub lid: Option<bool>,
}

impl PhysicalLocation {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Short human form, e.g. `left panel (center/left)`.
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = &self.panel {
            parts.push(format!("{p} panel"));
        }
        // Vertical and horizontal are meaningless alone, so keep them together
        // and visibly subordinate to the panel.
        let pos: Vec<&str> = [
            self.vertical_position.as_deref(),
            self.horizontal_position.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !pos.is_empty() {
            parts.push(format!("({})", pos.join("/")));
        }
        if self.dock == Some(true) {
            parts.push("on dock".to_string());
        }
        if self.lid == Some(true) {
            parts.push("on lid".to_string());
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbInterface {
    pub sysfs_name: String,
    pub number: Option<u32>,
    pub class: Option<u8>,
    pub subclass: Option<u8>,
    pub protocol: Option<u8>,
    pub driver: Option<String>,
    pub description: Option<String>,
}

/// A negotiated link rate, kept as a number plus the marketing-name mess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkSpeed {
    pub mbps: f64,
    pub class: SpeedClass,
    /// Human label, e.g. "SuperSpeed+ 10 Gbps (USB 3.2 Gen 2x1)".
    pub label: String,
}

impl LinkSpeed {
    pub fn from_mbps(mbps: f64) -> Self {
        let (class, label) = match mbps {
            m if m < 2.0 => (SpeedClass::Low, "Low-Speed 1.5 Mbps (USB 1.0)"),
            m if m < 20.0 => (SpeedClass::Full, "Full-Speed 12 Mbps (USB 1.1)"),
            m if m < 1000.0 => (SpeedClass::High, "High-Speed 480 Mbps (USB 2.0)"),
            m if m < 7000.0 => (SpeedClass::SuperSpeed, "SuperSpeed 5 Gbps (USB 3.2 Gen 1x1)"),
            m if m < 15000.0 => (
                SpeedClass::SuperSpeedPlus10,
                "SuperSpeed+ 10 Gbps (USB 3.2 Gen 2x1)",
            ),
            m if m < 30000.0 => (
                SpeedClass::SuperSpeedPlus20,
                "SuperSpeed+ 20 Gbps (USB 3.2 Gen 2x2)",
            ),
            _ => (SpeedClass::Usb4, "USB4 40 Gbps"),
        };
        Self {
            mbps,
            class,
            label: label.to_string(),
        }
    }

    /// Compact form for tree output, e.g. "480M".
    pub fn short(&self) -> String {
        if self.mbps >= 1000.0 {
            format!("{}G", self.mbps / 1000.0)
        } else if self.mbps.fract() == 0.0 {
            format!("{}M", self.mbps as u64)
        } else {
            format!("{}M", self.mbps)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedClass {
    Low,
    Full,
    High,
    SuperSpeed,
    SuperSpeedPlus10,
    SuperSpeedPlus20,
    Usb4,
}

// ---------------------------------------------------------------------------
// Type-C
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypecPort {
    pub name: String,
    pub index: u32,
    pub sysfs_path: PathBuf,

    pub data_role: Option<RoleField>,
    pub power_role: Option<RoleField>,
    pub preferred_role: Option<String>,
    pub port_type: Option<RoleField>,
    /// `default` | `1.5A` | `3.0A` | `usb_power_delivery`
    pub power_operation_mode: Option<String>,
    pub usb_capability: Option<RoleField>,
    pub vconn_source: Option<bool>,
    pub orientation: Option<String>,
    pub pd_revision: Option<String>,
    pub typec_revision: Option<String>,
    pub supported_accessory_modes: Vec<String>,
    pub physical_location: Option<PhysicalLocation>,

    pub alt_modes: Vec<AltMode>,
    /// The *local* port's own PD capabilities (what this machine offers/accepts).
    pub local_pd: Option<PowerDelivery>,
    /// Present only while something is attached.
    pub partner: Option<Partner>,
    /// Present only when the cable has an e-marker *and* the driver reports it.
    pub cable: Option<Cable>,
    pub plugs: Vec<Plug>,
    /// Live electrical state of the negotiated contract, via the power_supply class.
    pub power_supply: Option<PortPowerSupply>,
}

impl TypecPort {
    pub fn is_attached(&self) -> bool {
        self.partner.is_some()
    }

    pub fn pd_contract_active(&self) -> bool {
        self.power_operation_mode.as_deref() == Some("usb_power_delivery")
    }

    /// This machine is supplying power to the attached device.
    ///
    /// Direction matters for interpreting [`PortPowerSupply`]: the UCSI
    /// `ucsi-source-psy-*` node describes power coming *in*, so while sourcing
    /// its `online` is 0 and `current_now` is 0 even though power is flowing
    /// out. Reading those as "nothing is happening" would be wrong.
    pub fn is_sourcing(&self) -> bool {
        self.power_role
            .as_ref()
            .and_then(|r| r.current.as_deref())
            .is_some_and(|r| r.eq_ignore_ascii_case("source"))
    }

    /// Power ceiling implied by the Type-C current advertisement, in mW.
    ///
    /// This is what the CC resistors alone permit, with no PD contract. Returns
    /// `None` while a PD contract is in effect, since the contract supersedes it.
    pub fn typec_advertised_ceiling_mw(&self) -> Option<u32> {
        match self.power_operation_mode.as_deref()? {
            // USB 3 default is 5 V at 900 mA; USB 2 default is lower still.
            "default" => Some(4_500),
            "1.5A" => Some(7_500),
            "3.0A" => Some(15_000),
            // "usb_power_delivery" and anything unrecognised: not applicable.
            _ => None,
        }
    }

    /// This machine is drawing power from the attached device.
    pub fn is_sinking(&self) -> bool {
        self.power_role
            .as_ref()
            .and_then(|r| r.current.as_deref())
            .is_some_and(|r| r.eq_ignore_ascii_case("sink"))
    }

    /// Does this port support SuperSpeed data at all?
    pub fn supports_usb3(&self) -> bool {
        self.usb_capability
            .as_ref()
            .is_some_and(|c| c.supported.iter().any(|s| s.contains("usb3")))
    }
}

/// A sysfs field of the form `host [device]`: a list of supported values with
/// the currently active one in brackets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleField {
    pub current: Option<String>,
    pub supported: Vec<String>,
    pub raw: String,
}

impl RoleField {
    pub fn parse(raw: &str) -> Self {
        let mut supported = Vec::new();
        let mut current = None;
        for tok in raw.split_whitespace() {
            if let Some(inner) = tok.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
                current = Some(inner.to_string());
                supported.push(inner.to_string());
            } else {
                supported.push(tok.to_string());
            }
        }
        // A field with a single unbracketed value is itself the current value.
        if current.is_none() && supported.len() == 1 {
            current = supported.first().cloned();
        }
        Self {
            current,
            supported,
            raw: raw.to_string(),
        }
    }

    pub fn display(&self) -> String {
        self.current.clone().unwrap_or_else(|| self.raw.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AltMode {
    pub sysfs_name: String,
    pub svid: Option<u16>,
    pub svid_name: Option<String>,
    pub mode: Option<u32>,
    pub vdo: Option<Vdo>,
    pub active: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partner {
    pub sysfs_name: String,
    /// `type` attribute, e.g. `Sink` / `Source` / accessory kind. Frequently
    /// absent — plenty of drivers never populate it.
    pub kind: Option<String>,
    pub supports_pd: Option<bool>,
    /// `accessory_mode` when it is anything other than `none`.
    pub accessory_mode: Option<String>,
    pub pd_revision: Option<String>,
    pub num_alt_modes: Option<i64>,
    pub identity: Option<Identity>,
    pub alt_modes: Vec<AltMode>,
    /// The attached device's PD capabilities, when it speaks PD.
    pub pd: Option<PowerDelivery>,
}

impl Partner {
    /// True only when the attached device actually speaks Power Delivery.
    pub fn speaks_pd(&self) -> bool {
        self.supports_pd == Some(true)
    }

    /// PD revision worth showing. Drivers report `0.0` for a device that does
    /// not speak PD at all; printing "PD 0.0" would imply it does.
    pub fn pd_revision_display(&self) -> Option<&str> {
        self.pd_revision
            .as_deref()
            .filter(|r| !r.starts_with("0.") && *r != "0")
    }
}

/// The cable itself, as reported over SOP' communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cable {
    pub sysfs_name: String,
    /// `active` | `passive`
    pub kind: Option<String>,
    /// `type-a` | `type-b` | `type-c` | `captive`
    pub plug_type: Option<String>,
    pub pd_revision: Option<String>,
    pub identity: Option<Identity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plug {
    pub sysfs_name: String,
    pub num_alt_modes: Option<i64>,
    pub alt_modes: Vec<AltMode>,
}

/// The Discover Identity response: raw VDOs plus a best-effort decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id_header: Option<Vdo>,
    pub cert_stat: Option<Vdo>,
    pub product: Option<Vdo>,
    pub product_type_vdo1: Option<Vdo>,
    pub product_type_vdo2: Option<Vdo>,
    pub product_type_vdo3: Option<Vdo>,
    pub decoded: IdentityDecoded,
}

/// Decoded identity fields. Bit layouts moved between PD 2.0 and PD 3.x, so
/// every decode is best-effort and the raw VDOs above stay authoritative.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityDecoded {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub bcd_device: Option<u16>,
    pub xid: Option<u32>,
    pub product_type: Option<String>,
    pub modal_operation: Option<bool>,
    pub connector_type: Option<String>,
    /// Cable-only: highest data rate the cable is built for.
    pub cable_max_speed: Option<String>,
    /// Cable-only: VBUS current the cable is rated for, in mA (3000 or 5000).
    pub cable_current_ma: Option<u32>,
    /// Cable-only: max VBUS voltage in mV.
    pub cable_max_voltage_mv: Option<u32>,
    /// Cable-only: latency bucket, rendered as an approximate length.
    pub cable_latency: Option<String>,
    pub cable_plug_type: Option<String>,
    pub cable_termination: Option<String>,
    pub hw_version: Option<u8>,
    pub fw_version: Option<u8>,
    /// Partner-only: highest data rate the attached device supports.
    pub partner_max_speed: Option<String>,
    pub partner_device_capability: Option<Vec<String>>,
}

/// A 32-bit Vendor Defined Object. Kept raw *and* pre-formatted so a UI never
/// has to reimplement the hex rendering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Vdo {
    pub raw: u32,
    pub hex: HexU32,
}

impl Vdo {
    pub fn new(raw: u32) -> Self {
        Self {
            raw,
            hex: HexU32(raw),
        }
    }
}

/// Serializes as `"0x0123abcd"` so JSON output stays readable.
#[derive(Debug, Clone, Copy)]
pub struct HexU32(pub u32);

impl std::fmt::Display for HexU32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:08x}", self.0)
    }
}

impl Serialize for HexU32 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HexU32 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let t = s.trim_start_matches("0x").trim_start_matches("0X");
        u32::from_str_radix(t, 16)
            .map(HexU32)
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Power Delivery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerDelivery {
    pub name: String,
    pub revision: Option<String>,
    pub source_capabilities: Vec<Pdo>,
    pub sink_capabilities: Vec<Pdo>,
}

impl PowerDelivery {
    /// Highest power any single source PDO can deliver, in mW.
    pub fn max_source_power_mw(&self) -> Option<u32> {
        self.source_capabilities.iter().filter_map(|p| p.power_mw()).max()
    }

    /// Highest power the sink asks for across its PDOs, in mW.
    pub fn max_sink_power_mw(&self) -> Option<u32> {
        self.sink_capabilities.iter().filter_map(|p| p.power_mw()).max()
    }
}

/// A Power Data Object. One entry in a source's "here's what I can give you"
/// or a sink's "here's what I'd like" list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pdo {
    pub index: u32,
    pub kind: PdoKind,
    pub role: PdoRole,
    /// Fixed supply voltage, or `None` for ranged supplies.
    pub voltage_mv: Option<u32>,
    pub min_voltage_mv: Option<u32>,
    pub max_voltage_mv: Option<u32>,
    /// Source PDOs report `maximum_current`, sink PDOs `operational_current`.
    pub current_ma: Option<u32>,
    pub power_mw_field: Option<u32>,
    pub flags: BTreeMap<String, bool>,
    pub peak_current: Option<u32>,
    pub fast_role_swap_current: Option<u32>,
}

impl Pdo {
    /// True for a PPS augmented PDO whose real ceiling is the charger's own
    /// rating rather than max-voltage x max-current.
    pub fn is_power_limited(&self) -> bool {
        self.flags.get("pps_power_limited").copied().unwrap_or(false)
    }

    /// Deliverable power in mW, or `None` when it genuinely cannot be derived.
    ///
    /// A power-limited PPS APDO returns `None` on purpose. A 100 W charger
    /// advertises PPS as 3.3-21 V at 5 A, which multiplies out to 105 W — a
    /// figure it cannot actually deliver. Feeding that into a capability
    /// comparison makes a perfectly good 100 W contract look short, so
    /// power-limited APDOs are excluded and the fixed PDOs decide the maximum.
    pub fn power_mw(&self) -> Option<u32> {
        if let Some(p) = self.power_mw_field {
            return Some(p);
        }
        if self.kind == PdoKind::ProgrammableSupply && self.is_power_limited() {
            return None;
        }
        let v = self.voltage_mv.or(self.max_voltage_mv)?;
        let i = self.current_ma?;
        Some(((v as u64 * i as u64) / 1000) as u32)
    }

    pub fn describe(&self) -> String {
        let power = self
            .power_mw()
            .map(|mw| format!(" ({:.0} W)", mw as f64 / 1000.0))
            .unwrap_or_default();
        match self.kind {
            PdoKind::FixedSupply => format!(
                "{} @ {}{}",
                fmt_mv(self.voltage_mv),
                fmt_ma(self.current_ma),
                power
            ),
            PdoKind::Battery => format!(
                "{}-{} battery{}",
                fmt_mv(self.min_voltage_mv),
                fmt_mv(self.max_voltage_mv),
                power
            ),
            PdoKind::VariableSupply => format!(
                "{}-{} @ {}{}",
                fmt_mv(self.min_voltage_mv),
                fmt_mv(self.max_voltage_mv),
                fmt_ma(self.current_ma),
                power
            ),
            PdoKind::ProgrammableSupply => format!(
                "{}-{} PPS @ {}{}",
                fmt_mv(self.min_voltage_mv),
                fmt_mv(self.max_voltage_mv),
                fmt_ma(self.current_ma),
                if self.is_power_limited() {
                    " (power-limited)".to_string()
                } else {
                    power
                }
            ),
            PdoKind::Unknown => "unknown PDO".to_string(),
        }
    }
}

fn fmt_mv(mv: Option<u32>) -> String {
    mv.map(|v| {
        let s = format!("{:.3}", v as f64 / 1000.0);
        format!("{}V", s.trim_end_matches('0').trim_end_matches('.'))
    })
    .unwrap_or_else(|| "?V".into())
}

fn fmt_ma(ma: Option<u32>) -> String {
    ma.map(|a| {
        let s = format!("{:.3}", a as f64 / 1000.0);
        format!("{}A", s.trim_end_matches('0').trim_end_matches('.'))
    })
    .unwrap_or_else(|| "?A".into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdoKind {
    FixedSupply,
    Battery,
    VariableSupply,
    ProgrammableSupply,
    Unknown,
}

impl PdoKind {
    pub fn from_sysfs(s: &str) -> Self {
        match s {
            "fixed_supply" => Self::FixedSupply,
            "battery" => Self::Battery,
            "variable_supply" => Self::VariableSupply,
            "programmable_supply" => Self::ProgrammableSupply,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdoRole {
    Source,
    Sink,
}

/// The negotiated contract, from `/sys/class/power_supply/<psy>`.
///
/// The field semantics here are counter-intuitive and were verified against real
/// UCSI hardware:
///
/// * `voltage_now` / `current_now` describe the **selected PDO** — the contract
///   in effect. On a 100 W charger these read 20 V and 5 A.
/// * `voltage_min` / `voltage_max` / `current_max` describe a **capability
///   range** across the advertised PDOs, *not* contract limits. A real reading
///   had `voltage_max` = 13.2 V while `voltage_now` = 20 V, which is only
///   possible if they are not limits.
///
/// So the contract is `now x now`, and `*_max` must never be treated as a cap.
/// Neither pair is an instantaneous measurement: nothing in this node reports the
/// current actually being drawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortPowerSupply {
    pub name: String,
    pub online: Option<bool>,
    /// Voltage of the selected PDO — the contract voltage.
    pub voltage_now_mv: Option<u32>,
    /// Maximum current of the selected PDO — the contract current.
    pub current_now_ma: Option<u32>,
    /// Range endpoint across advertised PDOs. Not a contract limit.
    pub voltage_min_mv: Option<u32>,
    /// Range endpoint across advertised PDOs. Not a contract limit.
    pub voltage_max_mv: Option<u32>,
    /// Range endpoint across advertised PDOs. Not a contract limit.
    pub current_max_ma: Option<u32>,
    /// `usb_type` field, e.g. `C [PD] PD_PPS` — the bracketed value is active.
    pub usb_type: Option<RoleField>,
}

impl PortPowerSupply {
    /// Power the negotiated contract permits, in mW.
    ///
    /// While the port is sourcing this is what is being *advertised* to the
    /// attached device rather than a limit on incoming power — see
    /// [`TypecPort::is_sourcing`].
    pub fn contract_power_mw(&self) -> Option<u32> {
        let v = self.contract_voltage_mv()?;
        let i = self.contract_current_ma()?;
        Some(((v as u64 * i as u64) / 1000) as u32)
    }

    pub fn contract_voltage_mv(&self) -> Option<u32> {
        self.voltage_now_mv.filter(|v| *v > 0)
    }

    pub fn contract_current_ma(&self) -> Option<u32> {
        self.current_now_ma.filter(|i| *i > 0)
    }

    /// The advertised voltage range, when the endpoints are self-consistent.
    /// Reported separately from the contract so the two are never confused.
    pub fn voltage_range_mv(&self) -> Option<(u32, u32)> {
        let lo = self.voltage_min_mv?;
        let hi = self.voltage_max_mv?;
        (lo > 0 && hi >= lo).then_some((lo, hi))
    }

    /// Whether this node describes power actually arriving. False while the port
    /// is sourcing, and false when nothing is attached.
    pub fn is_drawing_power(&self) -> bool {
        self.online == Some(true) && self.voltage_now_mv.unwrap_or(0) > 0
    }

    /// True when the contract exceeds 3 A, which the PD specification permits
    /// only over a 5 A e-marked cable. Lets us conclude the cable is 5 A rated
    /// even when the controller never reports its e-marker.
    pub fn contract_requires_5a_cable(&self) -> bool {
        self.contract_current_ma().is_some_and(|i| i > 3000)
    }
}

// ---------------------------------------------------------------------------
// Thunderbolt / USB4
// ---------------------------------------------------------------------------

/// USB4 and Thunderbolt state from `/sys/bus/thunderbolt`.
///
/// A second cable-information path, independent of `/sys/class/typec`. Where a
/// PD e-marker is often withheld by platform firmware, retimers are enumerated
/// by the kernel directly — so on some hardware this is the only working source
/// of genuine cable identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThunderboltTopology {
    pub domains: Vec<ThunderboltDomain>,
    pub routers: Vec<ThunderboltRouter>,
    /// Retimers are the signal-conditioning silicon inside an **active** cable.
    pub retimers: Vec<Retimer>,
}

impl ThunderboltTopology {
    pub fn is_empty(&self) -> bool {
        self.domains.is_empty() && self.routers.is_empty() && self.retimers.is_empty()
    }

    /// True when an active cable is attached: retimers only exist inside one.
    pub fn has_active_cable(&self) -> bool {
        !self.retimers.is_empty()
    }

    /// Routers other than the host — i.e. attached devices.
    pub fn attached(&self) -> impl Iterator<Item = &ThunderboltRouter> {
        self.routers.iter().filter(|r| !r.is_host)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThunderboltDomain {
    pub name: String,
    /// `none` | `user` | `secure` | `dponly` | `usbonly` — the authorization
    /// policy for attached devices.
    pub security: Option<String>,
    pub iommu_dma_protection: Option<bool>,
    pub deauthorization: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThunderboltRouter {
    /// e.g. `0-0` for a host router, `0-1` for the first attached device.
    pub name: String,
    pub is_host: bool,
    /// Thunderbolt/USB4 generation: 3 = TB3/USB4 20G, 4 = TB4/USB4 40G.
    pub generation: Option<u32>,
    pub usb4_version: Option<String>,
    pub vendor_name: Option<String>,
    pub device_name: Option<String>,
    pub unique_id: Option<String>,
    pub authorized: Option<bool>,
    pub rx_speed: Option<String>,
    pub tx_speed: Option<String>,
    pub rx_lanes: Option<u32>,
    pub tx_lanes: Option<u32>,
    pub nvm_version: Option<String>,
    pub usb4_ports: Vec<String>,
}

impl ThunderboltRouter {
    pub fn label(&self) -> String {
        match (&self.vendor_name, &self.device_name) {
            (Some(v), Some(d)) => format!("{v} {d}"),
            (None, Some(d)) => d.clone(),
            (Some(v), None) => v.clone(),
            _ if self.is_host => "host router".to_string(),
            _ => self.name.clone(),
        }
    }
}

/// Active-cable silicon. Its NVM version is the cable's own firmware version —
/// the same value macOS surfaces as "Cable Firmware Version".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retimer {
    /// `<domain>-<route>:<port>.<index>`, e.g. `0-0:1.1`.
    pub name: String,
    pub vendor: Option<u16>,
    pub device: Option<u16>,
    pub nvm_version: Option<String>,
    pub nvm_authenticate: Option<String>,
}

// ---------------------------------------------------------------------------
// Kernel log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelLog {
    pub source: KernelLogSource,
    /// Why the log could not be read, when it couldn't.
    pub note: Option<String>,
    pub events: Vec<KernelEvent>,
}

impl KernelLog {
    pub fn unavailable(note: impl Into<String>) -> Self {
        Self {
            source: KernelLogSource::Unavailable,
            note: Some(note.into()),
            events: Vec::new(),
        }
    }

    /// Events attributable to one USB device path, e.g. `3-4`.
    pub fn for_device<'a>(&'a self, dev: &str) -> Vec<&'a KernelEvent> {
        self.events
            .iter()
            .filter(|e| e.device.as_deref() == Some(dev))
            .collect()
    }

    pub fn count_kind(&self, kind: EventKind) -> usize {
        self.events.iter().filter(|e| e.kind == kind).count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelLogSource {
    DevKmsg,
    Journalctl,
    Dmesg,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelEvent {
    pub kind: EventKind,
    pub severity: Severity,
    /// Device the line is about, normalized to a USB path like `3-4`.
    pub device: Option<String>,
    pub timestamp: Option<String>,
    pub text: String,
    /// Negative errno parsed out of the message, e.g. `-110` from
    /// "device descriptor read/all, error -110". This is usually the actual
    /// diagnosis, so it is extracted rather than left buried in the text.
    pub errno: Option<i32>,
}

/// Bus owning a USB device path: `6-1` -> `usb6`, `usb6` -> `usb6`.
pub fn bus_of(device: &str) -> Option<String> {
    if let Some(rest) = device.strip_prefix("usb") {
        return rest
            .chars()
            .all(|c| c.is_ascii_digit())
            .then(|| device.to_string());
    }
    let num = device.split('-').next()?;
    (!num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("usb{num}"))
}

impl KernelEvent {
    /// Bus name owning this event's device.
    pub fn bus(&self) -> Option<String> {
        bus_of(self.device.as_deref()?)
    }

    /// True when this line reports a SuperSpeed link training successfully.
    pub fn is_superspeed_train(&self) -> bool {
        self.kind == EventKind::DeviceEnumerating
            && self.text.to_ascii_lowercase().contains("superspeed")
    }
}

/// Plain-language meaning of a USB errno. These are the diagnosis, not noise.
pub fn errno_meaning(code: i32) -> Option<&'static str> {
    Some(match code {
        -110 => "ETIMEDOUT — the device never answered",
        -71 => "EPROTO — protocol error, typically signal integrity",
        -84 => "EILSEQ — CRC or framing error, typically signal integrity",
        -32 => "EPIPE — the endpoint stalled",
        -62 => "ETIME — the transfer timed out",
        -75 => "EOVERFLOW — babble, more data than the endpoint expected",
        -19 => "ENODEV — the device vanished mid-operation",
        -22 => "EINVAL — the kernel rejected the request",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Descriptor read failed, address not accepted, enumeration gave up.
    EnumerationFailure,
    /// Kernel re-reset an already-working device — the classic marginal-link sign.
    DeviceReset,
    /// A link trained and the kernel began enumerating: "new SuperSpeed USB
    /// device number 7". Benign alone, but decisive in context — a path that
    /// trains once and then fails has working wiring and a bad connection,
    /// whereas one that never trains is simply missing the wiring.
    DeviceEnumerating,
    /// Kernel explicitly blamed the cable.
    CableSuspect,
    /// A SuperSpeed link failed to train and fell back.
    LinkTrainingFailure,
    OverCurrent,
    InsufficientPower,
    InsufficientBandwidth,
    HostControllerFailure,
    Disconnect,
    TypecEvent,
    Other,
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// Ordered weakest to strongest so `max()` and `sort()` do the obvious thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
            Self::Critical => "CRITICAL",
        }
    }
}

/// How much to trust a finding. Software can measure the negotiated state, but
/// anything about the *cable* is usually inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Read directly from a register or descriptor.
    Measured,
    /// Deduced from two measured facts that disagree.
    Inferred,
    /// Pattern match on symptoms; could have another cause.
    Heuristic,
}

impl Confidence {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Inferred => "inferred",
            Self::Heuristic => "heuristic",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Stable machine-readable id, e.g. `LINK_BELOW_DEVICE_CAPABILITY`.
    pub code: String,
    pub severity: Severity,
    pub confidence: Confidence,
    pub subject: Subject,
    pub title: String,
    pub detail: String,
    /// The specific readings this conclusion rests on.
    pub evidence: Vec<String>,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Subject {
    Host,
    Port(String),
    Device(String),
    Cable(String),
}

impl Subject {
    pub fn display(&self) -> String {
        match self {
            Self::Host => "host".to_string(),
            Self::Port(p) => p.clone(),
            Self::Device(d) => d.clone(),
            Self::Cable(p) => format!("{p} cable"),
        }
    }
}

/// A snapshot plus its analysis — what a UI binds to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub snapshot: Snapshot,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn worst_severity(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }
}
