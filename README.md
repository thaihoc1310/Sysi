# Sysi Overlay

Sysi is a lightweight, native Ubuntu desktop overlay built with Rust and GTK 3. It draws directly onto a transparent, always-on-top window without a webview, browser engine, or full-screen backdrop.

## Features

- A compact `SYSTEM` widget with CPU/RAM rings. Its right-click menu can add a fixed five-row top-process list (with CPU, ID, and memory) and compact per-core percentages; the extra `/proc` reads run only while their section is enabled.
- Focus countdown with four visual styles, hover controls in both modes, and a persistent animated alarm that must be dismissed.
- A compact gear menu with persistent SYSTEM and timer visibility toggles.
- Note History: click or drag an old note onto the desktop to pin it again.
- Create multiple independent notes from the `NOTE` action.
- Click-through lock mode. Mouse events pass through everywhere except the timer circle, which keeps its hover and click control.
- `AUTO` samples the background beneath each widget and chooses a contrasting `LIGHT` or `DARK` foreground. In Edit Mode, right-click any widget to override it; its current mode is omitted from the menu. Using the Settings mode button resets every widget to the selected global mode.
  `AUTO` reads those pixels through the GNOME Shell extension, the only component that can see Wayland windows; without it a widget keeps whichever foreground it already had.
- Right-click the timer, then hover `STYLE` to preview `RING`, `DIGITAL`, `TICKS`, or `ARC`; click one to keep it.
- HiDPI-aware placement: widgets stop below Ubuntu's top panel and stay inside the real bottom edge of the display.
- Native HiDPI and multi-monitor placement, including 200% scaling.
- Persistent notes, positions, widget sizes, timer duration, and visibility settings.
- Automatic startup through XDG Autostart.

## Controls

- `Ctrl+Alt+O` — lock or unlock interaction.
- `Ctrl+F` — find text in the focused note. Use `Enter` / `Shift+Enter` (or
  `F3` / `Shift+F3`) to move between matches and `Escape` to close the panel.
- `Escape` — return to click-through lock mode.
- `sysi --toggle` — toggle interaction from a terminal or a custom desktop shortcut.
- `sysi --quit` — stop the running overlay.

Sysi opens in Edit Mode. Drag a widget to move it or drag the small bottom-right arc to resize it. Notes show their title bar only in Edit Mode and use it as the move handle. A short click still activates buttons and note editing. While Sysi is running, the gear in the GNOME panel expands to `SYSTEM`, `TIMER`, `AUTO` / `LIGHT` / `DARK`, `LOCK` / `UNLOCK`, `NOTE`, `HISTORY`, `DICTIONARY`, and `QUIT` directly in the panel.

In either mode, hovering the timer overlays `START`, `PAUSE`, `RESUME`, or `DISMISS` over the time; click to perform that action. In Edit Mode, right-click the timer and choose `EDIT TIME` to enter `MM:SS`, `HH:MM:SS`, or a plain number of minutes. Four consecutive digits such as `1050` are automatically formatted and accepted as `10:50`.

## Build

Ubuntu 24.04 and 26.04 build dependencies:

```bash
sudo apt install build-essential cargo libgtk-3-dev libx11-dev dpkg-dev
./scripts/build-deb.sh
```

The package is written to `dist/`.

## Install

```bash
sudo apt install ./dist/sysi-overlay_0.1.48_amd64.deb
```

Sysi starts automatically on the next desktop login. It can also be launched immediately from the application menu.

On Ubuntu GNOME, install the panel extension into your own extension directory, reload Shell, then enable it once:

```bash
/usr/bin/sysi --install-panel-extension
# Press Alt+F2, type r, then press Enter (X11 only).
# A Wayland session has no in-place Shell restart: log out and back in instead.
gnome-extensions enable sysi-panel@thaihoc
```

Sysi refreshes this small per-user copy when it starts, so upgrades stay in sync. The panel gear is visible only while Sysi runs. It watches the PID file through a GNOME event monitor—there is no polling loop. Clicking it expands the controls directly in the GNOME panel. Its `LOCK` / `UNLOCK` button changes the desktop widgets between Edit and Lock mode; `Ctrl+Alt+O` still does the same without disabling the panel controls.

## Display support

Sysi is an X11 overlay: always-on-top placement, sticky multi-monitor coverage, and click-through input shaping are all X11 window management. GDK 3 would otherwise pick its Wayland backend whenever `WAYLAND_DISPLAY` is set, where those calls are silent no-ops and the overlay behaves like an ordinary window. So Sysi asks for the X11 backend itself when a display is available, and runs through Xwayland on a Wayland session. Set `GDK_BACKEND` yourself to override that choice.

Ubuntu 26.04 (GNOME 50) ships no Xorg session at all — `/usr/share/xsessions` is gone — so this is the path every 26.04 desktop takes.

One thing Xwayland cannot give back is a truly global hotkey. `Ctrl+Alt+O` is grabbed on the X server, so it fires only while an X11 window holds focus; a focused Wayland window never delivers it. For a hotkey that works everywhere, bind `sysi --toggle` to a GNOME custom keyboard shortcut in Settings → Keyboard → Custom Shortcuts. The panel strip's `LOCK` / `UNLOCK` button and `Escape` are unaffected.
