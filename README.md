# Sysi Overlay

Sysi is a lightweight, native Ubuntu desktop overlay built with Rust and GTK 3. It draws directly onto a transparent, always-on-top window without a webview, browser engine, or full-screen backdrop.

## Features

- CPU and memory rings read directly from `/proc`.
- Circular focus countdown with progress, lock-mode hover controls, and a persistent animated alarm that must be dismissed.
- A compact gear menu with persistent CPU/RAM and timer visibility toggles.
- Note History: click or drag an old note onto the desktop to pin it again.
- Create multiple independent notes from the `NOTE` action.
- Click-through lock mode. Mouse events pass through everywhere except the timer circle, which keeps its hover and click control.
- Manual `LIGHT`, `GRAY`, and `DARK` foreground modes with no screen sampling. In Edit Mode, right-click any widget to give it its own mode; using the Settings mode button resets every widget to the selected mode.
- HiDPI-aware placement: widgets stop below Ubuntu's top panel and stay inside the real bottom edge of the display.
- Native HiDPI and multi-monitor placement, including 200% scaling.
- Persistent notes, positions, widget sizes, timer duration, and visibility settings.
- Automatic startup through XDG Autostart.

## Controls

- `Ctrl+Alt+O` — lock or unlock interaction.
- `Ctrl+Alt+G` — show or hide the Settings button.
- `Escape` — return to click-through lock mode.
- `sysi --toggle` — toggle interaction from a terminal or a custom desktop shortcut.
- `sysi --toggle-settings` — show or hide the Settings button.
- `sysi --quit` — stop the running overlay.

When interaction is unlocked, drag a widget to move it or drag the small bottom-right arc to resize it. Notes show their title bar only in Edit Mode and use it as the move handle. A short click still activates buttons and note editing. The gear menu contains `CPU / RAM`, `TIMER`, the current color mode, `NOTE`, `HISTORY`, and `QUIT`.

In lock mode, hover the timer to reveal `START`, `PAUSE`, `RESUME`, or `DISMISS`, then click the circle to perform that action. In edit mode, double-click the time and enter `MM:SS`, `HH:MM:SS`, or a plain number of minutes. Four consecutive digits such as `1050` are automatically formatted and accepted as `10:50`.

## Build

Ubuntu 24.04 build dependencies:

```bash
sudo apt install build-essential cargo libgtk-3-dev libx11-dev dpkg-dev
./scripts/build-deb.sh
```

The package is written to `dist/`.

## Install

```bash
sudo apt install ./dist/sysi-overlay_0.1.4_amd64.deb
```

Sysi starts automatically on the next desktop login. It can also be launched immediately from the application menu.

## Display support

X11 provides the complete experience: global hotkeys, always-on-top placement, and click-through input shaping. On Wayland, GTK input regions still work, but the compositor may restrict global hotkeys or always-on-top behavior. `sysi --toggle` can be assigned to a GNOME custom keyboard shortcut as a Wayland fallback.
