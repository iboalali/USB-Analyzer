//! The preferences dialog.
//!
//! One setting today, and built as a page rather than as a single control so the
//! second one costs a row instead of a redesign. `adw::PreferencesDialog` is what
//! every GNOME app puts behind Ctrl+comma, so this is the shape people already
//! know how to use — and it is adaptive for free, which matters because this app
//! is meant to be usable in a narrow window while cables are being swapped.
//!
//! No state lives here. The dialog reads what it is given and emits a message per
//! change; [`crate::AppModel`] owns the settings, applies them and saves them.
//! Writing the file from inside a widget callback would put persistence in the
//! one place with no way to report that it failed.

use relm4::adw::{self, prelude::*};
use relm4::gtk;

use crate::settings::{Settings, Theme};
use crate::Msg;

/// Build and present the dialog. Modal to `parent`.
pub fn present(current: &Settings, parent: &impl IsA<gtk::Widget>, sender: &relm4::Sender<Msg>) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_title("Appearance");

    let names = gtk::StringList::new(&[]);
    for t in Theme::ALL {
        names.append(t.label());
    }

    let row = adw::ComboRow::new();
    row.set_title("Colour scheme");
    row.set_subtitle(
        "Every colour in this window is a system colour, so following the system is \
         the default and usually right.",
    );
    row.set_model(Some(&names));
    row.set_selected(
        Theme::ALL
            .iter()
            .position(|t| *t == current.theme)
            .unwrap_or(0) as u32,
    );

    // `selected_notify` also fires while the row is being set up, which would
    // save the file on merely opening the dialog. Connected after
    // `set_selected` for that reason, and it still re-emits for a selection that
    // did not change — the model treats that as a no-op rather than trying to be
    // clever here.
    let s = sender.clone();
    row.connect_selected_notify(move |row| {
        if let Some(theme) = Theme::ALL.get(row.selected() as usize) {
            s.emit(Msg::SetTheme(*theme));
        }
    });

    group.add(&row);
    page.add(&group);
    dialog.add(&page);
    dialog.present(Some(parent));
}
