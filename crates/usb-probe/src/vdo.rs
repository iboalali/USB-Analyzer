//! Vendor Defined Object decoding.
//!
//! When a USB-C cable has an e-marker chip, the port controller reads its
//! Discover Identity response over SOP' and the kernel exposes the raw VDOs
//! under `.../identity/`. Those VDOs are the only way software can learn what a
//! cable is actually rated for — everything else about a cable is guesswork.
//!
//! Bit layouts follow the USB PD 3.x / Type-C specification. Two caveats that
//! the callers must respect:
//!
//! 1. The Product Type field in the ID Header was renumbered between PD 2.0 and
//!    PD 3.0, so decoding it needs the negotiated revision. When the revision is
//!    unknown we accept both encodings.
//! 2. The same VDO position means different things for SOP (the attached device)
//!    and SOP' (the cable), so the caller states which it has.
//!
//! Raw values are always preserved in [`crate::model::Identity`]; this module
//! only ever adds a best-effort interpretation on top.

use crate::model::{Identity, IdentityDecoded, Vdo};

/// Whether an identity came from the cable (SOP') or the attached device (SOP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityContext {
    Cable,
    Partner,
}

/// Extract bits `hi..=lo` (inclusive, MSB-numbered as in the spec).
fn bits(raw: u32, hi: u32, lo: u32) -> u32 {
    debug_assert!(hi >= lo && hi < 32);
    let width = hi - lo + 1;
    let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
    (raw >> lo) & mask
}

fn bit(raw: u32, n: u32) -> bool {
    (raw >> n) & 1 == 1
}

/// True when the PD revision string denotes 3.0 or later.
fn is_pd3(pd_revision: Option<&str>) -> Option<bool> {
    let rev = pd_revision?;
    let major: u32 = rev.split('.').next()?.trim().parse().ok()?;
    Some(major >= 3)
}

/// Decode every VDO we understand for this identity.
pub fn decode(
    id: &PartialIdentity,
    ctx: IdentityContext,
    pd_revision: Option<&str>,
) -> IdentityDecoded {
    let mut d = IdentityDecoded::default();

    if let Some(h) = id.id_header {
        let raw = h.raw;
        d.vendor_id = Some(bits(raw, 15, 0) as u16);
        d.modal_operation = Some(bit(raw, 26));
        d.connector_type = connector_type(bits(raw, 22, 21));
        let pt = bits(raw, 29, 27);
        d.product_type = Some(match ctx {
            IdentityContext::Cable => cable_product_type(pt, pd_revision).to_string(),
            IdentityContext::Partner => ufp_product_type(pt).to_string(),
        });
    }

    if let Some(c) = id.cert_stat {
        d.xid = Some(c.raw);
    }

    if let Some(p) = id.product {
        d.product_id = Some(bits(p.raw, 31, 16) as u16);
        d.bcd_device = Some(bits(p.raw, 15, 0) as u16);
    }

    if let Some(v1) = id.product_type_vdo1 {
        let raw = v1.raw;
        match ctx {
            IdentityContext::Cable => {
                // Passive and Active Cable VDO1 share the fields we care about
                // (speed, current, voltage, latency, termination) at the same
                // bit positions.
                d.hw_version = Some(bits(raw, 31, 28) as u8);
                d.fw_version = Some(bits(raw, 27, 24) as u8);
                d.cable_plug_type = cable_plug_type(bits(raw, 19, 18));
                d.cable_latency = cable_latency(bits(raw, 16, 13));
                d.cable_termination = cable_termination(bits(raw, 12, 11));
                d.cable_max_voltage_mv = Some(cable_max_voltage_mv(bits(raw, 10, 9)));
                d.cable_current_ma = cable_current_ma(bits(raw, 6, 5));
                d.cable_max_speed = Some(usb_speed(bits(raw, 2, 0)).to_string());
            }
            IdentityContext::Partner => {
                d.partner_max_speed = Some(usb_speed(bits(raw, 2, 0)).to_string());
                d.partner_device_capability = Some(device_capability(bits(raw, 27, 24)));
            }
        }
    }

    d
}

/// The raw VDO set, before decoding.
#[derive(Debug, Clone, Copy, Default)]
pub struct PartialIdentity {
    pub id_header: Option<Vdo>,
    pub cert_stat: Option<Vdo>,
    pub product: Option<Vdo>,
    pub product_type_vdo1: Option<Vdo>,
    pub product_type_vdo2: Option<Vdo>,
    pub product_type_vdo3: Option<Vdo>,
}

impl PartialIdentity {
    pub fn is_empty(&self) -> bool {
        self.id_header.is_none()
            && self.cert_stat.is_none()
            && self.product.is_none()
            && self.product_type_vdo1.is_none()
    }

    /// Finish into the model type, running the decoder.
    pub fn finish(self, ctx: IdentityContext, pd_revision: Option<&str>) -> Identity {
        let decoded = decode(&self, ctx, pd_revision);
        Identity {
            id_header: self.id_header,
            cert_stat: self.cert_stat,
            product: self.product,
            product_type_vdo1: self.product_type_vdo1,
            product_type_vdo2: self.product_type_vdo2,
            product_type_vdo3: self.product_type_vdo3,
            decoded,
        }
    }
}

// --- field tables ----------------------------------------------------------

/// Product Type for a cable plug (SOP'). PD 3.0 renumbered these, so when the
/// revision is unknown we report both readings rather than guess.
fn cable_product_type(v: u32, pd_revision: Option<&str>) -> &'static str {
    match (v, is_pd3(pd_revision)) {
        (0b011, Some(true)) => "Passive Cable",
        (0b100, Some(true)) => "Active Cable",
        (0b100, Some(false)) => "Passive Cable",
        (0b101, Some(false)) => "Active Cable",
        // Unknown revision: both encodings agree that 011 is only ever passive
        // and 101 is only ever active/VPD, but 100 is genuinely ambiguous.
        (0b011, None) => "Passive Cable",
        (0b100, None) => "Passive or Active Cable (ambiguous without PD revision)",
        (0b101, Some(true)) => "VCONN-Powered USB Device",
        (0b101, None) => "Active Cable or VCONN-Powered Device",
        (0b000, _) => "Undefined",
        _ => "Reserved",
    }
}

/// Product Type for an attached device (SOP, UFP field).
fn ufp_product_type(v: u32) -> &'static str {
    match v {
        0b000 => "Undefined",
        0b001 => "PDUSB Hub",
        0b010 => "PDUSB Peripheral",
        0b011 => "Power Sink Device",
        0b101 => "Alternate Mode Adapter",
        0b110 => "VCONN-Powered USB Device",
        _ => "Reserved",
    }
}

fn connector_type(v: u32) -> Option<String> {
    match v {
        0b10 => Some("USB Type-C receptacle".to_string()),
        0b11 => Some("USB Type-C captive plug".to_string()),
        // 00/01 are reserved, and are also what a PD 2.0 device leaves behind.
        _ => None,
    }
}

fn cable_plug_type(v: u32) -> Option<String> {
    Some(
        match v {
            0b00 => "USB Type-A",
            0b01 => "USB Type-B",
            0b10 => "USB Type-C",
            0b11 => "Captive",
            _ => return None,
        }
        .to_string(),
    )
}

/// Cable Latency, rendered as the approximate physical length it implies.
fn cable_latency(v: u32) -> Option<String> {
    Some(
        match v {
            0b0001 => "<10 ns (~1 m)",
            0b0010 => "10-20 ns (~2 m)",
            0b0011 => "20-30 ns (~3 m)",
            0b0100 => "30-40 ns (~4 m)",
            0b0101 => "40-50 ns (~5 m)",
            0b0110 => "50-60 ns (~6 m)",
            0b0111 => "60-70 ns (~7 m)",
            0b1000 => ">70 ns (>7 m)",
            0b1001 => "1000-2000 ns (active)",
            0b1010 => "2000-3000 ns (active)",
            0b1011 => "3000-4000 ns (active)",
            0b1100 => "4000-5000 ns (active)",
            0b1101 => "5000-6000 ns (active)",
            0b1110 => "6000-7000 ns (active)",
            _ => return None,
        }
        .to_string(),
    )
}

fn cable_termination(v: u32) -> Option<String> {
    Some(
        match v {
            0b00 => "VCONN not required",
            0b01 => "VCONN required",
            0b10 => "one end active, VCONN required",
            0b11 => "both ends active, VCONN required",
            _ => return None,
        }
        .to_string(),
    )
}

fn cable_max_voltage_mv(v: u32) -> u32 {
    match v {
        0b00 => 20_000,
        0b01 => 30_000,
        0b10 => 40_000,
        _ => 50_000,
    }
}

/// VBUS Current Handling Capability. `00` is "reserved" in the spec; in practice
/// it means the cable declares nothing and the Type-C default (3 A) applies.
fn cable_current_ma(v: u32) -> Option<u32> {
    match v {
        0b01 => Some(3000),
        0b10 => Some(5000),
        _ => None,
    }
}

/// USB Highest Speed, shared by cable VDOs and UFP VDO1.
fn usb_speed(v: u32) -> &'static str {
    match v {
        0b000 => "USB 2.0 only (480 Mbps)",
        0b001 => "USB 3.2 Gen 1 (5 Gbps)",
        0b010 => "USB 3.2 / USB4 Gen 2 (10 Gbps)",
        0b011 => "USB4 Gen 3 (20 Gbps)",
        0b100 => "USB4 Gen 4 (40 Gbps)",
        _ => "Reserved",
    }
}

/// Device Capability bitfield from UFP VDO1 (B27..24).
fn device_capability(v: u32) -> Vec<String> {
    let mut out = Vec::new();
    if v & 0b0001 != 0 {
        out.push("USB 2.0 device".to_string());
    }
    if v & 0b0010 != 0 {
        out.push("USB 2.0 billboard".to_string());
    }
    if v & 0b0100 != 0 {
        out.push("USB 3.2 device".to_string());
    }
    if v & 0b1000 != 0 {
        out.push("USB4 device".to_string());
    }
    out
}

/// The highest data rate a cable VDO speed field implies, in Mbps. Used to
/// compare a cable's rating against a link's negotiated rate.
pub fn cable_speed_mbps(vdo1: u32) -> f64 {
    match bits(vdo1, 2, 0) {
        0b000 => 480.0,
        0b001 => 5000.0,
        0b010 => 10000.0,
        0b011 => 20000.0,
        0b100 => 40000.0,
        _ => 0.0,
    }
}

/// Well-known Standard or Vendor IDs seen in alternate modes.
pub fn svid_name(svid: u16) -> Option<&'static str> {
    Some(match svid {
        0xff00 => "USB Type-C Bridge (legacy)",
        0xff01 => "DisplayPort Alt Mode (VESA)",
        0x8087 => "Intel (Thunderbolt 3)",
        0x17ef => "Lenovo",
        0x05ac => "Apple",
        0x04e8 => "Samsung",
        0x2b01 => "USB4 / TBT",
        0x18d1 => "Google",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vdo(raw: u32) -> Option<Vdo> {
        Some(Vdo::new(raw))
    }

    #[test]
    fn bits_extracts_inclusive_ranges() {
        assert_eq!(bits(0b1011_0000, 7, 4), 0b1011);
        assert_eq!(bits(0xffff_ffff, 31, 0), 0xffff_ffff);
        assert_eq!(bits(0x8000_0000, 31, 31), 1);
    }

    /// A 5 A / 20 V / 10 Gbps passive Type-C cable, ~1 m, hw rev 2, fw rev 1.
    #[test]
    fn decodes_a_full_featured_passive_cable() {
        // hw=2 (b31..28), fw=1 (b27..24), plug=Type-C (b19..18=10),
        // latency=0001 (b16..13), vbus=20V (b10..9=00), current=5A (b6..5=10),
        // speed=10Gbps (b2..0=010)
        let raw = 0x2108_2042;
        let id = PartialIdentity {
            product_type_vdo1: vdo(raw),
            ..Default::default()
        };
        let d = decode(&id, IdentityContext::Cable, Some("3.0"));
        assert_eq!(d.hw_version, Some(2));
        assert_eq!(d.fw_version, Some(1));
        assert_eq!(d.cable_plug_type.as_deref(), Some("USB Type-C"));
        assert_eq!(d.cable_latency.as_deref(), Some("<10 ns (~1 m)"));
        assert_eq!(d.cable_max_voltage_mv, Some(20_000));
        assert_eq!(d.cable_current_ma, Some(5000));
        assert_eq!(
            d.cable_max_speed.as_deref(),
            Some("USB 3.2 / USB4 Gen 2 (10 Gbps)")
        );
        assert_eq!(cable_speed_mbps(raw), 10000.0);
    }

    /// The cheap-charging-cable case: 3 A, USB 2.0 only. This is exactly what
    /// the diagnostics need to catch.
    #[test]
    fn decodes_a_usb2_only_3a_cable() {
        // current=01 (3A) at b6..5, speed=000 (USB 2.0) at b2..0
        let raw = 0x0000_0020;
        let id = PartialIdentity {
            product_type_vdo1: vdo(raw),
            ..Default::default()
        };
        let d = decode(&id, IdentityContext::Cable, Some("3.0"));
        assert_eq!(d.cable_current_ma, Some(3000));
        assert_eq!(d.cable_max_speed.as_deref(), Some("USB 2.0 only (480 Mbps)"));
        assert_eq!(cable_speed_mbps(raw), 480.0);
    }

    #[test]
    fn id_header_product_type_depends_on_pd_revision() {
        // 0b100 at b29..27
        let raw = 0x2000_05ac;
        let id = PartialIdentity {
            id_header: vdo(raw),
            ..Default::default()
        };

        let pd2 = decode(&id, IdentityContext::Cable, Some("2.0"));
        assert_eq!(pd2.product_type.as_deref(), Some("Passive Cable"));
        assert_eq!(pd2.vendor_id, Some(0x05ac));

        let pd3 = decode(&id, IdentityContext::Cable, Some("3.0"));
        assert_eq!(pd3.product_type.as_deref(), Some("Active Cable"));

        let unknown = decode(&id, IdentityContext::Cable, None);
        assert!(unknown.product_type.as_deref().unwrap().contains("ambiguous"));
    }

    #[test]
    fn passive_cable_in_pd3_numbering() {
        let id = PartialIdentity {
            id_header: vdo(0b011 << 27),
            ..Default::default()
        };
        let d = decode(&id, IdentityContext::Cable, Some("3.0"));
        assert_eq!(d.product_type.as_deref(), Some("Passive Cable"));
    }

    #[test]
    fn decodes_product_vdo_into_pid_and_bcd() {
        let id = PartialIdentity {
            product: vdo(0x1234_0100),
            ..Default::default()
        };
        let d = decode(&id, IdentityContext::Partner, Some("3.0"));
        assert_eq!(d.product_id, Some(0x1234));
        assert_eq!(d.bcd_device, Some(0x0100));
    }

    #[test]
    fn decodes_partner_ufp_vdo() {
        // device capability = 0b0100 (USB 3.2) at b27..24, speed = 010 at b2..0
        let id = PartialIdentity {
            product_type_vdo1: vdo(0x0400_0002),
            ..Default::default()
        };
        let d = decode(&id, IdentityContext::Partner, Some("3.0"));
        assert_eq!(
            d.partner_max_speed.as_deref(),
            Some("USB 3.2 / USB4 Gen 2 (10 Gbps)")
        );
        assert_eq!(
            d.partner_device_capability.as_deref(),
            Some(&["USB 3.2 device".to_string()][..])
        );
    }

    #[test]
    fn connector_type_ignores_reserved_values() {
        assert!(connector_type(0b00).is_none());
        assert_eq!(
            connector_type(0b11).as_deref(),
            Some("USB Type-C captive plug")
        );
    }

    #[test]
    fn empty_identity_is_detected() {
        assert!(PartialIdentity::default().is_empty());
    }

    #[test]
    fn known_svids_resolve() {
        assert_eq!(svid_name(0xff01), Some("DisplayPort Alt Mode (VESA)"));
        assert_eq!(svid_name(0x8087), Some("Intel (Thunderbolt 3)"));
        assert!(svid_name(0x0042).is_none());
    }
}
