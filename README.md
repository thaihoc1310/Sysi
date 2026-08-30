# Sysi Overlay

Sysi is a lightweight, native Ubuntu desktop overlay built with Rust and GTK 3. It draws directly onto a transparent, always-on-top window without a webview, browser engine, or full-screen backdrop.

## Features

- A compact `SYSTEM` widget with CPU/RAM rings. Its right-click menu can add a fixed five-row top-process list (with CPU, ID, and memory) and compact per-core percentages; the extra `/proc` reads run only while their section is enabled.
- Focus countdown with four visual styles, hover controls in both modes, and a persistent animated alarm that must be dismissed.
- A compact gear menu with persistent SYSTEM and timer visibility toggles.
- Note History: click or drag an old note onto the desktop to pin it again.
- Create multiple independent notes from the `NOTE` action.
- Click-through lock mode. Mouse events pass through everywhere except the timer circle, which keeps its hover and click control.
- Manual `LIGHT`, `GRAY`, and `DARK` foreground modes with no screen sampling. In Edit Mode, right-click any widget to give it its own mode; using the Settings mode button resets every widget to the selected mode.
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

Sysi opens in Edit Mode. Drag a widget to move it or drag the small bottom-right arc to resize it. Notes show their title bar only in Edit Mode and use it as the move handle. A short click still activates buttons and note editing. While Sysi is running, the gear in the GNOME panel expands to `SYSTEM`, `TIMER`, `LIGHT` / `GRAY` / `DARK`, `LOCK` / `UNLOCK`, `NOTE`, `HISTORY`, and `QUIT` directly in the panel.

In either mode, hovering the timer overlays `START`, `PAUSE`, `RESUME`, or `DISMISS` over the time; click to perform that action. In Edit Mode, right-click the timer and choose `EDIT TIME` to enter `MM:SS`, `HH:MM:SS`, or a plain number of minutes. Four consecutive digits such as `1050` are automatically formatted and accepted as `10:50`.

## Build

Ubuntu 24.04 build dependencies:

```bash
sudo apt install build-essential cargo libgtk-3-dev libx11-dev dpkg-dev
./scripts/build-deb.sh
```

The package is written to `dist/`.

## Install

```bash
sudo apt install ./dist/sysi-overlay_0.1.41_amd64.deb
```

Sysi starts automatically on the next desktop login. It can also be launched immediately from the application menu.

On Ubuntu GNOME, install the panel extension into your own extension directory, reload Shell, then enable it once:

```bash
/usr/bin/sysi --install-panel-extension
# Press Alt+F2, type r, then press Enter (X11 only).
gnome-extensions enable sysi-panel@thaihoc
```

Sysi refreshes this small per-user copy when it starts, so upgrades stay in sync. The panel gear is visible only while Sysi runs. It watches the PID file through a GNOME event monitor—there is no polling loop. Clicking it expands the controls directly in the GNOME panel. Its `LOCK` / `UNLOCK` button changes the desktop widgets between Edit and Lock mode; `Ctrl+Alt+O` still does the same without disabling the panel controls.

## Display support

X11 provides the complete experience: global hotkeys, always-on-top placement, and click-through input shaping. On Wayland, GTK input regions still work, but the compositor may restrict global hotkeys or always-on-top behavior. `sysi --toggle` can be assigned to a GNOME custom keyboard shortcut as a Wayland fallback.
