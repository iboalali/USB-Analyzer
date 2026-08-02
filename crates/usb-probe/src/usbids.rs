//! Vendor and product names, read from the system USB ID database.
//!
//! Device names normally come from the sysfs `manufacturer` and `product`
//! strings, which is fine for devices and useless for cables: an e-marker
//! carries a vendor ID in its ID Header VDO and never a string. Without a table
//! we would print `0x2109` where a name exists.
//!
//! # Read the system copy, do not bundle one
//!
//! `/usr/share/misc/usb.ids` ships with the `usb.ids` package and
//! `/usr/share/hwdata/usb.ids` with `hwdata` — on this machine the first is a
//! symlink to the second, and both are present on anything that has `lsusb`.
//! Reading at runtime beats bundling on every axis that matters: it cannot go
//! stale, it adds no data file to this repository, and it raises no licence
//! question.
//!
//! It is also better data. The system table names
//! `27c6  Shenzhen Goodix Technology Co.,Ltd.` and `05e3  Genesys Logic, Inc.`,
//! the latter being a hub whose sysfs `manufacturer` is absent entirely — so
//! this is the difference between naming the vendor and printing four hex
//! digits.
//!
//! # Absence is an answer, not a failure
//!
//! Hex is the correct output when the file is not installed. [`available`]
//! exists so a *rule* can tell the two apart: "this vendor id is in no database"
//! means nothing at all when there is no database, and a suspicion rule that
//! ignores the difference turns every cable on a minimal system into a suspect.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where the database lives, most specific first. Both are the same file here.
const CANDIDATES: [&str; 2] = ["/usr/share/misc/usb.ids", "/usr/share/hwdata/usb.ids"];

/// The parsed table: vendors, and products keyed by `(vid, pid)`.
#[derive(Debug, Default)]
pub struct UsbIds {
    /// The file this came from. `None` when no database was found.
    pub source: Option<PathBuf>,
    vendors: HashMap<u16, String>,
    products: HashMap<(u16, u16), String>,
}

impl UsbIds {
    /// True when a database was actually found and parsed.
    ///
    /// A rule that reasons about an *unknown* vendor id must check this: with no
    /// table, every id is unknown and the conclusion is meaningless.
    pub fn available(&self) -> bool {
        self.source.is_some() && !self.vendors.is_empty()
    }

    pub fn vendor(&self, vid: u16) -> Option<&str> {
        self.vendors.get(&vid).map(String::as_str)
    }

    pub fn product(&self, vid: u16, pid: u16) -> Option<&str> {
        self.products.get(&(vid, pid)).map(String::as_str)
    }

    /// `Genesys Logic, Inc.`, or `05e3` when the table cannot name it.
    ///
    /// Hex rather than "unknown": the number is a fact and is what someone would
    /// search for, while the word is only an admission.
    pub fn vendor_or_hex(&self, vid: u16) -> String {
        self.vendor(vid)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{vid:04x}"))
    }

    pub fn len(&self) -> usize {
        self.vendors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vendors.is_empty()
    }
}

/// The system database, parsed once per process.
///
/// 728 KB and ~26 000 lines, so it is worth not doing twice — but it is also
/// only done when something asks, which for a run that names no cable and no
/// stringless device is never.
pub fn system() -> &'static UsbIds {
    static TABLE: OnceLock<UsbIds> = OnceLock::new();
    TABLE.get_or_init(|| {
        CANDIDATES
            .iter()
            .map(Path::new)
            .find(|p| p.exists())
            .map(load)
            .unwrap_or_default()
    })
}

/// Parse one `usb.ids` file. An unreadable file yields an empty table.
pub fn load(path: &Path) -> UsbIds {
    let Ok(text) = std::fs::read_to_string(path) else {
        return UsbIds::default();
    };
    let mut out = parse(&text);
    out.source = Some(path.to_path_buf());
    out
}

/// The file is two significant shapes:
///
/// ```text
/// 05e3  Genesys Logic, Inc.
/// \t0610  Hub
/// ```
///
/// Everything after the first `C 00` line is the device-class list, which uses
/// the same indentation for entirely different numbers — parsing on past it
/// would file class codes as vendors.
fn parse(text: &str) -> UsbIds {
    let mut out = UsbIds::default();
    let mut current: Option<u16> = None;

    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `C 01  Audio` and the tables after it (AT, HID, R, ...) are not
        // vendors. They all start at column zero with a short token and a
        // space, which no vendor line does.
        if !line.starts_with('\t') && line.as_bytes().get(1) == Some(&b' ') {
            break;
        }

        if let Some(rest) = line.strip_prefix("\t\t") {
            // Interface names. Nothing here needs them.
            let _ = rest;
            continue;
        }
        if let Some(rest) = line.strip_prefix('\t') {
            if let (Some(vid), Some((pid, name))) = (current, split_id(rest)) {
                out.products.insert((vid, pid), name.to_string());
            }
            continue;
        }
        match split_id(line) {
            Some((vid, name)) => {
                current = Some(vid);
                out.vendors.insert(vid, name.to_string());
            }
            None => current = None,
        }
    }
    out
}

/// `05e3  Genesys Logic, Inc.` -> `(0x05e3, "Genesys Logic, Inc.")`
fn split_id(line: &str) -> Option<(u16, &str)> {
    let (id, name) = line.split_once("  ")?;
    let id = u16::from_str_radix(id.trim(), 16).ok()?;
    let name = name.trim();
    (!name.is_empty()).then_some((id, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
#\tList of USB ID's
# Version: 2024.03.18

0001  Fry's Electronics
\t7778  Counterfeit flash drive [Kingston]
05e3  Genesys Logic, Inc.
\t0608  Hub
\t0610  Hub
\t\t0001  an interface, which is not a product
27c6  Shenzhen Goodix Technology Co.,Ltd.

# Device classes
C 00  (Defined at Interface level)
C 01  Audio
\t01  Control Device
";

    #[test]
    fn parses_vendors_and_products() {
        let ids = parse(SAMPLE);
        assert_eq!(ids.vendor(0x05e3), Some("Genesys Logic, Inc."));
        assert_eq!(ids.vendor(0x27c6), Some("Shenzhen Goodix Technology Co.,Ltd."));
        assert_eq!(ids.product(0x05e3, 0x0610), Some("Hub"));
        assert_eq!(ids.vendor(0x9999), None);
    }

    /// The class tables reuse the same indentation for entirely different
    /// numbers. Reading past `C 00` files audio class codes as vendors.
    #[test]
    fn the_device_class_section_is_not_a_vendor_list() {
        let ids = parse(SAMPLE);
        // `C 01  Audio` must not have become vendor 0x01.
        assert_eq!(ids.vendor(0x0001), Some("Fry's Electronics"));
        assert_eq!(ids.len(), 3, "exactly the three real vendors");
    }

    /// Two-tab lines are interface names under a product, not products.
    #[test]
    fn interface_lines_are_not_products() {
        let ids = parse(SAMPLE);
        assert_eq!(ids.product(0x05e3, 0x0001), None);
    }

    #[test]
    fn an_unknown_vendor_renders_as_hex_not_as_a_word() {
        let ids = parse(SAMPLE);
        assert_eq!(ids.vendor_or_hex(0x05e3), "Genesys Logic, Inc.");
        assert_eq!(ids.vendor_or_hex(0x2ce3), "2ce3");
    }

    /// A missing database is not an error, and must be distinguishable from a
    /// database that simply does not know a vendor.
    #[test]
    fn a_missing_file_is_an_empty_table_that_says_so() {
        let ids = load(Path::new("/nonexistent/usb.ids"));
        assert!(!ids.available());
        assert!(ids.is_empty());
        assert_eq!(ids.vendor(0x05e3), None);
        assert_eq!(ids.vendor_or_hex(0x05e3), "05e3");
    }

    #[test]
    fn a_parsed_table_reports_itself_as_available() {
        let mut ids = parse(SAMPLE);
        assert!(!ids.available(), "no source path yet");
        ids.source = Some(PathBuf::from("/usr/share/misc/usb.ids"));
        assert!(ids.available());
    }

    /// Against the real file on this machine, where one is installed. Skipped
    /// rather than failed elsewhere: this asserts the parser against the
    /// genuine article, and its absence is not a bug in the parser.
    #[test]
    fn the_system_table_names_the_hardware_in_this_laptop() {
        let ids = system();
        if !ids.available() {
            eprintln!("no system usb.ids installed; skipping");
            return;
        }
        assert!(ids.len() > 1000, "only {} vendors parsed", ids.len());
        assert_eq!(ids.vendor(0x05e3), Some("Genesys Logic, Inc."));
        assert_eq!(ids.vendor(0x046d), Some("Logitech, Inc."));
        assert_eq!(
            ids.vendor(0x27c6),
            Some("Shenzhen Goodix Technology Co.,Ltd.")
        );
        // The EMV reader's vendor is genuinely absent from the database, which
        // is the case the counterfeit rule must not treat as suspicious on its
        // own.
        assert_eq!(ids.vendor(0x2ce3), None);
    }
}
