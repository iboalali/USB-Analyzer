//! Stored corrections: what the user says a device is.
//!
//! # Why this exists at all
//!
//! Not for decoration. Both storage devices on the development machine report
//! class `08`, which says *storage* and nothing about **what kind** — and
//! [`crate::model::BlockDevice::medium`] returns `Unknown` for nearly everything
//! behind a USB bridge, because bridges omit SCSI VPD page B1h. That is why the
//! throughput rules have to reason about "the slowest plausible medium" instead
//! of a real threshold. A user who says *"that one is a spinning disk"* supplies
//! a fact no amount of reading can recover.
//!
//! # A declaration may sharpen a finding, not only suppress one
//!
//! The exact opposite of the rule for product-string guesses in
//! [`crate::kind`], and correctly so. A guess may only quieten a rule because
//! the tool invented it. A user assertion is better evidence than anything on
//! the wire — they are holding the object.
//!
//! It is still not a measurement, so any finding resting on one is capped at
//! [`Confidence::Inferred`] and cites where the fact came from. `Measured`
//! means read off the hardware, and a declaration is not that however true it
//! is.
//!
//! # Remember, never generalise
//!
//! A correction is a stored fact about one identity, replayed on every future
//! sighting. Nothing here mines corrections for patterns and starts guessing: a
//! rule inferred from user data would produce findings with no traceable cause,
//! which is the one thing this project cannot afford. Every stored override is
//! listable and deletable, because a belief the user cannot inspect is one they
//! cannot correct.
//!
//! # Scope defaults to the model, with the unit as an escape hatch
//!
//! `VID:PID`, so correcting one SanDisk Ultra corrects every SanDisk Ultra;
//! `VID:PID:serial` for just this one. **Trustworthy** is doing real work in
//! that sentence — see [`usable_serial`]. Two of the six serials on this
//! machine are placeholders, and keying on those naively would relabel every
//! zero-serial device ever plugged in.
//!
//! # This is the project's first persistent state
//!
//! Worth being reluctant about. `$XDG_CONFIG_HOME/usbdiag/devices.json`, JSON
//! because `serde_json` is already a dev-dependency of this crate and a real
//! dependency of the CLI. An absent file means no overrides and no error, and
//! **only an explicit command writes it** — no read path ever persists
//! anything.
//!
//! It does change what a capture *is*: a pure function of the machine becomes a
//! function of the machine and a config file, so two runs on identical hardware
//! can differ. That is why `--no-overrides` exists and why every applied
//! declaration is visible in the JSON.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::kind::DeviceKind;
use crate::model::{Medium, UsbDevice};

/// The shortest serial worth keying on. Anything shorter is a counter, not an
/// identity.
const MIN_SERIAL_LEN: usize = 4;

/// One stored correction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    /// `vvvv:pppp` for a model, `vvvv:pppp:serial` for one unit.
    ///
    /// A single string rather than three fields because the file is meant to be
    /// hand-edited, and `0781:5583` is what a person recognises.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DeviceKind>,
    /// For storage: what the platters (or lack of them) actually are. The case
    /// that justifies the whole feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<Medium>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub set_at_unix_ms: u64,
}

impl Override {
    /// True when this entry names one physical unit rather than a model.
    pub fn is_unit(&self) -> bool {
        self.id.split(':').count() >= 3
    }

    /// `vvvv:pppp`, dropping any serial.
    pub fn model_id(&self) -> String {
        self.id.split(':').take(2).collect::<Vec<_>>().join(":")
    }

    pub fn scope_label(&self) -> &'static str {
        if self.is_unit() {
            "this unit"
        } else {
            "this model"
        }
    }

    /// Does this entry say anything at all?
    pub fn is_empty(&self) -> bool {
        self.kind.is_none() && self.medium.is_none() && self.note.is_none()
    }
}

/// The whole file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides {
    /// Bumped only if the shape changes incompatibly; readers ignore unknown
    /// higher versions rather than guessing.
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub devices: Vec<Override>,
}

fn one() -> u32 {
    1
}

impl Overrides {
    pub fn new() -> Self {
        Self {
            version: 1,
            devices: Vec::new(),
        }
    }

    /// Read the user's file. An absent or unreadable file is *no overrides*,
    /// not an error: a diagnostic tool must still run when its config is
    /// missing, and there is nothing here a user needs to be told about.
    pub fn load() -> Self {
        match default_path() {
            Some(p) => Self::load_from(&p),
            None => Self::new(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::new();
        };
        serde_json::from_str(&text).unwrap_or_else(|_| Self::new())
    }

    /// Write the file, creating its directory. Only ever called from an
    /// explicit command.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        text.push('\n');
        std::fs::write(path, text)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = default_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no config directory (set $XDG_CONFIG_HOME or $HOME)",
            )
        })?;
        self.save_to(&path)
    }

    /// The entry that applies to a device, if any.
    ///
    /// A unit entry beats a model entry, so "just this one" genuinely means
    /// just this one even when the model is also labelled.
    pub fn matching(&self, dev: &UsbDevice) -> Option<&Override> {
        let unit = unit_id(dev);
        if let Some(u) = &unit {
            if let Some(o) = self.devices.iter().find(|o| &o.id == u) {
                return Some(o);
            }
        }
        let model = model_id(dev)?;
        self.devices.iter().find(|o| o.id == model)
    }

    /// Add or replace the entry for `id`, keeping the file free of duplicates.
    pub fn set(&mut self, entry: Override) {
        self.devices.retain(|o| o.id != entry.id);
        if !entry.is_empty() {
            self.devices.push(entry);
        }
        self.devices.sort_by(|a, b| a.id.cmp(&b.id));
    }

    /// Remove one entry by id. Returns whether anything was removed.
    pub fn forget(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|o| o.id != id);
        self.devices.len() != before
    }
}

/// What a matched override asserts about a device, carried on the device itself
/// so a JSON consumer can see it was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    /// The id it matched, so the user can find and delete it.
    pub id: String,
    /// True when it names this physical unit rather than the model.
    pub unit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DeviceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<Medium>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Declaration {
    /// The evidence line a finding must cite when it leans on this.
    pub fn cite(&self) -> String {
        format!("declared by you for {} ({})", self.id, self.scope_label())
    }

    pub fn scope_label(&self) -> &'static str {
        if self.unit {
            "this unit"
        } else {
            "this model"
        }
    }
}

impl From<&Override> for Declaration {
    fn from(o: &Override) -> Self {
        Self {
            id: o.id.clone(),
            unit: o.is_unit(),
            kind: o.kind,
            medium: o.medium,
            note: o.note.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// `vvvv:pppp`, or `None` when the device reports no ids.
pub fn model_id(dev: &UsbDevice) -> Option<String> {
    Some(format!("{:04x}:{:04x}", dev.id_vendor?, dev.id_product?))
}

/// `vvvv:pppp:serial`, or `None` when there is no serial worth keying on.
pub fn unit_id(dev: &UsbDevice) -> Option<String> {
    let serial = dev.serial.as_deref().filter(|s| usable_serial(s))?;
    Some(format!("{}:{}", model_id(dev)?, serial.trim()))
}

/// Can this serial identify one physical unit?
///
/// Placeholders are common and dangerous. On the development machine the
/// MediaTek radio reports `000000000` and the Dell DA20 reports
/// `00000000000000000`; keying on those would make "just this one adapter"
/// silently mean "every zero-serial device ever plugged in". The three genuine
/// serials here — `4C530001010412118490`, `54B80A3FA797C091604B95`,
/// `UID9802CAEE_XXXX_MOC_B` — show what a real one looks like.
///
/// Rejects: empty, shorter than [`MIN_SERIAL_LEN`], and anything that is one
/// character repeated. Deliberately not cleverer than that — a rule that tries
/// to spot "looks fake" would eventually reject somebody's real serial, and the
/// cost of a false reject (falls back to model scope) is much lower than the
/// cost of a false accept.
pub fn usable_serial(serial: &str) -> bool {
    let s = serial.trim();
    if s.len() < MIN_SERIAL_LEN {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    !chars.all(|c| c == first)
}

/// `$XDG_CONFIG_HOME/usbdiag/devices.json`, or the `$HOME/.config` fallback.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("usbdiag").join("devices.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as ts;

    fn drive(vendor: u16, product: u16, serial: Option<&str>) -> UsbDevice {
        let mut d = ts::device("4-1", "3.00", 5000.0, Some("usb4"));
        d.id_vendor = Some(vendor);
        d.id_product = Some(product);
        d.serial = serial.map(str::to_string);
        d
    }

    fn entry(id: &str, kind: Option<DeviceKind>, medium: Option<Medium>) -> Override {
        Override {
            id: id.into(),
            kind,
            medium,
            note: None,
            set_at_unix_ms: 0,
        }
    }

    // --- serials ---

    /// The two placeholders actually present on the development machine.
    #[test]
    fn the_placeholder_serials_on_this_machine_are_rejected() {
        assert!(!usable_serial("000000000"));
        assert!(!usable_serial("00000000000000000"));
    }

    #[test]
    fn the_real_serials_on_this_machine_are_accepted() {
        assert!(usable_serial("4C530001010412118490"));
        assert!(usable_serial("54B80A3FA797C091604B95"));
        assert!(usable_serial("UID9802CAEE_XXXX_MOC_B"));
    }

    #[test]
    fn short_and_empty_serials_are_rejected() {
        assert!(!usable_serial(""));
        assert!(!usable_serial("   "));
        assert!(!usable_serial("0"));
        assert!(!usable_serial("123"));
        assert!(usable_serial("1234"));
    }

    #[test]
    fn a_repeated_character_is_not_an_identity() {
        assert!(!usable_serial("AAAAAAAA"));
        assert!(!usable_serial("--------"));
        assert!(usable_serial("AAAAAAAB"));
    }

    /// A device with a junk serial must fall back to model scope rather than
    /// getting a unit id nobody can trust.
    #[test]
    fn a_junk_serial_leaves_only_a_model_id() {
        let d = drive(0x0e8d, 0xe025, Some("000000000"));
        assert_eq!(model_id(&d).as_deref(), Some("0e8d:e025"));
        assert_eq!(unit_id(&d), None);
    }

    #[test]
    fn a_real_serial_produces_a_unit_id() {
        let d = drive(0x0781, 0x5583, Some("4C530001010412118490"));
        assert_eq!(
            unit_id(&d).as_deref(),
            Some("0781:5583:4C530001010412118490")
        );
    }

    // --- matching ---

    #[test]
    fn a_model_entry_matches_every_unit_of_that_model() {
        let mut o = Overrides::new();
        o.set(entry("0781:5583", Some(DeviceKind::Storage), None));
        assert!(o.matching(&drive(0x0781, 0x5583, Some("AAAA0001"))).is_some());
        assert!(o.matching(&drive(0x0781, 0x5583, Some("BBBB0002"))).is_some());
        assert!(o.matching(&drive(0x0781, 0x9999, None)).is_none());
    }

    /// The escape hatch has to actually escape: a unit entry must win over a
    /// model entry, or "just this one" is a lie.
    #[test]
    fn a_unit_entry_beats_a_model_entry() {
        let mut o = Overrides::new();
        o.set(entry("0781:5583", None, Some(Medium::SolidState)));
        o.set(entry("0781:5583:4C530001010412118490", None, Some(Medium::Rotating)));

        let this_one = drive(0x0781, 0x5583, Some("4C530001010412118490"));
        assert_eq!(o.matching(&this_one).unwrap().medium, Some(Medium::Rotating));

        let another = drive(0x0781, 0x5583, Some("54B80A3FA797C091604B95"));
        assert_eq!(o.matching(&another).unwrap().medium, Some(Medium::SolidState));
    }

    /// The placeholder trap, end to end: labelling one zero-serial device as a
    /// unit is impossible, so a second zero-serial device of a *different*
    /// model cannot inherit it.
    #[test]
    fn a_placeholder_serial_cannot_be_used_to_label_one_unit() {
        let d = drive(0x0e8d, 0xe025, Some("000000000"));
        assert_eq!(unit_id(&d), None, "there is no unit id to store");

        let mut o = Overrides::new();
        o.set(entry("0e8d:e025", Some(DeviceKind::Wireless), None));
        // A different model with the same junk serial is untouched.
        let other = drive(0x1234, 0x5678, Some("000000000"));
        assert!(o.matching(&other).is_none());
    }

    // --- the file ---

    #[test]
    fn setting_the_same_id_twice_replaces_rather_than_duplicates() {
        let mut o = Overrides::new();
        o.set(entry("0781:5583", Some(DeviceKind::Storage), None));
        o.set(entry("0781:5583", Some(DeviceKind::Camera), None));
        assert_eq!(o.devices.len(), 1);
        assert_eq!(o.devices[0].kind, Some(DeviceKind::Camera));
    }

    /// Setting an entry that asserts nothing removes it — otherwise clearing a
    /// label would leave a tombstone the user cannot see the point of.
    #[test]
    fn an_empty_entry_is_removed_rather_than_stored() {
        let mut o = Overrides::new();
        o.set(entry("0781:5583", Some(DeviceKind::Storage), None));
        o.set(entry("0781:5583", None, None));
        assert!(o.devices.is_empty());
    }

    #[test]
    fn forget_reports_whether_it_removed_anything() {
        let mut o = Overrides::new();
        o.set(entry("0781:5583", Some(DeviceKind::Storage), None));
        assert!(o.forget("0781:5583"));
        assert!(!o.forget("0781:5583"));
    }

    #[test]
    fn a_missing_file_is_no_overrides_and_no_error() {
        let o = Overrides::load_from(Path::new("/nonexistent/usbdiag/devices.json"));
        assert!(o.devices.is_empty());
    }

    /// A hand-edited file that has been broken must not take the tool down with
    /// it. The user gets their diagnosis; the labels are what is lost.
    #[test]
    fn a_corrupt_file_is_no_overrides_and_no_error() {
        let dir = std::env::temp_dir().join("usbdiag-test-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(Overrides::load_from(&path).devices.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_file_round_trips() {
        let dir = std::env::temp_dir().join("usbdiag-test-roundtrip");
        let path = dir.join("devices.json");
        let _ = std::fs::remove_file(&path);

        let mut o = Overrides::new();
        o.set(Override {
            id: "0781:5583".into(),
            kind: Some(DeviceKind::Storage),
            medium: Some(Medium::Rotating),
            note: Some("the old Seagate".into()),
            set_at_unix_ms: 1_700_000_000_000,
        });
        o.save_to(&path).unwrap();

        let back = Overrides::load_from(&path);
        assert_eq!(back.devices, o.devices);
        assert_eq!(back.version, 1);

        // Hand-editable: the id is one readable string, not three fields.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"0781:5583\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_declaration_carries_the_id_so_it_can_be_found_and_deleted() {
        let o = entry("0781:5583:4C530001010412118490", Some(DeviceKind::Storage), None);
        let d = Declaration::from(&o);
        assert!(d.unit);
        assert_eq!(d.scope_label(), "this unit");
        assert!(d.cite().contains("0781:5583:4C530001010412118490"));
    }

    #[test]
    fn the_config_path_lands_under_xdg_config_home() {
        // Not asserting the real environment, only the shape of the answer.
        let p = default_path().expect("HOME is set in a test environment");
        assert!(p.ends_with("usbdiag/devices.json"), "{}", p.display());
    }
}
