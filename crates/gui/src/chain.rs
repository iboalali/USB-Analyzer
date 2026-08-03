//! The bottleneck chain, drawn with cairo.
//!
//! Boxes and labels cannot do proportional bars, connecting arrows and the
//! "▲ the limit" marker without fighting the layout engine, so this is a
//! `gtk::DrawingArea` — the same answer TempoUI's `timeline.rs` reached, and
//! for the same reason.
//!
//! **No diagnosis happens here.** Which stage carries the marker comes from
//! [`usb_probe::chain`], which reads it off the findings. This file knows how
//! to draw a `Chain`; it does not know what a cable is.
//!
//! Two shapes, one widget. Above ~560 px the four stages sit side by side; below
//! it they transpose into four labelled rows rather than shrinking, because four
//! columns stop being legible long before they stop fitting. The switch is made
//! from the allocated width inside the draw function, so it needs no breakpoint
//! plumbing and works when the pane is resized rather than only when the window
//! crosses a threshold.

use relm4::gtk::cairo::{Context, FontSlant, FontWeight};
use relm4::gtk::{self, glib, prelude::*};
use usb_probe::chain::Chain;

/// Below this width the chain transposes to rows.
const WIDE_AT: i32 = 560;

/// Column layout, measured from the draw code below: cap baseline 14, bar at
/// 26, value baseline 52, then `SUB_LINES` caption lines of 14, then the
/// marker line when there is one. Derived rather than guessed, because a
/// constant that is merely generous leaves a band of dead space under the
/// bars that reads as a layout bug.
const SUB_LINES: i32 = 2;
const COL_BASE_H: i32 = 14 + 12 + 26 + 18;
const LINE_H: i32 = 14;
const MARK_H: i32 = 18;

const ROW_H: i32 = 46;
const ROWS_PAD: i32 = 10;

/// Colours, keyed on the current colour scheme.
///
/// These duplicate values that also live in `style.css`. Cairo has no access to
/// CSS, and the non-deprecated ways to ask GTK for a named colour do not exist
/// in the 4.14 baseline, so the palette is written twice; both copies are the
/// libadwaita 1.5 palette and are commented as such in each file.
#[derive(Clone, Copy)]
struct Palette {
    fg: (f64, f64, f64, f64),
    dim: (f64, f64, f64, f64),
    dimmer: (f64, f64, f64, f64),
    track: (f64, f64, f64, f64),
    fill: (f64, f64, f64, f64),
    marked: (f64, f64, f64, f64),
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            fg: (1.0, 1.0, 1.0, 1.0),
            dim: (1.0, 1.0, 1.0, 0.7),
            dimmer: (1.0, 1.0, 1.0, 0.45),
            track: (1.0, 1.0, 1.0, 0.12),
            fill: (0.208, 0.518, 0.894, 1.0),  // #3584e4
            marked: (0.753, 0.110, 0.157, 1.0), // #c01c28
        }
    } else {
        Palette {
            fg: (0.0, 0.0, 0.024, 0.8),
            dim: (0.0, 0.0, 0.024, 0.55),
            dimmer: (0.0, 0.0, 0.024, 0.38),
            track: (0.0, 0.0, 0.024, 0.1),
            fill: (0.208, 0.518, 0.894, 1.0),  // #3584e4
            marked: (0.878, 0.106, 0.141, 1.0), // #e01b24
        }
    }
}

fn set(cr: &Context, c: (f64, f64, f64, f64)) {
    cr.set_source_rgba(c.0, c.1, c.2, c.3);
}

/// A `DrawingArea` that draws `chain` and re-lays itself out on resize.
pub fn area(chain: Chain) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("chain");
    area.set_hexpand(true);

    let n = chain.stages.len() as i32;
    let marked = chain.limited_by.is_some();
    let wide_h = COL_BASE_H + SUB_LINES * LINE_H + if marked { MARK_H } else { 4 };
    let narrow_h = n * ROW_H + 2 * ROWS_PAD;
    area.set_content_height(wide_h);

    area.connect_resize(move |a, w, _h| {
        let want = if w >= WIDE_AT { wide_h } else { narrow_h };
        if a.content_height() == want {
            return;
        }
        // Deferred to idle on purpose. `::resize` is emitted from inside
        // `size_allocate`, and changing a size request there queues a resize
        // during allocation — GTK drops it, and the widget keeps the height of
        // the layout it is no longer using. That is how the transposed chain
        // ended up drawing four rows into the height of two.
        let weak = a.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(a) = weak.upgrade() {
                a.set_content_height(want);
            }
        });
    });

    area.set_draw_func(move |a, cr, w, h| {
        let dark = relm4::adw::StyleManager::default().is_dark();
        let p = palette(dark);
        // Text follows the widget's own CSS colour where it can; the semantic
        // colours below cannot be read that way and come from the palette.
        let fg = a.color();
        let p = Palette {
            fg: (
                fg.red() as f64,
                fg.green() as f64,
                fg.blue() as f64,
                fg.alpha() as f64,
            ),
            ..p
        };
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        if w >= WIDE_AT {
            draw_columns(cr, &chain, &p, w as f64, h as f64);
        } else {
            draw_rows(cr, &chain, &p, w as f64);
        }
    });
    area
}

// ---------------------------------------------------------------------------
// wide: four stages side by side
// ---------------------------------------------------------------------------

fn draw_columns(cr: &Context, chain: &Chain, p: &Palette, w: f64, _h: f64) {
    let n = chain.stages.len() as f64;
    let gap = 26.0;
    let col = ((w - gap * (n - 1.0)) / n).max(40.0);
    let max = chain.max();

    for (i, st) in chain.stages.iter().enumerate() {
        let x = i as f64 * (col + gap);
        let mut y = 14.0;

        set(cr, p.dimmer);
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        cr.set_font_size(11.0);
        show(cr, x, y, &fit(cr, &st.cap.to_uppercase(), col));

        y += 12.0;
        bar(cr, p, x, y, col, st.fraction(max), st.marked_by.is_some());

        y += 26.0;
        set(cr, if st.marked_by.is_some() { p.marked } else { p.fg });
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Bold);
        cr.set_font_size(17.0);
        show(cr, x, y, &fit(cr, &st.value, col));

        y += 18.0;
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        cr.set_font_size(11.5);
        set(cr, p.dim);
        for line in wrap(cr, &st.sub, col, SUB_LINES as usize) {
            show(cr, x, y, &line);
            y += 14.0;
        }

        if let Some(code) = &st.marked_by {
            set(cr, p.marked);
            cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Bold);
            cr.set_font_size(11.5);
            let base = y + 2.0;
            let ind = marker(cr, x, base, 11.5);
            let text = fit(cr, &format!("the limit \u{00b7} {code}"), col - ind);
            show(cr, x + ind, base, &text);
        }

        // Arrow into the next stage, on the bar's centre line.
        if i + 1 < chain.stages.len() {
            set(cr, p.dimmer);
            arrow(cr, x + col + 6.0, 26.0 + 5.0, gap - 12.0);
        }
    }
}

/// The "this is the limit" triangle, filled as a path and returning the width
/// the caller should indent its text by.
///
/// **Not a glyph, on purpose.** This was `\u{25b2}` and rendered as a
/// missing-glyph box. Text here goes through cairo's *toy* API
/// (`select_font_face` + `show_text`), which uses one font face and does no
/// fallback of its own — and `Sans` resolves to Noto Sans, which has no
/// Geometric Shapes block. Pango would have substituted another font; cairo
/// draws the box. The between-stage [`arrow`] was always a path, which is
/// exactly why it never had this problem.
///
/// `baseline` is the text baseline the triangle rests on, so it lines up with
/// the label beside it however the font metrics fall.
fn marker(cr: &Context, x: f64, baseline: f64, size: f64) -> f64 {
    let w = size * 0.72;
    let h = size * 0.62;
    cr.move_to(x + w / 2.0, baseline - h);
    cr.line_to(x + w, baseline);
    cr.line_to(x, baseline);
    cr.close_path();
    let _ = cr.fill();
    w + size * 0.3
}

fn arrow(cr: &Context, x: f64, y: f64, len: f64) {
    cr.set_line_width(1.4);
    cr.move_to(x, y);
    cr.line_to(x + len - 4.0, y);
    let _ = cr.stroke();
    cr.move_to(x + len, y);
    cr.line_to(x + len - 5.0, y - 3.5);
    cr.line_to(x + len - 5.0, y + 3.5);
    cr.close_path();
    let _ = cr.fill();
}

// ---------------------------------------------------------------------------
// narrow: four labelled rows
// ---------------------------------------------------------------------------

fn draw_rows(cr: &Context, chain: &Chain, p: &Palette, w: f64) {
    let max = chain.max();
    let cap_w = (w * 0.34).clamp(90.0, 150.0);
    let val_w = 86.0;
    let bar_w = (w - cap_w - val_w - 20.0).max(30.0);

    for (i, st) in chain.stages.iter().enumerate() {
        let y = ROWS_PAD as f64 + i as f64 * ROW_H as f64;

        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        cr.set_font_size(12.0);
        set(cr, p.dim);
        show(cr, 0.0, y + 14.0, &fit(cr, &st.cap, cap_w - 8.0));

        cr.set_font_size(10.5);
        let base = y + 29.0;
        let avail = cap_w + bar_w - 8.0;
        match &st.marked_by {
            Some(code) => {
                set(cr, p.marked);
                let ind = marker(cr, 0.0, base, 10.5);
                show(cr, ind, base, &fit(cr, code, avail - ind));
            }
            None => {
                set(cr, p.dimmer);
                show(cr, 0.0, base, &fit(cr, &st.sub, avail));
            }
        }

        bar(
            cr,
            p,
            cap_w,
            y + 6.0,
            bar_w,
            st.fraction(max),
            st.marked_by.is_some(),
        );

        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Bold);
        cr.set_font_size(13.0);
        set(cr, if st.marked_by.is_some() { p.marked } else { p.fg });
        let v = fit(cr, &st.value, val_w);
        // `x_advance`, not `width`: the ink extent omits the right side bearing,
        // which right-aligns the glyphs a hair past the edge and clips them.
        let tw = cr.text_extents(&v).map(|e| e.x_advance()).unwrap_or(0.0);
        show(cr, w - tw - 2.0, y + 15.0, &v);
    }
}

// ---------------------------------------------------------------------------
// pieces
// ---------------------------------------------------------------------------

/// One bar. `fraction` of `None` means the magnitude is unknown, which is drawn
/// as a dashed outline — not as an empty bar, which would read as zero.
fn bar(cr: &Context, p: &Palette, x: f64, y: f64, w: f64, fraction: Option<f64>, marked: bool) {
    let h = 10.0;
    let r = h / 2.0;

    match fraction {
        Some(f) => {
            set(cr, p.track);
            rounded(cr, x, y, w, h, r);
            let _ = cr.fill();

            let fw = (w * f).max(h);
            set(cr, if marked { p.marked } else { p.fill });
            rounded(cr, x, y, fw, h, r);
            let _ = cr.fill();
        }
        None => {
            set(cr, p.dimmer);
            cr.set_line_width(1.0);
            cr.set_dash(&[3.0, 3.0], 0.0);
            rounded(cr, x + 0.5, y + 0.5, w - 1.0, h - 1.0, r);
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);
        }
    }
}

/// cairo has no rounded-rectangle primitive.
fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.close_path();
}

fn show(cr: &Context, x: f64, y: f64, s: &str) {
    cr.move_to(x, y);
    let _ = cr.show_text(s);
}

fn width_of(cr: &Context, s: &str) -> f64 {
    cr.text_extents(s).map(|e| e.width()).unwrap_or(0.0)
}

/// Truncate to fit, with an ellipsis. Measured against the current font, so it
/// must be called after the font face and size are set.
fn fit(cr: &Context, s: &str, max: f64) -> String {
    if width_of(cr, s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        probe.push('\u{2026}');
        if width_of(cr, &probe) > max {
            break;
        }
        out.push(ch);
    }
    out.push('\u{2026}');
    out
}

/// Greedy word wrap to at most `lines` lines.
///
/// Text that will not fit is ellipsised onto the last line rather than dropped:
/// a caption that silently loses its second half is worse than one that visibly
/// ran out of room.
fn wrap(cr: &Context, s: &str, max: f64, lines: usize) -> Vec<String> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() && out.len() < lines {
        let last_line = out.len() + 1 == lines;
        let mut cur = String::new();
        while i < words.len() {
            let probe = if cur.is_empty() {
                words[i].to_string()
            } else {
                format!("{cur} {}", words[i])
            };
            if !cur.is_empty() && width_of(cr, &probe) > max {
                break;
            }
            cur = probe;
            i += 1;
        }
        if last_line && i < words.len() {
            cur = fit(cr, &format!("{cur} {}", words[i..].join(" ")), max);
            i = words.len();
        }
        out.push(cur);
    }
    out
}
