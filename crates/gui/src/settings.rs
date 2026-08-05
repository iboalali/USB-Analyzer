//! What the viewer remembers about how it should look.
//!
//! # A JSON file, not GSettings
//!
//! GSettings is the idiomatic answer and it is the wrong one here, for a reason
//! that is not a matter of taste: **GLib aborts the process when a schema is
//! missing.** `g_settings_new` on an unregistered id is fatal, not an error to
//! handle. The README documents `cargo run --bin usbdiag-gui` from a build tree
//! as a supported way to run this, and a schema only exists once it has been
//! compiled into a schema directory — so the first setting would have turned
//! every uninstalled build into a crash, and every `git clone` into "install it
//! first". A user-local schema install avoids root but not the compile step or
//! the cache that has to stay in step with it.
//!
//! So: one small JSON file, beside the device labels this project already keeps
//! that way. No schema, no cache, no install step, and a build tree behaves like
//! an install.
//!
//! It is deliberately **not** `devices.json`. That file is a set of assertions
//! about hardware — "this thing is a card reader" — which are worth keeping for
//! years and are meaningful to copy to another machine. A colour scheme is
//! neither. Mixing them would mean one file whose two halves have nothing to do
//! with each other, and a corrupt preference costing somebody their labels.
//!
//! # Nothing here can stop the app opening
//!
//! Every failure resolves to a default. A missing file is the normal first run;
//! an unparseable one is not worth a dialog on startup; and a setting written by
//! a newer version is a *setting from a newer version*, not a broken file. The
//! window opens looking like the system told it to, which is exactly what it did
//! before this module existed.

use std::path::PathBuf;

use relm4::adw;
use serde::{Deserialize, Serialize};

/// Where the colour scheme comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Whatever the desktop asks for, and it changes when the desktop does.
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// In the order the dropdown shows them, which is also the order they are
    /// offered in: following the system first, because it is the default and the
    /// right answer for almost everybody.
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    /// The label in the dropdown.
    pub fn label(&self) -> &'static str {
        match self {
            Theme::System => "Follow the system",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }

    /// What goes in the file. Stable, and separate from [`Theme::label`] so the
    /// wording can be improved without invalidating everyone's settings.
    pub fn slug(&self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    fn from_slug(s: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.slug() == s)
    }

    fn scheme(&self) -> adw::ColorScheme {
        match self {
            // `Default` rather than `PreferLight`: it means "no opinion", which
            // is what following the system is.
            Theme::System => adw::ColorScheme::Default,
            Theme::Light => adw::ColorScheme::ForceLight,
            Theme::Dark => adw::ColorScheme::ForceDark,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, with = "theme_slug")]
    pub theme: Theme,
}

/// A theme is stored as its slug, and an unrecognised one is not an error.
///
/// The whole file would otherwise fail to parse because of one word — so a
/// version that learns a fourth scheme would make this version fall back to
/// defaults for *every* setting rather than just that one.
mod theme_slug {
    use super::Theme;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Theme, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(t.slug())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Theme, D::Error> {
        Ok(Option::<String>::deserialize(d)?
            .as_deref()
            .and_then(Theme::from_slug)
            .unwrap_or_default())
    }
}

impl Settings {
    /// Read them, or return the defaults. Never fails, by design — see the
    /// module note.
    pub fn load() -> Settings {
        default_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// `Err` is worth showing — somebody changed a setting and it did not stick —
    /// but it is the caller's business what to do about it.
    pub fn save(&self) -> std::io::Result<()> {
        let path = default_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no config directory (set $XDG_CONFIG_HOME or $HOME)",
            )
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        text.push('\n');
        std::fs::write(path, text)
    }

    /// Hand the choice to libadwaita, which is the only thing that acts on it.
    ///
    /// Every colour in `style.css` is a libadwaita named colour precisely so this
    /// is all it takes.
    pub fn apply(&self) {
        adw::StyleManager::default().set_color_scheme(self.theme.scheme());
    }
}

/// `$XDG_CONFIG_HOME/usbdiag/gui.json`, or the `$HOME/.config` fallback.
///
/// The same directory `usbdiag labels` writes to, and the same resolution rules —
/// duplicated rather than shared, because reaching into the library for a path
/// helper would make the CLI's config layout part of the GUI's API.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("usbdiag").join("gui.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Settings {
        serde_json::from_str(text).unwrap_or_default()
    }

    #[test]
    fn a_theme_survives_the_file() {
        for t in Theme::ALL {
            let text = serde_json::to_string(&Settings { theme: t }).unwrap();
            assert!(text.contains(t.slug()), "{text}");
            assert_eq!(parse(&text).theme, t);
        }
    }

    /// Three ways a file can disappoint, and all of them open the window.
    #[test]
    fn nothing_a_file_can_say_stops_the_app_opening() {
        // Never written yet.
        assert_eq!(parse("{}"), Settings::default());
        // Truncated, or hand-edited into nonsense.
        assert_eq!(parse("{\"theme\""), Settings::default());
        assert_eq!(parse("not json at all"), Settings::default());
        // Present but empty.
        assert_eq!(parse("{\"theme\": null}"), Settings::default());
    }

    /// A scheme this version has never heard of came from a version that had.
    /// Falling back for that one setting is right; refusing to read the file at
    /// all would discard every *other* setting alongside it.
    #[test]
    fn a_setting_from_a_newer_version_is_not_a_broken_file() {
        let s: Settings = serde_json::from_str(r#"{"theme": "solarized-midnight"}"#)
            .expect("an unknown scheme must still parse");
        assert_eq!(s.theme, Theme::System);
    }

    /// Labels and preferences are different kinds of thing with different
    /// lifetimes, and one must never be able to cost you the other.
    #[test]
    fn preferences_do_not_share_a_file_with_device_labels() {
        // Restored rather than removed. Clearing a variable this machine may
        // legitimately have set would leak out of this test into whatever runs
        // next in the same process.
        let was = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/usbdiag-settings-test");

        let ours = default_path().unwrap();
        assert!(ours.ends_with("usbdiag/gui.json"), "{}", ours.display());
        assert_ne!(
            ours,
            usb_probe::overrides::default_path().unwrap(),
            "a corrupt preference must not be able to cost somebody their labels"
        );

        match was {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
