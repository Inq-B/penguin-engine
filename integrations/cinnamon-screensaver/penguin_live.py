"""Optional read-only Wallpaper Engine frame viewer for Cinnamon Screensaver.

This module is loaded only by the patched MonitorView background widget.
Authentication, the unlock dialog and input grabs are owned by Cinnamon.
"""
from pathlib import Path
import os
import time

from gi.repository import Gdk, GdkPixbuf, GLib, Gtk

RUNTIME = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
ROOT = RUNTIME / "penguin-engine"
CURRENT = ROOT / "lockscreen-live-current"
ENABLED = Path.home() / ".config/penguin-engine/cinnamon-screensaver-live.enabled"

class LiveWallpaper(Gtk.DrawingArea):
    """Composite one shared virtual-desktop image at its physical X11 offset."""

    def __init__(self, rect):
        super().__init__()
        self.rect = (int(rect.x), int(rect.y), int(rect.width), int(rect.height))
        self.pixbuf = None
        self.filename = None
        self.source_id = 0
        self.set_size_request(max(1, self.rect[2]), max(1, self.rect[3]))
        self.connect("draw", self.on_draw)
        self.connect("destroy", self.on_destroy)
        self.source_id = GLib.timeout_add(400, self.refresh)
        self.refresh()

    def on_destroy(self, *args):
        if self.source_id:
            GLib.source_remove(self.source_id)
            self.source_id = 0

    def refresh(self):
        if not ENABLED.is_file():
            return GLib.SOURCE_REMOVE
        try:
            # Only the two expected filenames are accepted, not arbitrary paths.
            name = CURRENT.read_text(encoding="ascii").strip()
            if name not in ("lockscreen-live-a.png", "lockscreen-live-b.png"):
                return GLib.SOURCE_CONTINUE
            path = ROOT / name
            if path.name == self.filename:
                return GLib.SOURCE_CONTINUE
            pixbuf = GdkPixbuf.Pixbuf.new_from_file(str(path))
            self.pixbuf = pixbuf
            self.filename = path.name
            self.queue_draw()
        except (OSError, GLib.Error, ValueError):
            # A producer may be between atomic updates or shut down.
            pass
        return GLib.SOURCE_CONTINUE

    def on_draw(self, widget, cr):
        if self.pixbuf is None:
            cr.set_source_rgb(0.0, 0.0, 0.0)
            cr.paint()
            return False

        x, y, width, height = self.rect
        # PNGs use the XRandR virtual desktop coordinates, while this widget
        # occupies an individual screen (or the single spanned stage).
        Gdk.cairo_set_source_pixbuf(cr, self.pixbuf, -x, -y)
        cr.paint()

        # Match Cinnamon's existing MonitorView.shade_wallpaper callback.
        # The lock dialog remains readable and its controls stay above us.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.7)
        cr.paint()
        return False

def maybe_live_wallpaper(rect):
    """Fall back to Cinnamon's regular background when integration is off."""
    if not ENABLED.is_file() or not CURRENT.is_file():
        return None
    try:
        return LiveWallpaper(rect)
    except Exception as exc:
        print("penguin-live: could not start wallpaper background:", exc)
        return None
