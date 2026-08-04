#!/usr/bin/env python3
"""Check that every shipped icon can actually be loaded as an icon.

Run it directly, or let CI run it:

    ./scripts/check-icons.py

# Why this exists

An SVG can be perfectly valid, render correctly in a browser, pass an XML
parser — and still fail to load as an *icon*, silently, leaving a blank tile in
the app grid with no error anywhere the user can see.

That happened here. The application icon carried a long explanatory comment
above its opening tag, which pushed `<svg` to byte 905. gdk-pixbuf sniffs for the
SVG signature only near the start of a file, so it never recognised the format
and GNOME drew nothing. A sibling project's icon, with a 134-byte preamble,
loaded fine, which is what made the boundary obvious. Comments belong *inside*
the element.

# Two checks, because one of them travels and the other is decisive

`gi`/GdkPixbuf gives the real answer — it is the same code path GNOME uses — but
it is not installed everywhere, notably not on a bare CI runner. The byte-offset
check needs nothing but the standard library and catches the exact regression
above, so it always runs. When GdkPixbuf is available, a real load runs too.

# A load failure is only evidence if the loader works at all

GdkPixbuf can be importable on a machine with no SVG loader installed — a CI
runner with `python3-gi` and no `librsvg2-common`, for instance. Every icon then
"fails", which says nothing about the icons: this script's first version did
exactly that and turned the baseline CI leg red while the icons were fine.

So a control comes first: a minimal SVG written to a temporary file. If *that*
cannot be loaded, this environment cannot rasterise SVG and the load check is
skipped with a note. Only when the control loads does a failure on a real icon
mean anything. It is the same reasoning that found the original bug — a sibling
project's icon loaded, ours did not, so the loader was not the problem.
"""

import glob
import os
import sys
import tempfile

# Comfortably below where sniffing gave up (a 905-byte preamble failed) and
# comfortably above any reasonable `<?xml ...?>` declaration.
MAX_SIGNATURE_OFFSET = 256

ICONS = "data/icons/hicolor/*/apps/*.svg"

# The smallest thing that is unarguably a loadable SVG. If this fails, the
# machine has no SVG loader and nothing can be concluded about the real icons.
CONTROL = (
    b'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">'
    b'<rect width="16" height="16" fill="#000"/></svg>'
)


def loader_works(loader) -> bool:
    """Can this machine rasterise SVG at all?"""
    with tempfile.NamedTemporaryFile(suffix=".svg", delete=False) as f:
        f.write(CONTROL)
        control = f.name
    try:
        loader.new_from_file_at_scale(control, 16, 16, True)
        return True
    except Exception:  # noqa: BLE001 — any failure means "no usable loader"
        return False
    finally:
        os.unlink(control)


def main() -> int:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    os.chdir(root)

    paths = sorted(glob.glob(ICONS))
    if not paths:
        print(f"no icons matched {ICONS} — wrong directory?", file=sys.stderr)
        return 1

    loader = None
    try:
        import gi

        gi.require_version("GdkPixbuf", "2.0")
        from gi.repository import GdkPixbuf

        loader = GdkPixbuf.Pixbuf
    except Exception as e:  # noqa: BLE001 — any import problem means "not here"
        print(f"note: GdkPixbuf unavailable ({type(e).__name__}), offset check only")

    if loader is not None and not loader_works(loader):
        # Importable but toothless: python3-gi without librsvg2-common, say. The
        # icons cannot be judged here, and calling them broken would be wrong.
        print("note: no working SVG loader on this machine, offset check only")
        loader = None

    failed = False
    for path in paths:
        with open(path, "rb") as f:
            head = f.read(4096)
        offset = head.find(b"<svg")

        if offset < 0:
            print(f"FAIL {path}: no <svg> tag in the first {len(head)} bytes")
            failed = True
            continue
        if offset > MAX_SIGNATURE_OFFSET:
            print(
                f"FAIL {path}: <svg> starts at byte {offset}, past the "
                f"{MAX_SIGNATURE_OFFSET}-byte limit — an icon loader will not "
                f"recognise this file. Move comments inside the element."
            )
            failed = True
            continue

        if loader is not None:
            try:
                pb = loader.new_from_file_at_scale(path, 48, 48, True)
                print(f"ok   {path}  (<svg at {offset}, loads {pb.get_width()}x{pb.get_height()})")
            except Exception as e:  # noqa: BLE001 — the loader's own failure is the result
                print(f"FAIL {path}: will not load as an icon — {str(e)[:100]}")
                failed = True
        else:
            print(f"ok   {path}  (<svg at {offset})")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
