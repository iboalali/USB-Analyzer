//! What this process is actually allowed to do.
//!
//! Everything the crate does today is passive: read sysfs, read the kernel log,
//! conclude. Some questions cannot be answered that way — a marginal cable
//! usually negotiates full speed and only fails under load, which no amount of
//! reading negotiated state will reveal. Answering those needs privileged
//! interfaces, and two of them change the state of the bus.
//!
//! This module is the gate. It reports what is reachable **before** anything is
//! attempted, so that:
//!
//! * a front end can grey out what it cannot offer rather than failing on use;
//! * a skipped probe can be reported as skipped, with the reason, instead of
//!   being silently omitted — the same policy the kernel-log rules already
//!   follow;
//! * nothing classified [`ProbeClass::Disruptive`] can run by accident.
//!
//! # Detection is by attempt, not by arithmetic
//!
//! Deciding "am I root, therefore I may" is wrong often enough to matter:
//! file ACLs, group membership, capability bits and container policies all
//! override the naive answer in both directions. So where a thing can be tried
//! cheaply and without side effects, it is tried. Opening a usbfs node
//! read-write is exactly what libusb does to talk to a device and by itself
//! affects nothing — it claims no interface and detaches no driver — and the
//! node chosen is a root hub, the least interesting device on any bus.
//!
//! # Four ways to be unavailable
//!
//! They are kept apart because the fix differs completely: load a module, gain
//! a privilege, rebuild a kernel, or nothing at all.

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sysfs as fsx;

/// Whether an interface can be used, and if not, why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Availability {
    /// Present, and this process can use it.
    Usable,
    /// Present, but this process may not use it. Fix: privilege.
    Denied(String),
    /// The kernel supports it but the module is not loaded. Fix: `modprobe`.
    NotLoaded(String),
    /// This kernel was not built with it. Fix: a different kernel.
    Unsupported(String),
    /// Cannot be determined — usually because the directory that would answer
    /// the question is itself unreadable. Fix: unknown, which is the point.
    Unknown(String),
}

impl Availability {
    pub fn is_usable(&self) -> bool {
        matches!(self, Availability::Usable)
    }

    /// One short phrase, suitable for a status line or a finding's evidence.
    pub fn explain(&self) -> &str {
        match self {
            Availability::Usable => "available",
            Availability::Denied(w)
            | Availability::NotLoaded(w)
            | Availability::Unsupported(w)
            | Availability::Unknown(w) => w,
        }
    }
}

/// A privileged kernel interface, and where it was looked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interface {
    pub availability: Availability,
    /// The path that was tested, when one was found at all.
    pub path: Option<PathBuf>,
}

impl Interface {
    fn new(availability: Availability, path: Option<PathBuf>) -> Self {
        Self { availability, path }
    }

    pub fn is_usable(&self) -> bool {
        self.availability.is_usable()
    }
}

/// What this process may do, resolved at capture time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    /// Effective UID, from `/proc/self/status`. Reported for context only —
    /// no decision here is made from it, because being root is neither
    /// necessary nor sufficient for any particular access.
    pub effective_uid: Option<u32>,
    /// The kernel's URB tracer: every transfer, with its completion status.
    /// Read-only, and the single richest source of evidence about a link.
    pub usbmon: Interface,
    /// usbfs device nodes — the only way to issue transfers from userspace,
    /// and therefore the only way to load a link deliberately.
    pub usbfs: Interface,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            effective_uid: None,
            usbmon: Interface::new(Availability::Unknown("not probed".into()), None),
            usbfs: Interface::new(Availability::Unknown("not probed".into()), None),
        }
    }
}

impl Capabilities {
    pub fn is_root(&self) -> bool {
        self.effective_uid == Some(0)
    }

    /// Why this probe cannot run now, or `None` if it can.
    pub fn blocker(&self, probe: &ProbeInfo) -> Option<String> {
        let iface = match probe.needs {
            Requirement::Nothing => return None,
            Requirement::Usbmon => &self.usbmon,
            Requirement::Usbfs => &self.usbfs,
        };
        if iface.is_usable() {
            return None;
        }
        Some(format!(
            "{} needs {}: {}",
            probe.name,
            probe.needs.label(),
            iface.availability.explain()
        ))
    }

    /// Everything this machine's interfaces allow, so a front end can offer
    /// exactly that.
    ///
    /// Capability only — consent, targeting and whether the probe is written
    /// yet are all decided later, by [`crate::probe::plan`].
    pub fn runnable(&self) -> Vec<&'static ProbeInfo> {
        PROBES
            .iter()
            .filter(|p| self.blocker(p).is_none())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The probe registry
// ---------------------------------------------------------------------------

/// What a probe does to the system. The distinction that decides whether it may
/// run without being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeClass {
    /// Reads only, needs no privilege. Runs by default.
    Passive,
    /// Needs privilege, but changes nothing on the bus. Safe to run whenever it
    /// is available — it is only the privilege that is in question.
    PrivilegedRead,
    /// Takes a device away from its driver or off the bus. Never runs without
    /// explicit consent, and never on a device backing a mounted filesystem.
    Disruptive,
}

impl ProbeClass {
    pub fn label(&self) -> &'static str {
        match self {
            ProbeClass::Passive => "passive",
            ProbeClass::PrivilegedRead => "privileged, read-only",
            ProbeClass::Disruptive => "disruptive",
        }
    }

    /// May this run without the user asking for it by name?
    pub fn runs_by_default(&self) -> bool {
        matches!(self, ProbeClass::Passive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    Nothing,
    Usbmon,
    Usbfs,
}

impl Requirement {
    pub fn label(&self) -> &'static str {
        match self {
            Requirement::Nothing => "nothing",
            Requirement::Usbmon => "usbmon",
            Requirement::Usbfs => "usbfs write access",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProbeInfo {
    pub name: &'static str,
    pub class: ProbeClass,
    pub needs: Requirement,
    /// Whether the code behind it exists yet. A registered but unimplemented
    /// probe is listed and then refused by name, which is the honest answer —
    /// far better than omitting it and letting the gap go unnoticed.
    pub implemented: bool,
    pub summary: &'static str,
}

/// Every probe the crate knows about, implemented or not.
///
/// Listed here rather than discovered, so that a front end can show what the
/// tool *could* do on better-privileged hardware, and so an unimplemented probe
/// is a visible gap rather than an absence nobody notices.
pub const PROBES: &[ProbeInfo] = &[
    ProbeInfo {
        name: "snapshot",
        class: ProbeClass::Passive,
        needs: Requirement::Nothing,
        implemented: true,
        summary: "Read sysfs, the kernel log and DRM. Everything the default run does.",
    },
    ProbeInfo {
        name: "storage-sample",
        class: ProbeClass::Passive,
        needs: Requirement::Nothing,
        implemented: true,
        summary: "Two reads of /sys/block/*/stat over a window, for live throughput.",
    },
    ProbeInfo {
        name: "urb-errors",
        class: ProbeClass::PrivilegedRead,
        needs: Requirement::Usbmon,
        implemented: true,
        summary: "Count URB completion errors per device over a window. Observes \
                  behaviour rather than negotiated state, so it is the one probe that \
                  can move a cable finding from inferred to measured.",
    },
    ProbeInfo {
        name: "throughput",
        class: ProbeClass::Disruptive,
        needs: Requirement::Usbfs,
        implemented: false,
        summary: "Drive bulk transfers and compare achieved throughput against what \
                  both the link rate and the storage medium allow.",
    },
    ProbeInfo {
        name: "reenumerate",
        class: ProbeClass::Disruptive,
        needs: Requirement::Usbfs,
        implemented: false,
        summary: "Cycle the port repeatedly and record the distribution of negotiated \
                  speeds. The only way to catch an intermittent link.",
    },
];

pub fn probe(name: &str) -> Option<&'static ProbeInfo> {
    PROBES.iter().find(|p| p.name == name)
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub fn detect() -> Capabilities {
    detect_in(Path::new("/"))
}

/// As [`detect`], against an alternate filesystem root so it can be tested.
pub fn detect_in(root: &Path) -> Capabilities {
    Capabilities {
        effective_uid: read_effective_uid(root),
        usbmon: detect_usbmon(root),
        usbfs: detect_usbfs(root),
    }
}

/// `Uid:  <real> <effective> <saved> <fs>` in `/proc/self/status`.
///
/// Read rather than asked for, because `geteuid(2)` would mean a libc
/// dependency for one integer.
fn read_effective_uid(root: &Path) -> Option<u32> {
    fsx::read_str(root.join("proc/self/status"))?
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// usbmon exposes two APIs: binary character devices `/dev/usbmon<N>`, and a
/// text stream under debugfs.
///
/// Only the text stream counts as usable here. The binary API needs ioctls,
/// which would mean a libc dependency, so nothing in this crate can read it —
/// reporting it as available would be a promise the crate cannot keep. A
/// binary node is still worth finding, because its existence proves the module
/// is loaded and narrows down why the text stream is missing.
fn detect_usbmon(root: &Path) -> Interface {
    let text = root.join(TEXT_API.trim_start_matches('/'));

    // `EACCES` here almost never comes from the file. `/sys/kernel/debug` is
    // mode 0700, so an unprivileged open fails while *traversing the
    // directory*, long before the leaf is reached — and the error is identical
    // whether or not the leaf exists. Saying "it exists but you may not read
    // it" would be a claim this process has no way to check.
    let blocked = match OpenOptions::new().read(true).open(&text) {
        Ok(_) => return Interface::new(Availability::Usable, Some(text)),
        Err(e) => e.kind() == ErrorKind::PermissionDenied,
    };

    // What can be checked: whether the module is there at all. `/proc/modules`
    // is world-readable, and so is the existence of `/dev/usbmon*`, even though
    // the nodes themselves are not.
    let loaded = module_loaded(root, "usbmon") || !binary_nodes(root).is_empty();
    let configured = kernel_config_value(root, "CONFIG_USB_MON");

    match (loaded, blocked) {
        // Present, and something is in the way. The one unambiguous case.
        (true, true) => Interface::new(
            Availability::Denied(format!(
                "usbmon is loaded, but {} cannot be opened by this process — debugfs is \
                 mode 0700, so reading the URB stream needs root",
                text.display()
            )),
            Some(text),
        ),
        // Loaded, reachable, and still not there: debugfs is not mounted.
        (true, false) => Interface::new(
            Availability::Unknown(
                "the usbmon module is loaded but its text stream was not found — debugfs \
                 may not be mounted at /sys/kernel/debug"
                    .into(),
            ),
            None,
        ),
        // Not loaded. Whether that is fixable depends on the kernel build, and
        // being blocked from debugfs does not change the answer.
        (false, _) => match configured {
            Some('m') => Interface::new(
                Availability::NotLoaded(format!(
                    "the kernel supports usbmon but the module is not loaded — \
                     'sudo modprobe usbmon' loads it{}",
                    if blocked {
                        ", though reading its stream will still need root"
                    } else {
                        ""
                    }
                )),
                None,
            ),
            // Built in, so "not loaded" is not a state it can be in; the only
            // remaining explanations are about visibility.
            Some('y') if blocked => Interface::new(
                Availability::Denied(
                    "usbmon is built into this kernel, but /sys/kernel/debug is not \
                     readable by this process — reading the URB stream needs root"
                        .into(),
                ),
                None,
            ),
            Some('y') => Interface::new(
                Availability::Unknown(
                    "usbmon is built into this kernel but no text stream was found — \
                     debugfs may not be mounted"
                        .into(),
                ),
                None,
            ),
            Some(_) => Interface::new(
                Availability::Unknown(
                    "the kernel configuration reports usbmon in a form this tool does not \
                     recognise"
                        .into(),
                ),
                None,
            ),
            // No config to read. Being blocked means we genuinely cannot tell;
            // being able to look and finding nothing means it is not there.
            None if blocked => Interface::new(
                Availability::Unknown(
                    "/sys/kernel/debug is not readable and the kernel configuration could \
                     not be checked, so usbmon support is undetermined without privilege"
                        .into(),
                ),
                None,
            ),
            None => Interface::new(
                Availability::Unsupported(
                    "this kernel was not built with CONFIG_USB_MON, so URB tracing is \
                     unavailable at any privilege level"
                        .into(),
                ),
                None,
            ),
        },
    }
}

/// The all-buses text stream. Per-bus streams are `<busnum>u`.
const TEXT_API: &str = "/sys/kernel/debug/usb/usbmon/0u";

/// `/dev/usbmon<N>` — the binary API. Not readable by this crate, but its
/// presence is proof the module is loaded.
fn binary_nodes(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fsx::list_dir(root.join("dev"))
        .into_iter()
        .filter(|e| fsx::file_name(e).starts_with("usbmon"))
        .collect();
    out.sort();
    out
}

/// Whether a usbfs node can be opened for writing — the access every active
/// probe needs, and the one that is root-only on a stock system.
///
/// A root hub is chosen deliberately: opening a usbfs node has no effect on the
/// device, but if that ever changed, a root hub is the safest thing to have
/// touched.
fn detect_usbfs(root: &Path) -> Interface {
    let Some(node) = usbfs_probe_node(root) else {
        return Interface::new(
            Availability::Unsupported(
                "no usbfs device nodes under /dev/bus/usb — usbfs is not mounted, so \
                 userspace cannot issue transfers"
                    .into(),
            ),
            None,
        );
    };

    match OpenOptions::new().read(true).write(true).open(&node) {
        Ok(_) => Interface::new(Availability::Usable, Some(node)),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Interface::new(
            Availability::Denied(format!(
                "{} is not writable by this process — usbfs nodes are root-only on a \
                 stock system, and group membership does not change that",
                node.display()
            )),
            Some(node),
        ),
        Err(e) => Interface::new(
            Availability::Denied(format!("{}: {e}", node.display())),
            Some(node),
        ),
    }
}

/// The first root hub node: `/dev/bus/usb/<bus>/001`.
fn usbfs_probe_node(root: &Path) -> Option<PathBuf> {
    let mut buses = fsx::list_dir(root.join("dev/bus/usb"));
    buses.sort();
    buses
        .into_iter()
        .map(|b| b.join("001"))
        .find(|n| n.exists())
}

fn module_loaded(root: &Path, name: &str) -> bool {
    fsx::read_str(root.join("proc/modules")).is_some_and(|m| {
        m.lines()
            .filter_map(|l| l.split_whitespace().next())
            .any(|m| m == name)
    })
}

/// The value of a kernel config option: `y`, `m`, or `None` when unset or when
/// `/boot/config-<release>` cannot be read.
fn kernel_config_value(root: &Path, key: &str) -> Option<char> {
    let release = fsx::read_str(root.join("proc/sys/kernel/osrelease"))?;
    let config = fsx::read_str(root.join(format!("boot/config-{release}")))?;
    config
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))?
        .chars()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("usbprobe-caps-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn fake_root(tag: &str, config: Option<&str>, modules: &str) -> PathBuf {
        let root = scratch(tag);
        fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        fs::create_dir_all(root.join("proc/self")).unwrap();
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::create_dir_all(root.join("dev/bus/usb/001")).unwrap();
        fs::write(
            root.join("proc/self/status"),
            "Name:\tusbdiag\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n",
        )
        .unwrap();
        fs::write(root.join("proc/sys/kernel/osrelease"), "6.17.0-test\n").unwrap();
        fs::write(root.join("proc/modules"), modules).unwrap();
        if let Some(c) = config {
            fs::write(root.join("boot/config-6.17.0-test"), c).unwrap();
        }
        root
    }

    #[test]
    fn reads_the_effective_uid_not_the_real_one() {
        let root = scratch("uid");
        fs::create_dir_all(root.join("proc/self")).unwrap();
        // Real 1000, effective 0 — what a setuid binary looks like. Taking the
        // first field would get this exactly backwards.
        fs::write(
            root.join("proc/self/status"),
            "Uid:\t1000\t0\t0\t1000\nGid:\t1000\t1000\t1000\t1000\n",
        )
        .unwrap();
        assert_eq!(read_effective_uid(&root), Some(0));
        let _ = fs::remove_dir_all(&root);
    }

    /// The state of the machine this was written on: usbmon is a module, and it
    /// is not loaded. The fix is one command, so the answer must say so rather
    /// than reporting the interface as missing.
    #[test]
    fn a_module_that_is_not_loaded_is_not_the_same_as_unsupported() {
        let root = fake_root("notloaded", Some("CONFIG_USB_MON=m\n"), "ext4 1000000 0 - Live\n");
        let caps = detect_in(&root);
        assert!(
            matches!(caps.usbmon.availability, Availability::NotLoaded(_)),
            "{:?}",
            caps.usbmon
        );
        assert!(caps.usbmon.availability.explain().contains("modprobe"));
        assert!(!caps.is_root());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_kernel_without_usbmon_is_reported_as_unsupported() {
        let root = fake_root("unsup", Some("CONFIG_DEBUG_FS=y\n"), "");
        let caps = detect_in(&root);
        assert!(
            matches!(caps.usbmon.availability, Availability::Unsupported(_)),
            "{:?}",
            caps.usbmon
        );
        // No amount of privilege fixes this, and the text must not suggest it.
        assert!(caps.usbmon.availability.explain().contains("any privilege"));
        let _ = fs::remove_dir_all(&root);
    }

    /// A readable text stream is the whole point, and it must win over any
    /// inference from the kernel config.
    #[test]
    fn an_openable_interface_is_usable_whatever_the_config_says() {
        let root = fake_root("usable", None, "");
        let text = root.join("sys/kernel/debug/usb/usbmon");
        fs::create_dir_all(&text).unwrap();
        fs::write(text.join("0u"), b"").unwrap();

        let caps = detect_in(&root);
        assert_eq!(caps.usbmon.availability, Availability::Usable);
        assert_eq!(caps.usbmon.path, Some(text.join("0u")));
        let _ = fs::remove_dir_all(&root);
    }

    /// Caught on the live machine, which is the only place it could have been.
    ///
    /// `/sys/kernel/debug` is mode 0700, so an unprivileged open of anything
    /// beneath it fails while traversing the directory — with the same
    /// `EACCES` whether or not the file is there. An earlier version read that
    /// as proof the stream existed, which meant it would have claimed usbmon
    /// was present on a kernel that had never heard of it.
    #[test]
    fn permission_denied_on_debugfs_is_not_proof_the_stream_exists() {
        let root = fake_root("blocked", Some("CONFIG_USB_MON=m\n"), "");
        let debug = root.join("sys/kernel/debug");
        fs::create_dir_all(&debug).unwrap();
        let mut perms = fs::metadata(&debug).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        fs::set_permissions(&debug, perms).unwrap();

        // Root traverses a 0000 directory regardless, so there is nothing to
        // assert if the suite happens to be running privileged.
        if read_effective_uid(Path::new("/")) == Some(0) {
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let caps = detect_in(&root);
        // The module is not loaded, so that is the answer — not "denied", and
        // certainly not a claim about a file nobody could see.
        assert!(
            matches!(caps.usbmon.availability, Availability::NotLoaded(_)),
            "{:?}",
            caps.usbmon
        );
        let why = caps.usbmon.availability.explain();
        assert!(why.contains("modprobe"), "{why}");
        assert!(why.contains("still need root"), "both barriers named: {why}");
        assert!(caps.usbmon.path.is_none(), "no path was ever confirmed");

        // With the module loaded, the same EACCES now means something definite.
        fs::write(root.join("dev/usbmon0"), b"").unwrap();
        let caps = detect_in(&root);
        assert!(
            matches!(caps.usbmon.availability, Availability::Denied(_)),
            "{:?}",
            caps.usbmon
        );
        assert!(caps.usbmon.availability.explain().contains("needs root"));

        let mut perms = fs::metadata(&debug).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        let _ = fs::set_permissions(&debug, perms);
        let _ = fs::remove_dir_all(&root);
    }

    /// The binary API proves the module is loaded but cannot be read by this
    /// crate, so it must never be reported as usable — only used to sharpen
    /// the explanation for why the text stream is not there.
    #[test]
    fn a_binary_node_alone_is_not_usable() {
        let root = fake_root("binonly", Some("CONFIG_USB_MON=m\n"), "");
        fs::write(root.join("dev/usbmon0"), b"").unwrap();

        let caps = detect_in(&root);
        assert!(!caps.usbmon.is_usable());
        assert!(
            matches!(caps.usbmon.availability, Availability::Unknown(_)),
            "loaded but no text stream: {:?}",
            caps.usbmon
        );
        assert!(caps.usbmon.availability.explain().contains("debugfs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_usbfs_nodes_are_unsupported_not_denied() {
        let root = fake_root("nousbfs", Some("CONFIG_USB_MON=m\n"), "");
        let caps = detect_in(&root);
        assert!(
            matches!(caps.usbfs.availability, Availability::Unsupported(_)),
            "{:?}",
            caps.usbfs
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A writable node means the probes that need it can run — which on a real
    /// system means root, and in this test means an ordinary file.
    #[test]
    fn a_writable_usbfs_node_unlocks_the_disruptive_probes() {
        let root = fake_root("usbfs", None, "");
        fs::write(root.join("dev/bus/usb/001/001"), b"").unwrap();

        let caps = detect_in(&root);
        assert_eq!(caps.usbfs.availability, Availability::Usable);

        let names: Vec<&str> = caps.runnable().iter().map(|p| p.name).collect();
        assert!(names.contains(&"throughput"), "{names:?}");
        assert!(names.contains(&"reenumerate"), "{names:?}");
        // usbmon is still absent, so its probe stays blocked.
        assert!(!names.contains(&"urb-errors"), "{names:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// Passive probes must never be gated on anything.
    #[test]
    fn passive_probes_run_with_no_capabilities_at_all() {
        let caps = Capabilities::default();
        let names: Vec<&str> = caps.runnable().iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["snapshot", "storage-sample"]);

        for p in PROBES {
            assert_eq!(
                p.class.runs_by_default(),
                p.class == ProbeClass::Passive,
                "{} must not run by default",
                p.name
            );
            // Anything that needs an interface must be classified beyond passive.
            if p.needs != Requirement::Nothing {
                assert_ne!(p.class, ProbeClass::Passive, "{}", p.name);
            }
        }
    }

    #[test]
    fn a_blocker_names_the_probe_the_interface_and_the_reason() {
        let caps = Capabilities {
            effective_uid: Some(1000),
            usbmon: Interface::new(Availability::NotLoaded("module not loaded".into()), None),
            usbfs: Interface::new(Availability::Usable, None),
        };
        let why = caps.blocker(probe("urb-errors").unwrap()).unwrap();
        assert!(why.contains("urb-errors") && why.contains("usbmon"), "{why}");
        assert!(why.contains("module not loaded"), "{why}");
        assert!(caps.blocker(probe("throughput").unwrap()).is_none());
        assert!(caps.blocker(probe("snapshot").unwrap()).is_none());
    }

    /// Detection against the real machine must not panic, whatever it finds.
    #[test]
    fn detecting_on_the_host_does_not_panic() {
        let caps = detect();
        assert!(caps.effective_uid.is_some(), "/proc/self/status is readable");
        // Unprivileged CI and unprivileged desktops alike: the passive probes
        // are always available and nothing else may be assumed.
        assert!(!caps.runnable().is_empty());
    }
}
