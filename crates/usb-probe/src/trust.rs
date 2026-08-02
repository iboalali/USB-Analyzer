//! Weak signals that a cable's e-marker was not written by whoever it claims.
//!
//! After `usbeehive::cable::CableTrust` (MIT). Three signals, **none
//! conclusive**, and the whole design here is about keeping them that way.
//!
//! # Why this is the most dangerous rule in the project
//!
//! *"Your cable may be counterfeit"* is the most damaging sentence this tool
//! could emit wrongly. A cheap-but-honest cable from a vendor who never
//! registered with the USB-IF trips two of the three signals on its own, and
//! the user's response to being told is to throw away a working cable.
//!
//! So: [`Confidence::Heuristic`] always, `Info` for one signal and `Low` for
//! more, and the word *counterfeit* appears nowhere in the output. What the
//! finding says is that fields are unusual, and what it says next is that an
//! honest cheap cable looks like this too.
//!
//! # Two of the three can only run under conditions
//!
//! **Unknown vendor id is only as good as the database.** With no `usb.ids`
//! installed every vendor is unknown, and the signal would fire on every cable
//! on the machine. [`UsbIds::available`] gates it.
//!
//! **Reserved bits are only readable when the layout is known.** [`crate::vdo`]
//! already refuses to guess between the PD 2.0 and PD 3.x product-type
//! encodings and reports *"Passive or Active Cable (ambiguous without PD
//! revision)"*. That uncertainty is inherited here: the Passive and Active
//! Cable VDOs put different fields at the same offsets, so unless the cable is
//! known to be passive, the check does not run at all. Even then only the bits
//! reserved in *both* revisions are examined by default — PD 2.0 uses B4..B3
//! for SuperSpeed directionality where PD 3.x reserves them, and reading those
//! without knowing the revision would fire on healthy PD 2.0 cables.

use crate::model::{Cable, Confidence, Identity};
use crate::usbids::UsbIds;

/// Bits reserved in the Passive Cable VDO under **both** PD 2.0 and PD 3.x.
/// Safe to examine whenever the cable is known to be passive.
const RESERVED_EITHER_REVISION: u32 = (1 << 20) | (1 << 17) | (1 << 8) | (1 << 7);

/// Reserved under PD 3.x only. PD 2.0 puts SuperSpeed directionality support
/// here, so examining these without a known revision accuses honest hardware.
const RESERVED_PD3_ONLY: u32 = (1 << 4) | (1 << 3);

/// One reason to look twice. Never a reason to conclude anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The ID Header VDO declares vendor 0000, which no real vendor owns.
    ZeroVendorId,
    /// A vendor id the USB-IF database does not list.
    VendorNotInDatabase(u16),
    /// Bits the specification reserves are set. `mask` is what was set.
    ReservedBitsSet { mask: u32, checked: u32 },
}

impl Signal {
    /// One line, phrased as an observation rather than an accusation.
    pub fn describe(&self) -> String {
        match self {
            Self::ZeroVendorId => {
                "the e-marker declares vendor id 0000, which is not assigned to anyone".into()
            }
            Self::VendorNotInDatabase(vid) => format!(
                "vendor id {vid:04x} is not in the USB-IF database on this machine"
            ),
            Self::ReservedBitsSet { mask, checked } => format!(
                "cable VDO has reserved bits set (0x{mask:08x} of the 0x{checked:08x} checked)"
            ),
        }
    }
}

/// Every signal a cable trips. Empty is the ordinary case.
///
/// `ids` decides whether the database-dependent signal can run at all.
pub fn signals(cable: &Cable, ids: &UsbIds) -> Vec<Signal> {
    let Some(id) = &cable.identity else {
        return Vec::new();
    };
    let mut out = Vec::new();

    match id.decoded.vendor_id {
        // Only when the header was actually read: a cable with no ID Header VDO
        // reports no vendor at all, which is silence rather than a zero.
        Some(0) if id.id_header.is_some() => out.push(Signal::ZeroVendorId),
        // Gated on the database existing. Without it, "not listed" is a
        // statement about this machine and not about the cable.
        Some(vid) if vid != 0 && ids.available() && ids.vendor(vid).is_none() => {
            out.push(Signal::VendorNotInDatabase(vid))
        }
        _ => {}
    }

    if let Some(sig) = reserved_bits(id) {
        out.push(sig);
    }
    out
}

/// The reserved-bit check, or `None` when the layout cannot be pinned down.
fn reserved_bits(id: &Identity) -> Option<Signal> {
    let vdo1 = id.product_type_vdo1?.raw;
    let product_type = id.decoded.product_type.as_deref()?;

    // Anything but a cable known to be passive is out of scope. The Active
    // Cable VDO puts different fields at these offsets, and the ambiguous
    // reading — which `vdo.rs` produces on purpose when the PD revision is
    // unknown — is exactly the case that must not be guessed at.
    if product_type != "Passive Cable" {
        return None;
    }

    let checked = RESERVED_EITHER_REVISION;
    let mask = vdo1 & checked;
    (mask != 0).then_some(Signal::ReservedBitsSet { mask, checked })
}

/// Bits that could also be examined if the PD revision were known to be 3.x.
///
/// Exposed so the omission is visible rather than silently absent: this is what
/// the check gives up by refusing to guess the revision.
pub fn bits_not_checked() -> u32 {
    RESERVED_PD3_ONLY
}

/// Always. A pattern match on symptoms is never more than a heuristic, and the
/// consequence of being wrong here is someone binning a working cable.
pub const CONFIDENCE: Confidence = Confidence::Heuristic;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdentityDecoded, Vdo};
    use crate::usbids;
    use std::path::Path;

    fn cable(vendor_id: Option<u16>, product_type: &str, vdo1: Option<u32>) -> Cable {
        Cable {
            sysfs_name: "port0-cable".into(),
            kind: Some("passive".into()),
            plug_type: Some("type-c".into()),
            pd_revision: Some("3.0".into()),
            identity: Some(Identity {
                id_header: vendor_id.map(|v| Vdo::new(v as u32)),
                cert_stat: None,
                product: None,
                product_type_vdo1: vdo1.map(Vdo::new),
                product_type_vdo2: None,
                product_type_vdo3: None,
                decoded: IdentityDecoded {
                    vendor_id,
                    product_type: Some(product_type.into()),
                    ..Default::default()
                },
            }),
        }
    }

    /// A real table, so "not in the database" means what it says.
    fn ids() -> UsbIds {
        let t = usbids::system();
        if t.available() {
            usbids::load(Path::new("/usr/share/misc/usb.ids"))
        } else {
            UsbIds::default()
        }
    }

    #[test]
    fn an_ordinary_cable_trips_nothing() {
        let table = ids();
        if !table.available() {
            eprintln!("no usb.ids; skipping");
            return;
        }
        // Genesys Logic, a real registered vendor, and no reserved bits.
        let c = cable(Some(0x05e3), "Passive Cable", Some(0x0008_2042));
        assert!(signals(&c, &table).is_empty());
    }

    #[test]
    fn vendor_zero_is_a_signal() {
        let c = cable(Some(0), "Passive Cable", Some(0));
        assert_eq!(signals(&c, &UsbIds::default()), vec![Signal::ZeroVendorId]);
    }

    /// The trap that would make every cable suspicious on a minimal system.
    #[test]
    fn an_unknown_vendor_is_not_a_signal_without_a_database() {
        let empty = UsbIds::default();
        assert!(!empty.available());
        let c = cable(Some(0xfffe), "Passive Cable", Some(0));
        assert!(
            signals(&c, &empty).is_empty(),
            "with no database, 'not in the database' says nothing"
        );
    }

    #[test]
    fn an_unknown_vendor_is_a_signal_when_the_database_is_present() {
        let table = ids();
        if !table.available() {
            eprintln!("no usb.ids; skipping");
            return;
        }
        let c = cable(Some(0xfffe), "Passive Cable", Some(0));
        assert_eq!(
            signals(&c, &table),
            vec![Signal::VendorNotInDatabase(0xfffe)]
        );
    }

    #[test]
    fn reserved_bits_set_on_a_known_passive_cable_are_a_signal() {
        // B17 set.
        let c = cable(Some(0x05e3), "Passive Cable", Some(1 << 17));
        let s = signals(&c, &UsbIds::default());
        assert_eq!(
            s,
            vec![Signal::ReservedBitsSet {
                mask: 1 << 17,
                checked: RESERVED_EITHER_REVISION
            }]
        );
    }

    /// The inherited-uncertainty rule. `vdo.rs` produces this exact string when
    /// the PD revision is unknown, and the two encodings disagree about what
    /// lives at these offsets — so the check must not run.
    #[test]
    fn an_ambiguous_product_type_disables_the_reserved_bit_check() {
        let c = cable(
            Some(0x05e3),
            "Passive or Active Cable (ambiguous without PD revision)",
            Some(0xffff_ffff),
        );
        assert!(
            signals(&c, &UsbIds::default()).is_empty(),
            "every reserved bit is set and it still must not fire"
        );
    }

    /// An active cable's VDO has a different layout, so these offsets mean
    /// something else entirely.
    #[test]
    fn an_active_cable_is_out_of_scope() {
        let c = cable(Some(0x05e3), "Active Cable", Some(0xffff_ffff));
        assert!(signals(&c, &UsbIds::default()).is_empty());
    }

    /// PD 2.0 puts SuperSpeed directionality support in B4..B3, so a healthy
    /// PD 2.0 cable has them set. They must not be part of the default check.
    #[test]
    fn the_pd2_directionality_bits_are_never_examined() {
        let c = cable(Some(0x05e3), "Passive Cable", Some((1 << 4) | (1 << 3)));
        assert!(
            signals(&c, &UsbIds::default()).is_empty(),
            "B4..B3 are directionality under PD 2.0 and must not accuse it"
        );
        assert_eq!(bits_not_checked(), (1 << 4) | (1 << 3));
    }

    #[test]
    fn a_cable_with_no_identity_says_nothing() {
        let mut c = cable(Some(0), "Passive Cable", Some(0));
        c.identity = None;
        assert!(signals(&c, &UsbIds::default()).is_empty());
    }

    /// A cable that reports no ID Header at all has no vendor, which is not the
    /// same as a vendor of zero.
    #[test]
    fn a_missing_id_header_is_silence_not_a_zero_vendor() {
        let c = cable(None, "Passive Cable", Some(0));
        assert!(signals(&c, &UsbIds::default()).is_empty());
    }

    #[test]
    fn every_signal_reads_as_an_observation() {
        for s in [
            Signal::ZeroVendorId,
            Signal::VendorNotInDatabase(0xfffe),
            Signal::ReservedBitsSet {
                mask: 1 << 17,
                checked: RESERVED_EITHER_REVISION,
            },
        ] {
            let text = s.describe();
            assert!(!text.is_empty());
            for word in ["counterfeit", "fake", "fraud"] {
                assert!(!text.contains(word), "{text:?} must not say {word}");
            }
        }
    }
}
