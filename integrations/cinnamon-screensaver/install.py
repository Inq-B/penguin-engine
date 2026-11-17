#!/usr/bin/env python3
"""Reversible native Cinnamon Screensaver background-only integration.

Requires root for the system-owned Python package, not for changing the
user's session settings. This intentionally does not touch PAM, manager.py,
service.py, input grabs, or the lock/unlock dialog.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import os
import py_compile
import shutil
import sys
import tempfile

DEFAULT = Path("/usr/share/cinnamon-screensaver")
ORIGINAL_NAME = "monitorView.py"
BACKUP_NAME = "monitorView.py.penguin-original"
MODULE_NAME = "penguin_live.py"
HOOK = '''    def set_next_wallpaper_image(self, image):
        self.wallpaper_stack.transition_to_image(image)'''
REPLACEMENT = '''    def set_next_wallpaper_image(self, image):
        # BEGIN PENGUIN LIVE BACKGROUND
        # Only the wallpaper widget is replaced. Cinnamon's stage, keyboard
        # grabs, password prompt and PAM auth are not modified.
        try:
            from penguin_live import maybe_live_wallpaper
            live = maybe_live_wallpaper(self.rect)
            if live is not None:
                image = live
        except Exception as exc:
            print("penguin-live: reverting to Cinnamon background:", exc)
        # END PENGUIN LIVE BACKGROUND
        self.wallpaper_stack.transition_to_image(image)'''


def replace_file(path: Path, data: bytes, mode: int) -> None:
    # Write into the target directory and rename atomically to avoid leaving
    # screensaver Python half-written if installation is interrupted.
    descriptor, filename = tempfile.mkstemp(prefix=".penguin-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(filename, mode)
        os.replace(filename, path)
    finally:
        try:
            os.unlink(filename)
        except FileNotFoundError:
            pass


def install(folder: Path) -> None:
    target = folder / ORIGINAL_NAME
    backup = folder / BACKUP_NAME
    module = folder / MODULE_NAME
    if not target.is_file():
        raise RuntimeError(f"Cinnamon Screensaver file not found: {target}")
    if not folder.is_dir():
        raise RuntimeError(f"Cinnamon Screensaver directory not found: {folder}")
    existing = target.read_text(encoding="utf-8")
    if "BEGIN PENGUIN LIVE BACKGROUND" in existing:
        print("Cinnamon integration already installed.")
        return
    if backup.exists() or module.exists():
        raise RuntimeError(
            "Existing backup or module found; refusing to overwrite it. "
            "Review the installation before proceeding."
        )
    if existing.count(HOOK) != 1:
        raise RuntimeError(
            "Cinnamon's MonitorView does not match the supported hook. "
            "No system files were changed."
        )

    updated = existing.replace(HOOK, REPLACEMENT)
    # Verify the resulting source parses before touching system files.
    with tempfile.TemporaryDirectory() as temp:
        candidate = Path(temp) / ORIGINAL_NAME
        candidate.write_text(updated, encoding="utf-8")
        py_compile.compile(str(candidate), doraise=True)
        py_compile.compile(str(Path(__file__).with_name(MODULE_NAME)), doraise=True)

    original_mode = target.stat().st_mode & 0o777
    shutil.copy2(target, backup)
    try:
        replace_file(module, Path(__file__).with_name(MODULE_NAME).read_bytes(), 0o644)
        replace_file(target, updated.encode("utf-8"), original_mode)
    except Exception:
        replace_file(target, backup.read_bytes(), original_mode)
        module.unlink(missing_ok=True)
        raise
    print(f"Installed the optional background adapter in {folder}.")
    print("Now enable your USER marker and restart the user wallpaper daemon.")
    print("Reapply this adapter if a Cinnamon package update replaces monitorView.py.")


def uninstall(folder: Path) -> None:
    target = folder / ORIGINAL_NAME
    backup = folder / BACKUP_NAME
    module = folder / MODULE_NAME
    if not backup.exists():
        raise RuntimeError("No Penguin Engine backup exists. Refusing to alter Cinnamon.")
    if target.is_file() and "BEGIN PENGUIN LIVE BACKGROUND" not in target.read_text(encoding="utf-8"):
        raise RuntimeError(
            "MonitorView was replaced since installation (perhaps by a package update). "
            "Refusing to overwrite the newer Cinnamon file."
        )
    replace_file(target, backup.read_bytes(), backup.stat().st_mode & 0o777)
    backup.unlink()
    module.unlink(missing_ok=True)
    print("Restored original Cinnamon MonitorView. Remove the user marker to disable the daemon.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("install", "uninstall"))
    parser.add_argument("--folder", type=Path, default=DEFAULT,
                        help=f"Cinnamon Screensaver Python folder (default: {DEFAULT})")
    args = parser.parse_args()
    try:
        if args.action == "install":
            install(args.folder)
        else:
            uninstall(args.folder)
    except (OSError, RuntimeError, py_compile.PyCompileError) as exc:
        print(f"penguin-cinnamon: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
