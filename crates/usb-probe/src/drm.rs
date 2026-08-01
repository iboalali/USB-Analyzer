//! Display outputs, from `/sys/class/drm`.
//!
//! # Why a USB tool reads the GPU's connectors
//!
//! DisplayPort Alternate Mode is a USB-C feature, and `/sys/class/typec` can
//! only describe the *negotiation*: which modes each side advertises and which
//! the driver believes was entered. Whether a picture actually came out the
//! other end is not in that class at all — it is in DRM, where a connector
//! either reads `connected` or does not.
//!
//! Those are different claims, and on this hardware they disagree: every local
//! Type-C port advertises DisplayPort Alt Mode with `active = yes`, on both
//! ports, while a charger with zero alternate modes is attached to one of them
//! and nothing is attached to the other. A port's own alt-mode objects describe
//! what the *port* can do; only the partner's say what was entered.
//!
//! # What sysfs will and will not tell you
//!
//! Per connector: whether something is plugged in (`status`), whether the kernel
//! is driving it (`enabled`, `dpms`), the modes it is willing to drive
//! (`modes`), and the display's own EDID.
//!
//! It will **not** tell you the mode currently being scanned out. That lives in
//! the atomic KMS state, reachable through debugfs (root) or a DRM master
//! connection, not through sysfs. So this can say "the display can do
//! 2560x1440 at 144 Hz and the kernel has it enabled" and never "it is running
//! at 2560x1440 right now".
//!
//! Everything here is world-readable and needs no privileges.

use std::path::Path;

use crate::model::{DisplayConnector, DisplayIdentity, DisplayMode};
use crate::sysfs as fsx;

const SYS_DRM: &str = "/sys/class/drm";

/// The base EDID block. Extension blocks follow in 128-byte units.
const EDID_BLOCK: usize = 128;

const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Offsets of the four 18-byte descriptors in the base block.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];

pub fn read() -> Vec<DisplayConnector> {
    read_from(Path::new(SYS_DRM))
}

pub fn read_from(dir: &Path) -> Vec<DisplayConnector> {
    let mut out = Vec::new();
    for entry in fsx::list_dir(dir) {
        let name = fsx::file_name(&entry);
        // Connectors are named `card<N>-<CONNECTOR>`. The card itself, render
        // nodes and the `version` file are not connectors.
        if !name.starts_with("card") {
            continue;
        }
        let Some((_, connector)) = name.split_once('-') else {
            continue;
        };
        // Writeback is a virtual sink for capturing the composited output. It
        // is not a socket and nobody can plug a cable into it.
        if connector.starts_with("Writeback") {
            continue;
        }

        out.push(DisplayConnector {
            connector: connector.to_string(),
            name,
            connector_id: fsx::read_u32(&entry, "connector_id"),
            status: fsx::read_attr(&entry, "status"),
            enabled: fsx::read_bool(&entry, "enabled"),
            dpms: fsx::read_attr(&entry, "dpms"),
            modes: fsx::read_attr(&entry, "modes")
                .map(|m| m.lines().map(str::to_string).collect())
                .unwrap_or_default(),
            display: std::fs::read(entry.join("edid"))
                .ok()
                .as_deref()
                .and_then(decode_edid),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Decode the parts of an EDID that identify a display.
///
/// Returns `None` for anything that is not a valid base block. A disconnected
/// connector's `edid` file is zero bytes, and a marginal DDC line can return a
/// corrupt one — the checksum is the only defence against reporting garbage as
/// a monitor name, so it is enforced rather than ignored.
pub fn decode_edid(raw: &[u8]) -> Option<DisplayIdentity> {
    if raw.len() < EDID_BLOCK || raw[..8] != EDID_HEADER {
        return None;
    }
    let base = &raw[..EDID_BLOCK];
    // Every byte of the block sums to zero, modulo 256.
    if base.iter().fold(0u8, |acc, b| acc.wrapping_add(*b)) != 0 {
        return None;
    }

    let mut id = DisplayIdentity {
        manufacturer: Some(pnp_id(u16::from_be_bytes([base[8], base[9]]))),
        manufacturer_name: None,
        product_code: Some(u16::from_le_bytes([base[10], base[11]])),
        serial: Some(u32::from_le_bytes([base[12], base[13], base[14], base[15]])),
        // Byte 17 is years since 1990. Zero means "not stated", not 1990.
        year: (base[17] > 0).then(|| 1990 + base[17] as u32),
        edid_version: Some(format!("{}.{}", base[18], base[19])),
        name: None,
        serial_text: None,
        preferred_mode: None,
    };
    id.manufacturer_name = id.manufacturer.as_deref().and_then(pnp_vendor).map(str::to_string);

    for off in DESCRIPTORS {
        let d = &base[off..off + 18];
        // A descriptor beginning with two zero bytes is a display descriptor;
        // anything else is a detailed timing.
        if d[0] == 0 && d[1] == 0 {
            let text = || descriptor_text(&d[5..]);
            match d[3] {
                0xfc => id.name = text(),
                0xff => id.serial_text = text(),
                _ => {}
            }
        } else if id.preferred_mode.is_none() {
            // The first detailed timing is the preferred one, by definition.
            id.preferred_mode = detailed_timing(d);
        }
    }
    Some(id)
}

/// The three-letter manufacturer code packed into bytes 8-9: five bits per
/// letter, `A` = 1.
fn pnp_id(v: u16) -> String {
    [(v >> 10) & 0x1f, (v >> 5) & 0x1f, v & 0x1f]
        .into_iter()
        .map(|c| (c as u8 + b'A' - 1) as char)
        .collect()
}

/// Names for the PNP codes likely to turn up on a laptop or a desk. Not a
/// complete registry — the code itself is always reported, this only saves the
/// reader a lookup for the common cases.
fn pnp_vendor(code: &str) -> Option<&'static str> {
    Some(match code {
        "AAA" => "Avolites",
        "ACI" => "Asus",
        "ACR" => "Acer",
        "AOC" => "AOC",
        "APP" => "Apple",
        "AUO" => "AU Optronics",
        "BNQ" => "BenQ",
        "BOE" => "BOE",
        "CMN" => "Chi Mei",
        "DEL" => "Dell",
        "GSM" => "LG",
        "HWP" => "HP",
        "IVM" => "Iiyama",
        "LEN" => "Lenovo",
        "LGD" => "LG Display",
        "MSI" => "MSI",
        "NCP" => "Nec",
        "PHL" => "Philips",
        "SAM" => "Samsung",
        "SDC" => "Samsung Display",
        "SHP" => "Sharp",
        "VSC" => "ViewSonic",
        _ => return None,
    })
}

/// Descriptor text is ASCII padded with `0x0a` then spaces.
fn descriptor_text(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|c| *c == 0x0a).unwrap_or(b.len());
    let s: String = b[..end]
        .iter()
        .map(|c| if c.is_ascii_graphic() || *c == b' ' { *c as char } else { '?' })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// An 18-byte detailed timing descriptor.
///
/// The active and blanking counts are split across a shared byte — the upper
/// four bits of each 12-bit value live in the high and low nibbles of one
/// trailing byte. Refresh follows from the pixel clock divided by the total
/// pixels including blanking, which is why blanking has to be decoded at all.
fn detailed_timing(d: &[u8]) -> Option<DisplayMode> {
    let pixel_clock_khz = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
    let h_active = ((d[4] as u32 >> 4) << 8) | d[2] as u32;
    let h_blank = ((d[4] as u32 & 0xf) << 8) | d[3] as u32;
    let v_active = ((d[7] as u32 >> 4) << 8) | d[5] as u32;
    let v_blank = ((d[7] as u32 & 0xf) << 8) | d[6] as u32;
    if pixel_clock_khz == 0 || h_active == 0 || v_active == 0 {
        return None;
    }
    let total = (h_active + h_blank) as f64 * (v_active + v_blank) as f64;
    Some(DisplayMode {
        width: h_active,
        height: v_active,
        refresh_hz: if total > 0.0 {
            pixel_clock_khz as f64 * 1000.0 / total
        } else {
            0.0
        },
        pixel_clock_khz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The base block of a real LG UltraGear, read from this machine's
    /// `card1-HDMI-A-1`. Keeping real bytes means the decode is checked against
    /// what hardware actually emits, not against my idea of the format.
    const ULTRAGEAR: [u8; 128] = [
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x1e, 0x6d, 0x65, 0x77,
        0x94, 0xd7, 0x01, 0x00, 0x04, 0x1f, 0x01, 0x03, 0x80, 0x46, 0x27, 0x78,
        0xea, 0xcd, 0xb4, 0xa5, 0x56, 0x50, 0xa1, 0x27, 0x13, 0x50, 0x54, 0x21,
        0x08, 0x00, 0xd1, 0xc0, 0x61, 0x40, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x6f, 0xc2, 0x00, 0xa0, 0xa0, 0xa0,
        0x55, 0x50, 0x30, 0x20, 0x35, 0x00, 0xb9, 0x88, 0x21, 0x00, 0x00, 0x1a,
        0x00, 0x00, 0x00, 0xfd, 0x00, 0x30, 0x90, 0x1e, 0xfa, 0x41, 0x00, 0x0a,
        0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0xfc, 0x00, 0x4c,
        0x47, 0x20, 0x55, 0x4c, 0x54, 0x52, 0x41, 0x47, 0x45, 0x41, 0x52, 0x0a,
        0x00, 0x00, 0x00, 0xff, 0x00, 0x31, 0x30, 0x34, 0x4e, 0x54, 0x4a, 0x4a,
        0x33, 0x4a, 0x37, 0x32, 0x34, 0x0a, 0x01, 0x25,
    ];

    #[test]
    fn decodes_a_real_monitor() {
        let id = decode_edid(&ULTRAGEAR).expect("valid base block");
        assert_eq!(id.manufacturer.as_deref(), Some("GSM"));
        assert_eq!(id.manufacturer_name.as_deref(), Some("LG"));
        assert_eq!(id.product_code, Some(0x7765));
        assert_eq!(id.year, Some(2021));
        assert_eq!(id.edid_version.as_deref(), Some("1.3"));
        assert_eq!(id.name.as_deref(), Some("LG ULTRAGEAR"));
        assert_eq!(id.serial_text.as_deref(), Some("104NTJJ3J724"));

        let m = id.preferred_mode.expect("a preferred timing");
        assert_eq!((m.width, m.height), (2560, 1440));
        assert_eq!(m.pixel_clock_khz, 497_750);
        assert!(
            (m.refresh_hz - 120.0).abs() < 0.1,
            "expected ~120 Hz, got {}",
            m.refresh_hz
        );
        assert_eq!(m.describe(), "2560x1440 @ 120 Hz");
    }

    /// A disconnected connector's `edid` is empty, and a corrupt read must not
    /// be reported as a monitor.
    #[test]
    fn rejects_empty_and_corrupt_edid() {
        assert!(decode_edid(&[]).is_none());
        assert!(decode_edid(&[0u8; 128]).is_none(), "no header");

        let mut broken = ULTRAGEAR;
        broken[60] ^= 0xff; // inside the preferred timing, so the checksum fails
        assert!(
            decode_edid(&broken).is_none(),
            "a bad checksum must not yield a monitor name"
        );
    }

    #[test]
    fn reads_connectors_and_skips_what_is_not_one() {
        let base = std::env::temp_dir().join(format!("usbprobe-drm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        let hdmi = base.join("card1-HDMI-A-1");
        fs::create_dir_all(&hdmi).unwrap();
        fs::write(hdmi.join("connector_id"), "108\n").unwrap();
        fs::write(hdmi.join("status"), "connected\n").unwrap();
        fs::write(hdmi.join("enabled"), "enabled\n").unwrap();
        fs::write(hdmi.join("dpms"), "On\n").unwrap();
        fs::write(hdmi.join("modes"), "2560x1440\n1920x1080\n").unwrap();
        fs::write(hdmi.join("edid"), ULTRAGEAR).unwrap();

        let dp = base.join("card1-DP-1");
        fs::create_dir_all(&dp).unwrap();
        fs::write(dp.join("status"), "disconnected\n").unwrap();
        fs::write(dp.join("enabled"), "disabled\n").unwrap();
        fs::write(dp.join("modes"), "").unwrap();
        fs::write(dp.join("edid"), b"").unwrap();

        // None of these are connectors.
        for other in ["card1", "renderD128", "card1-Writeback-1"] {
            fs::create_dir_all(base.join(other)).unwrap();
        }
        fs::write(base.join("version"), "drm 1.1.0\n").unwrap();

        let c = read_from(&base);
        assert_eq!(c.len(), 2, "got {:?}", c.iter().map(|c| &c.name).collect::<Vec<_>>());

        assert_eq!(c[0].name, "card1-DP-1");
        assert!(!c[0].is_connected());
        assert!(c[0].display.is_none());

        let h = &c[1];
        assert_eq!(h.connector, "HDMI-A-1");
        assert_eq!(h.connector_id, Some(108));
        assert!(h.is_connected() && h.is_lit());
        assert_eq!(h.modes.len(), 2);
        assert_eq!(h.label(), "LG ULTRAGEAR");

        let _ = fs::remove_dir_all(&base);
    }

    /// A connected display the kernel is not driving is the normal state of a
    /// monitor that has gone to sleep, and must not read as "in use".
    #[test]
    fn connected_is_not_the_same_as_lit() {
        let base = std::env::temp_dir().join(format!("usbprobe-drm2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let c = base.join("card1-HDMI-A-1");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("status"), "connected\n").unwrap();
        fs::write(c.join("enabled"), "disabled\n").unwrap();
        fs::write(c.join("dpms"), "Off\n").unwrap();

        let read = read_from(&base);
        assert!(read[0].is_connected());
        assert!(!read[0].is_lit());

        let _ = fs::remove_dir_all(&base);
    }
}
