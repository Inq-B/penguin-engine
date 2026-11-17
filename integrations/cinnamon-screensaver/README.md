# Experimental native Cinnamon Screensaver integration (X11)

**Branch:** `feat/cinnamon-screensaver` — separate from the working
`feat/x11-backend` snapshot integration.

Cinnamon Screensaver doesn't offer a supported plug-in API for wallpaper
content. Its lock-screen `Stage` is a GTK3 overlay with `MonitorView`
widgets responsible for drawing the background. This prototype adds a
**background-only, read-only** GTK `DrawingArea` adapter to that widget.
It does **not** replace the lock screen, intercept passwords, patch PAM,
change input grabs, or install a different lock daemon.

## How it works

- The `penguind-x11` daemon continues to render the normal wallpaper.
- When the normal **Lock-screen wallpaper snapshot** setting is enabled,
  the first GPU snapshot primes a frame in
  `$XDG_RUNTIME_DIR/penguin-engine/lockscreen-live-a.png`.
- A user-owned opt-in marker at
  `~/.config/penguin-engine/cinnamon-screensaver-live.enabled`
  enables the prototype.
- After Cinnamon signals that it is locked, Penguin Engine captures a new
  **wallpaper-only** GPU frame at a maximum of ~2 FPS. The daemon publishes
  completed PNGs to two alternating filenames in the per-user runtime
  directory and atomically updates `lockscreen-live-current`.
- The patched Cinnamon `MonitorView` loads those frames into a GTK
  background widget at the correct monitor offset. It keeps Cinnamon's
  familiar 70% shading and all unlock/clock/UI widgets above the background.
- If the stream is unavailable, Cinnamon uses its original static background.

This is an *experimental, low-framerate animation*, not an interactive
wallpaper. It may consume significant CPU when compressing large (e.g.,
4480x1440) PNGs. Start by testing one lightweight wallpaper.

## Installation (Arch/Cinnamon)

From the repository root, build the daemon with default features enabled:

```bash
git switch feat/cinnamon-screensaver
cargo install --path apps/penguind --bins --locked --force
```

**Inspect the installer and the adapter before giving root access.** The
installer modifies the system Python file
`/usr/share/cinnamon-screensaver/monitorView.py`, keeps an original
`monitorView.py.penguin-original` backup, copies `penguin_live.py`
alongside it, and refuses to patch an unfamiliar version. Any Cinnamon
package upgrade may overwrite the patch, in which case inspect and
reapply it.

```bash
python3 -m unittest discover integrations/cinnamon-screensaver -p 'test_*.py'
sudo python3 integrations/cinnamon-screensaver/install.py install
mkdir -p ~/.config/penguin-engine
touch ~/.config/penguin-engine/cinnamon-screensaver-live.enabled
systemctl --user restart penguin-engine-x11.service
```

Enable **Lock-screen wallpaper snapshot** in Penguin Engine's Settings →
Displays. Confirm the runtime stream is primed before locking:

```bash
cat "$XDG_RUNTIME_DIR/penguin-engine/lockscreen-live-current"
ls -lh "$XDG_RUNTIME_DIR/penguin-engine/"lockscreen-live-*.png
```

Now restart the Cinnamon Screensaver process **after unlocking**, so its
next Stage imports the modified Python module. On many Cinnamon versions:

```bash
cinnamon-screensaver-command --exit
```

The Cinnamon session will normally reactivate the screensaver service when
needed. Do not stop or kill the screensaver while it is handling an active
lock. Then lock with your ordinary shortcut and verify frame updates.

## Disable / uninstall

From an unlocked desktop:

```bash
rm -f ~/.config/penguin-engine/cinnamon-screensaver-live.enabled
systemctl --user restart penguin-engine-x11.service
sudo python3 integrations/cinnamon-screensaver/install.py uninstall
```

Restart the Cinnamon Screensaver process after unlocking to unload the
optional module. The standard static snapshot feature remains usable.

## Limitations and safety

- Cinnamon versions with different `monitorView.py` contents are deliberately
  rejected by the patcher.
- This touches a system-owned package file; a Cinnamon update may require
  reinstalling the integration. An ordinary desktop screenshot is **not**
  used; only wallpaper renderer pixels leave the GPU.
- The adapter uses 2 FPS PNGs initially for simplicity and safe file
  delivery, not shared-memory textures or a video decoder. This is a
  proof-of-concept and not designed as a high-refresh screensaver.
- The Python adapter reads frame files only and never receives authentication
  data. The original Cinnamon logic handles all credentials, wake events,
  keyboard grabs and unlock decisions.
- Only tested via source review / CI unless explicitly verified on a
  live Cinnamon X11 session. Be prepared to uninstall if the lock screen
  becomes unstable.
