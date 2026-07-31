//! Small read helpers for sysfs.
//!
//! Every function returns `Option` and never panics. A missing attribute, a
//! permission error, and an unparseable value are all just `None` — sysfs is
//! full of all three depending on kernel version and driver.

use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{RoleField, Vdo};

/// Read a sysfs attribute and trim it. Empty reads become `None`.
pub fn read_str(path: impl AsRef<Path>) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Read an attribute from a directory: `read_attr(dir, "speed")`.
pub fn read_attr(dir: impl AsRef<Path>, name: &str) -> Option<String> {
    read_str(dir.as_ref().join(name))
}

pub fn read_u32(dir: impl AsRef<Path>, name: &str) -> Option<u32> {
    read_attr(dir, name)?.parse().ok()
}

pub fn read_u8(dir: impl AsRef<Path>, name: &str) -> Option<u8> {
    read_attr(dir, name)?.parse().ok()
}

pub fn read_i64(dir: impl AsRef<Path>, name: &str) -> Option<i64> {
    read_attr(dir, name)?.parse().ok()
}

pub fn read_u64(dir: impl AsRef<Path>, name: &str) -> Option<u64> {
    read_attr(dir, name)?.parse().ok()
}

/// Hex without a `0x` prefix, as USB descriptors are exposed (`idVendor`).
pub fn read_hex_u16(dir: impl AsRef<Path>, name: &str) -> Option<u16> {
    u16::from_str_radix(read_attr(dir, name)?.trim_start_matches("0x"), 16).ok()
}

pub fn read_hex_u8(dir: impl AsRef<Path>, name: &str) -> Option<u8> {
    u8::from_str_radix(read_attr(dir, name)?.trim_start_matches("0x"), 16).ok()
}

/// Hex with an optional `0x` prefix, as Type-C VDOs are exposed.
pub fn read_vdo(dir: impl AsRef<Path>, name: &str) -> Option<Vdo> {
    let raw = read_attr(dir, name)?;
    let t = raw.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(t, 16).ok().map(Vdo::new)
}

/// `yes` / `no`, used by Type-C attributes.
pub fn read_yesno(dir: impl AsRef<Path>, name: &str) -> Option<bool> {
    match read_attr(dir, name)?.as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// `1` / `0`, used by PD flag attributes and `authorized`.
pub fn read_flag(dir: impl AsRef<Path>, name: &str) -> Option<bool> {
    match read_attr(dir, name)?.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Boolean attributes that may be spelled either way.
///
/// The Type-C ABI is inconsistent: `vconn_source` and alt-mode `active` use
/// `yes`/`no`, while PD flag attributes use `1`/`0`. Some attributes have also
/// changed spelling across kernel versions, so accept both rather than silently
/// returning `None` and disabling whatever rule depends on the value.
pub fn read_bool(dir: impl AsRef<Path>, name: &str) -> Option<bool> {
    match read_attr(dir, name)?.as_str() {
        "yes" | "1" | "true" | "enabled" => Some(true),
        "no" | "0" | "false" | "disabled" => Some(false),
        _ => None,
    }
}

/// A value carrying a unit suffix, e.g. `3000mA` or `5000mV` -> `3000`.
pub fn read_suffixed(dir: impl AsRef<Path>, name: &str, suffix: &str) -> Option<u32> {
    let v = read_attr(dir, name)?;
    v.trim_end_matches(suffix).trim().parse().ok()
}

/// A `host [device]`-style multi-value field.
pub fn read_role(dir: impl AsRef<Path>, name: &str) -> Option<RoleField> {
    read_attr(dir, name).map(|r| RoleField::parse(&r))
}

/// Float attributes: `speed` (1.5, 480, 10000) and `version` (" 3.20").
pub fn read_f64(dir: impl AsRef<Path>, name: &str) -> Option<f64> {
    read_attr(dir, name)?.parse().ok()
}

pub fn read_f32(dir: impl AsRef<Path>, name: &str) -> Option<f32> {
    read_attr(dir, name)?.parse().ok()
}

/// Directory entries, sorted by name so output is deterministic.
/// Returns an empty vec rather than an error when the directory is absent.
pub fn list_dir(path: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(path) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(_) => return Vec::new(),
    };
    out.sort();
    out
}

/// Subdirectory names (following symlinks, as sysfs class dirs are symlink farms).
pub fn list_subdirs(path: impl AsRef<Path>) -> Vec<PathBuf> {
    list_dir(path)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Basename of a symlink target, used to name a bound driver.
pub fn read_link_name(dir: impl AsRef<Path>, name: &str) -> Option<String> {
    let target = fs::read_link(dir.as_ref().join(name)).ok()?;
    target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
}

/// Resolve a symlink to its real device path.
pub fn canonicalize(path: impl AsRef<Path>) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `supports_usb_power_delivery` reads `no`, not `0`. Reading it
    /// with a 0/1-only parser returned None on real hardware and silently
    /// disabled every rule that depended on it.
    #[test]
    fn read_bool_accepts_both_spellings() {
        let dir = std::env::temp_dir().join(format!("usbprobe-bool-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("yesno"), "no\n").unwrap();
        fs::write(dir.join("flag"), "1\n").unwrap();
        fs::write(dir.join("junk"), "maybe\n").unwrap();

        assert_eq!(read_bool(&dir, "yesno"), Some(false));
        assert_eq!(read_bool(&dir, "flag"), Some(true));
        assert_eq!(read_bool(&dir, "junk"), None);
        assert_eq!(read_bool(&dir, "absent"), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_field_marks_current_value() {
        let r = RoleField::parse("host [device]");
        assert_eq!(r.current.as_deref(), Some("device"));
        assert_eq!(r.supported, vec!["host", "device"]);
    }

    #[test]
    fn role_field_with_single_value_is_current() {
        let r = RoleField::parse("default");
        assert_eq!(r.current.as_deref(), Some("default"));
    }

    #[test]
    fn role_field_handles_usb_type_style() {
        // /sys/class/power_supply/*/usb_type
        let r = RoleField::parse("[C] PD PD_PPS");
        assert_eq!(r.current.as_deref(), Some("C"));
        assert!(r.supported.iter().any(|s| s == "PD_PPS"));
    }

    #[test]
    fn role_field_with_no_bracket_and_many_values_has_no_current() {
        let r = RoleField::parse("usb2 usb3");
        assert!(r.current.is_none());
        assert_eq!(r.supported.len(), 2);
    }
}
