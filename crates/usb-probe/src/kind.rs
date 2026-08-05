//! What a device *is*, so an observation can be read in context.
//!
//! This is not decoration, and it is not a prettier `class_name`. What a device
//! is changes what a reading *means*: 12 Mbps is correct for a keyboard and a
//! fault for an external SSD; 900 mA is a phone charging and a mouse
//! malfunctioning. The taxonomy is a rule input, which is exactly why it needs
//! rules of its own.
//!
//! # A guess may quieten a rule; it may never start one
//!
//! [`KindSource`] splits *asserted* from *guessed*, and the split is in the API
//! rather than in a comment:
//!
//! * [`Kind::grounds`] returns a kind only when someone who could actually know
//!   said so — the device's own descriptors, or the user. It is the only
//!   accessor a rule may use as grounds for a **new** finding, and
//!   [`Kind::cap`] weakens the confidence such a finding may claim.
//! * [`Kind::kind`] is the best available answer, guesses included. It is for
//!   display and for staying quiet.
//!
//! The asymmetry is not stylistic. A wrong guess that suppresses costs a missed
//! detection, which is bad. A wrong guess that accuses costs a false accusation
//! against hardware the user then goes and replaces, which is the failure this
//! whole project exists to avoid. Only one of those is recoverable, so a guess
//! the *tool* made may push toward silence and never toward blame.
//!
//! A user's correction is the other way round, and deliberately so: they are
//! holding the object, so it may sharpen a finding as well as quieten one. It
//! is still not a measurement, so it caps at [`crate::Confidence::Inferred`]
//! and has to be cited. See [`crate::overrides`].
//!
//! # Only the free half is built
//!
//! Classification here is by class code alone — the device asserting what it
//! is, not us inferring it. Run against the development machine it names every
//! attached device but one: the hub, the Bluetooth radio (`bDeviceClass ef`,
//! deferring to three `0xe0` interfaces) and the smart-card reader all answer.
//! The exception is a Goodix fingerprint reader at `bDeviceClass ef` /
//! `bInterfaceClass ff` — Miscellaneous over Vendor Specific, the class-code
//! equivalent of a shrug — and it comes out `Unknown` rather than being forced
//! into a bucket.
//!
//! Product-string heuristics — "this string contains *webcam*" — are the
//! remaining half and are deliberately absent. [`KindSource::Heuristic`] exists
//! so the model can already carry one, and so a front end written now renders
//! it correctly when it arrives; nothing produces one yet.

use serde::{Deserialize, Serialize};

use crate::model::UsbDevice;

/// What a device is for.
///
/// Coarser than the USB class list on purpose: this answers "what did the user
/// plug in", not "which specification does this interface implement". The raw
/// class codes stay on [`UsbDevice`] for anyone who wants them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Hub,
    Keyboard,
    Mouse,
    /// A human-interface device that is neither keyboard nor mouse: a tablet, a
    /// game controller, a fingerprint reader that admits to being HID.
    InputDevice,
    Storage,
    Audio,
    /// A webcam or other video-class device.
    Camera,
    /// Still-image class: scanners, and cameras in PTP mode.
    Imaging,
    Printer,
    Network,
    SmartcardReader,
    /// A device whose only job is to announce that an Alternate Mode was not
    /// entered. Never what someone plugged in; always a symptom.
    Billboard,
    /// A wireless radio — Bluetooth, and the class-0xe0 controllers.
    Wireless,
    Diagnostic,
    #[default]
    Unknown,
}

impl DeviceKind {
    /// Every kind, so a caller that must cover them all cannot quietly miss one
    /// a later version adds. `Unknown` is included — it is a kind a device can
    /// be, whatever a particular UI chooses to offer.
    pub const ALL: [DeviceKind; 15] = [
        Self::Hub,
        Self::Keyboard,
        Self::Mouse,
        Self::InputDevice,
        Self::Storage,
        Self::Audio,
        Self::Camera,
        Self::Imaging,
        Self::Printer,
        Self::Network,
        Self::SmartcardReader,
        Self::Billboard,
        Self::Wireless,
        Self::Diagnostic,
        Self::Unknown,
    ];

    /// User-facing name, lower case so it reads inside a sentence.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::InputDevice => "input device",
            Self::Storage => "storage",
            Self::Audio => "audio device",
            Self::Camera => "camera",
            Self::Imaging => "scanner or camera",
            Self::Printer => "printer",
            Self::Network => "network adapter",
            Self::SmartcardReader => "smart-card reader",
            Self::Billboard => "alt-mode billboard",
            Self::Wireless => "wireless radio",
            Self::Diagnostic => "diagnostic device",
            Self::Unknown => "unrecognised device",
        }
    }

    /// The same name in title case, for somewhere it stands on its own rather
    /// than sitting inside a sentence: a dropdown entry, a field's value, a
    /// caption built out of `·` separators.
    ///
    /// Spelled out rather than derived by upper-casing each word, because title
    /// case is not a string operation: *Scanner or Camera* keeps `or` lower,
    /// *Smart-Card Reader* capitalises after the hyphen, and no rule short of a
    /// word list gets both right. The test below keeps the two in step — a title
    /// that stops being the same words as its label fails.
    pub fn title(&self) -> &'static str {
        match self {
            Self::Hub => "Hub",
            Self::Keyboard => "Keyboard",
            Self::Mouse => "Mouse",
            Self::InputDevice => "Input Device",
            Self::Storage => "Storage",
            Self::Audio => "Audio Device",
            Self::Camera => "Camera",
            Self::Imaging => "Scanner or Camera",
            Self::Printer => "Printer",
            Self::Network => "Network Adapter",
            Self::SmartcardReader => "Smart-Card Reader",
            Self::Billboard => "Alt-Mode Billboard",
            Self::Wireless => "Wireless Radio",
            Self::Diagnostic => "Diagnostic Device",
            Self::Unknown => "Unrecognised Device",
        }
    }

    /// The kind a USB class code names, where it names one a person would
    /// recognise.
    ///
    /// `None` for the codes that decline to answer: `0x00` (see the
    /// interfaces), `0xef` (miscellaneous), `0xfe` (application specific) and
    /// `0xff` (vendor specific). Those are not "unknown device" — they are
    /// "ask somewhere else", and a renderer can use the difference to decide
    /// whether printing the raw class alongside the kind adds anything.
    pub fn from_class(code: u8) -> Option<Self> {
        from_class_code(code)
    }

    /// True when the device carries a user-serviceable data payload whose speed
    /// is worth judging.
    ///
    /// A keyboard at 1.5 Mbps is working perfectly; a drive at 480 Mbps may not
    /// be. Rules about throughput and link width should ask this rather than
    /// re-listing classes.
    pub fn speed_matters(&self) -> bool {
        matches!(self, Self::Storage | Self::Camera | Self::Network)
    }
}

/// Where a [`Kind`] came from. A second axis from [`crate::Confidence`], which
/// is about certainty; this is about provenance, and conflating them would hide
/// a stale user override behind an `inferred` badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindSource {
    /// Read off `bDeviceClass` / `bInterfaceClass`. The device asserts it.
    #[default]
    Class,
    /// Pattern-matched from a product string. A guess — see the module docs.
    Heuristic,
    /// The user said so.
    User,
}

impl KindSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Class => "declared by the device",
            Self::Heuristic => "guessed from its name",
            Self::User => "set by you",
        }
    }

    /// May a rule treat a kind from this source as grounds for a new finding?
    ///
    /// The device's own claim, yes. A user's correction, also yes — they are
    /// holding the object, which is better evidence than anything on the wire,
    /// and the case the feature exists for is a user supplying a fact the bus
    /// cannot carry ("that one is a spinning disk").
    ///
    /// A product-string guess, never. The tool invented it, so it may push
    /// toward silence and never toward blame. See the module docs.
    pub fn is_evidence(&self) -> bool {
        matches!(self, Self::Class | Self::User)
    }

    /// The strongest confidence a finding resting on this may claim.
    ///
    /// A declaration is not a measurement however true it is: `Measured` means
    /// read off the hardware. So a user's correction caps at `Inferred`, and a
    /// finding that leans on one has to say where the fact came from.
    pub fn confidence_cap(&self) -> crate::model::Confidence {
        use crate::model::Confidence;
        match self {
            Self::Class => Confidence::Measured,
            Self::User => Confidence::Inferred,
            Self::Heuristic => Confidence::Heuristic,
        }
    }
}

/// A classification, with its provenance attached so the two cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Kind {
    pub kind: DeviceKind,
    pub source: KindSource,
}

impl Kind {
    pub fn class(kind: DeviceKind) -> Self {
        Self {
            kind,
            source: KindSource::Class,
        }
    }

    /// The kind a rule may use as grounds for a **new** finding, or `None`.
    ///
    /// See the module docs: this is the enforcement point for "a guess may
    /// quieten a rule but never start one". A user's declaration passes — with
    /// its confidence capped, see [`Kind::cap`].
    pub fn grounds(&self) -> Option<DeviceKind> {
        (self.source.is_evidence() && self.kind != DeviceKind::Unknown).then_some(self.kind)
    }

    /// `want`, weakened if this kind's provenance cannot support it.
    ///
    /// A rule that selected a device by its kind calls this on the confidence
    /// it was going to claim, so a finding reached via a user's label cannot
    /// come out `Measured`.
    pub fn cap(&self, want: crate::model::Confidence) -> crate::model::Confidence {
        use crate::model::Confidence::*;
        match (self.source.confidence_cap(), want) {
            (Measured, w) => w,
            (Inferred, Measured) => Inferred,
            (Inferred, w) => w,
            (Heuristic, _) => Heuristic,
        }
    }

    /// Is this a definite answer, from any source?
    pub fn is_known(&self) -> bool {
        self.kind != DeviceKind::Unknown
    }

    /// `camera · guessed from its name`
    pub fn describe(&self) -> String {
        format!("{} \u{00b7} {}", self.kind.label(), self.source.label())
    }

    /// `Camera · guessed from its name` — [`Kind::describe`] with the kind as a
    /// value rather than as prose, for a caption under a heading.
    pub fn caption(&self) -> String {
        format!("{} \u{00b7} {}", self.kind.title(), self.source.label())
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Classify a device from its class codes.
///
/// `bDeviceClass` is consulted first but is usually `0x00` ("see the
/// interfaces"), and the three catch-all codes below say just as little, so
/// most devices are decided by their interface list.
pub fn classify(dev: &UsbDevice) -> Kind {
    if dev.is_root_hub {
        return Kind::class(DeviceKind::Hub);
    }
    if let Some(k) = dev.device_class.and_then(from_class_code) {
        return Kind::class(k);
    }
    match interface_kind(dev) {
        Some(k) => Kind::class(k),
        None => Kind::default(),
    }
}

/// The best answer across a composite device's interfaces.
///
/// A composite device is several things at once — a webcam is video plus audio,
/// a headset is audio plus HID — so the interfaces are ranked and the most
/// specific wins rather than whichever happened to be enumerated first.
fn interface_kind(dev: &UsbDevice) -> Option<DeviceKind> {
    dev.interfaces
        .iter()
        .filter_map(|i| {
            let class = i.class?;
            // HID says what it is in the protocol byte, but only for boot-
            // protocol interfaces; a keyboard's second interface reports 0.
            if class == CLASS_HID {
                return Some(match i.protocol {
                    Some(1) => DeviceKind::Keyboard,
                    Some(2) => DeviceKind::Mouse,
                    _ => DeviceKind::InputDevice,
                });
            }
            from_class_code(class)
        })
        .min_by_key(rank)
}

/// Lower sorts first, so this is the tie-break order for a composite device.
///
/// Storage outranks everything but a hub because it is the kind whose readings
/// carry the most diagnostic weight — a slow link matters there and nowhere
/// else. `InputDevice` sits last of the real answers because it is what HID
/// falls back to when the protocol byte says nothing.
fn rank(k: &DeviceKind) -> u8 {
    use DeviceKind::*;
    match k {
        Hub => 0,
        Storage => 1,
        Camera => 2,
        Imaging => 3,
        Printer => 4,
        SmartcardReader => 5,
        Billboard => 6,
        Network => 7,
        Audio => 8,
        Keyboard => 9,
        Mouse => 10,
        Wireless => 11,
        Diagnostic => 12,
        InputDevice => 13,
        Unknown => 14,
    }
}

const CLASS_HID: u8 = 0x03;

/// A USB class code, where it names something a person would recognise.
///
/// `None` for the codes that decline to answer: `0x00` (see the interfaces),
/// `0xef` (miscellaneous), `0xfe` (application specific) and `0xff` (vendor
/// specific). Those are not "unknown device" — they are "ask somewhere else".
fn from_class_code(code: u8) -> Option<DeviceKind> {
    Some(match code {
        0x01 | 0x10 => DeviceKind::Audio,
        0x02 | 0x0a => DeviceKind::Network,
        CLASS_HID => DeviceKind::InputDevice,
        0x06 => DeviceKind::Imaging,
        0x07 => DeviceKind::Printer,
        0x08 => DeviceKind::Storage,
        0x09 => DeviceKind::Hub,
        0x0b => DeviceKind::SmartcardReader,
        0x0e => DeviceKind::Camera,
        0x11 => DeviceKind::Billboard,
        0xdc => DeviceKind::Diagnostic,
        0xe0 => DeviceKind::Wireless,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, UsbInterface};
    use crate::test_support as ts;

    fn iface(class: u8, protocol: Option<u8>) -> UsbInterface {
        UsbInterface {
            sysfs_name: "x:1.0".into(),
            number: Some(0),
            class: Some(class),
            subclass: Some(0),
            protocol,
            driver: None,
            description: None,
        }
    }

    fn with(ifaces: Vec<UsbInterface>) -> UsbDevice {
        let mut d = ts::device("1-1", "2.00", 480.0, Some("usb1"));
        d.interfaces = ifaces;
        d
    }

    /// Two spellings of one name, and nothing but capitals may differ between
    /// them. Catches the drift the duplication invites: a label reworded and its
    /// title left behind, which would show one device two different names
    /// depending on which widget it appeared in.
    #[test]
    fn a_title_is_its_label_with_capitals() {
        for k in DeviceKind::ALL {
            let (label, title) = (k.label(), k.title());
            assert_eq!(
                title.to_lowercase(),
                label,
                "{k:?}: title {title:?} is not the same words as label {label:?}"
            );
            assert!(
                title.starts_with(|c: char| c.is_uppercase()),
                "{k:?}: {title:?} does not start with a capital"
            );
            // Sentence-cased instead of title-cased is the likely slip, so name
            // it: every word carries a capital unless it is a word title case
            // leaves alone.
            for word in title.split(' ') {
                let minor = matches!(word, "or" | "and" | "the" | "a" | "of");
                assert_eq!(
                    !minor,
                    word.starts_with(|c: char| c.is_uppercase()),
                    "{k:?}: {word:?} in {title:?}"
                );
            }
        }
    }

    #[test]
    fn a_root_hub_is_a_hub() {
        let hub = ts::root_hub("usb1", 5000.0);
        assert_eq!(classify(&hub).kind, DeviceKind::Hub);
    }

    #[test]
    fn the_device_class_wins_when_it_says_anything() {
        let mut d = with(vec![iface(0x08, None)]);
        d.device_class = Some(0x09);
        assert_eq!(classify(&d).kind, DeviceKind::Hub);
    }

    /// `0x00` means "see the interfaces", which is not the same as unknown.
    #[test]
    fn a_per_interface_device_class_defers_to_the_interfaces() {
        let mut d = with(vec![iface(0x08, None)]);
        d.device_class = Some(0x00);
        assert_eq!(classify(&d).kind, DeviceKind::Storage);
    }

    /// The real shape of the fingerprint reader on this machine: Miscellaneous
    /// over Vendor Specific, which answers nothing. It must come out Unknown
    /// rather than being forced into a bucket.
    #[test]
    fn miscellaneous_over_vendor_specific_is_a_shrug() {
        let mut d = with(vec![iface(0xff, None)]);
        d.device_class = Some(0xef);
        let k = classify(&d);
        assert_eq!(k.kind, DeviceKind::Unknown);
        assert!(!k.is_known());
        assert_eq!(k.grounds(), None);
    }

    #[test]
    fn hid_reads_its_kind_off_the_protocol_byte() {
        assert_eq!(
            classify(&with(vec![iface(0x03, Some(1))])).kind,
            DeviceKind::Keyboard
        );
        assert_eq!(
            classify(&with(vec![iface(0x03, Some(2))])).kind,
            DeviceKind::Mouse
        );
    }

    /// A non-boot HID interface reports protocol 0, which says nothing beyond
    /// "human interface" — and must not be guessed into a keyboard.
    #[test]
    fn hid_without_a_boot_protocol_stays_generic() {
        assert_eq!(
            classify(&with(vec![iface(0x03, Some(0))])).kind,
            DeviceKind::InputDevice
        );
    }

    /// A webcam is video plus audio plus, often, HID. The answer is camera.
    #[test]
    fn a_composite_device_is_named_by_its_most_specific_interface() {
        let d = with(vec![
            iface(0x01, None),       // audio
            iface(0x0e, None),       // video
            iface(0x03, Some(0)),    // HID controls
        ]);
        assert_eq!(classify(&d).kind, DeviceKind::Camera);
    }

    /// Interface order must not decide the answer.
    #[test]
    fn the_ranking_is_independent_of_enumeration_order() {
        let a = with(vec![iface(0x0e, None), iface(0x01, None)]);
        let b = with(vec![iface(0x01, None), iface(0x0e, None)]);
        assert_eq!(classify(&a).kind, classify(&b).kind);
    }

    #[test]
    fn a_device_with_no_interfaces_is_unknown() {
        assert_eq!(classify(&with(vec![])).kind, DeviceKind::Unknown);
    }

    // --- the suppress-only rule ---

    /// The whole point of the split: only what the device asserted can be
    /// grounds for a new finding.
    #[test]
    fn a_guess_is_never_grounds_for_a_finding() {
        let guessed = Kind {
            kind: DeviceKind::Storage,
            source: KindSource::Heuristic,
        };
        assert_eq!(guessed.grounds(), None);
        assert!(guessed.is_known(), "it is still shown, and may still quieten");
    }

    /// The opposite of the guess rule, and deliberately so: the user is holding
    /// the object, so their correction may sharpen a finding as well as quieten
    /// one. What it cannot do is claim to have been measured.
    #[test]
    fn a_user_override_is_grounds_but_never_measured() {
        let declared = Kind {
            kind: DeviceKind::Storage,
            source: KindSource::User,
        };
        assert_eq!(declared.grounds(), Some(DeviceKind::Storage));
        assert_eq!(declared.cap(Confidence::Measured), Confidence::Inferred);
        // It does not *raise* a weaker claim either.
        assert_eq!(declared.cap(Confidence::Heuristic), Confidence::Heuristic);
    }

    #[test]
    fn a_class_derived_kind_may_claim_a_measurement() {
        let k = Kind::class(DeviceKind::Storage);
        assert_eq!(k.cap(Confidence::Measured), Confidence::Measured);
    }

    /// A guess cannot lend its confidence to anything, in either direction.
    #[test]
    fn a_guess_caps_everything_to_heuristic() {
        let guessed = Kind {
            kind: DeviceKind::Storage,
            source: KindSource::Heuristic,
        };
        assert_eq!(guessed.cap(Confidence::Measured), Confidence::Heuristic);
    }

    #[test]
    fn an_asserted_kind_is_grounds_for_a_finding() {
        let k = Kind::class(DeviceKind::Storage);
        assert_eq!(k.grounds(), Some(DeviceKind::Storage));
    }

    /// Unknown is never grounds for anything, whatever the source says.
    #[test]
    fn unknown_is_not_grounds_even_when_asserted() {
        assert_eq!(Kind::class(DeviceKind::Unknown).grounds(), None);
    }

    #[test]
    fn only_the_kinds_whose_speed_is_worth_judging_say_so() {
        assert!(DeviceKind::Storage.speed_matters());
        assert!(DeviceKind::Camera.speed_matters());
        assert!(!DeviceKind::Keyboard.speed_matters());
        assert!(!DeviceKind::Hub.speed_matters());
        assert!(!DeviceKind::Unknown.speed_matters());
    }

    #[test]
    fn every_kind_has_a_label_and_a_rank() {
        use DeviceKind::*;
        let all = [
            Hub, Keyboard, Mouse, InputDevice, Storage, Audio, Camera, Imaging, Printer,
            Network, SmartcardReader, Billboard, Wireless, Diagnostic, Unknown,
        ];
        let mut ranks: Vec<u8> = all.iter().map(rank).collect();
        ranks.sort_unstable();
        ranks.dedup();
        assert_eq!(ranks.len(), all.len(), "two kinds share a rank");
        assert!(all.iter().all(|k| !k.label().is_empty()));
    }
}
