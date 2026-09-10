# Sysi Overlay

Sysi is a lightweight, native Ubuntu desktop overlay built with Rust and GTK 3. It draws directly onto a transparent, always-on-top window without a webview, browser engine, or full-screen backdrop.

## Features

- A compact `SYSTEM` widget with CPU/RAM rings. Its right-click menu adds:
  - `SWAP` and `GPUS` rings, and a `TEMPERATURE` submenu with `CPU`, `GPU`, and `SSD` rings. A drive ring is captioned with its maker, so `SAMSUNG` and `UMIS` rather than `SSD 1` and `SSD 2`; a drive from nobody recognisable falls back to its product line, then to its size. A temperature ring is filled on a fixed 0-100 degree scale and reads `45C` in the middle, so its caption stays free for the sensor's name; past 85 degrees the ring and its number turn warm, the card's only colour.
  - `MEMORY DETAIL`, `DISK /`, and `DISK /HOME` capacity rows: a bar, how much of the space is gone, the total, and the percentage. `/home` is omitted when it shares a filesystem with `/`, and swap when the machine has none.
  - A fixed five-row top-process list (with CPU, ID, and memory), compact per-core percentages, and a `NETWORK` row with the current download and upload rate over the physical interfaces.

  Every sensor, `/proc` walk, and `nvidia-smi` call runs only while its section is enabled, and all of it on a sampler thread rather than the GTK main loop.
- Focus countdown with four visual styles, hover controls in both modes, and a persistent animated alarm that must be dismissed.
- A compact gear menu with persistent SYSTEM and timer visibility toggles.
- Note History: click or drag an old note onto the desktop to pin it again.
- A small transparent `USAGE` card for Codex, Claude Code, and Oh My Pi (OMP). It
  shows the remaining percentage, the server-provided reset time, and the age
  of the last snapshot. The card polls only while visible, keeps the previous
  values on screen while refreshing, and replaces them with an error if
  refresh fails.
- A `TOKENS` tab on the same card totalling what has actually been spent, read
  from the session logs those three CLIs already keep on the disk. One headline
  figure, a bar splitting it between the three, and a bar per source; hover any
  of them for the exact count and the input / output / cache breakdown. The
  window is `TODAY`, `7D`, `30D`, or everything the logs still hold.
- Create multiple independent notes from the `NOTE` action. Hiding a blank note deletes it; notes containing text or images stay in History.
- The panel’s final `settings` button opens colour mode, font size (`− number +`), lock/unlock, and quit. Each window also has its own font-size controls in its right-click menu. Changing the global font size clears the individual overrides; sizes persist across restarts.
- Click-through lock mode. Mouse events pass through everywhere except the timer circle, which keeps its hover and click control.
- `AUTO` samples the background beneath each widget and chooses a contrasting `LIGHT` or `DARK` foreground. In Edit Mode, right-click any widget to override it; the single colour entry cycles AUTO → LIGHT → DARK → AUTO. Using the Settings mode button resets every widget to the selected global mode.
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

Sysi opens in Edit Mode. Drag a widget to move it or drag the small bottom-right arc to resize it. Notes show their title bar only in Edit Mode and use it as the move handle. A short click still activates buttons and note editing. While Sysi is running, the gear in the GNOME panel expands to `SYSTEM`, `TIMER`, `NOTE`, `HISTORY`, `USAGE`, `DICTIONARY`, and `SETTINGS` directly in the panel.

Quota sources are the CLIs already installed on the machine: `codex app-server` for Codex's `account/rateLimits/read`, `claude -p /usage --output-format json` for Claude Code, and `omp usage --json` for OMP. Claude Code answers `/usage` from what it already knows, at no token cost, but it refuses to run at all while the account is over its limit — so Claude has a second source, Anthropic's OAuth usage endpoint read with the credential Claude Code stored. Either can be shut out by the very quota it reports on, so they cover for each other: the endpoint is asked first because it answers in one request instead of booting a CLI, after that whichever answered last is asked first, a failure falls through to the other, and a working fallback becomes the preferred source, so the pair swaps rather than hammering the blocked side. The stored Claude token is read to send that one request and never written, refreshed or persisted; no other credential is touched. Each source has its own in-flight guard and retry cooldown (2, 4, 8, then 15 minutes on errors). Switching sources or reopening keeps each source's last successful rows visible while its refresh is in flight; failed reads clear that source's quota because account ownership cannot be verified. OMP rows retain per-limit account identifiers in RAM and all rows are scrollable; no raw JSON is saved. Reset labels update locally and each elapsed reset triggers one fetch, subject to cooldown. The card names the signed-in account by the address each CLI stores locally (`~/.codex/auth.json`, `~/.claude.json`, the OMP report metadata) rather than an opaque account id, and a manual refresh runs `omp usage invalidate` first so OMP re-reads the providers instead of replaying its cached report. A missing login, unsupported plan, or provider without quota data is shown as a status message instead of being converted to a fake percentage — for Claude that message is the CLI's own opening line, which says whether the credential is a subscription or an API key.

The `TOKENS` tab talks to nothing. It reads `~/.codex/sessions`, `~/.claude/projects` (subagent transcripts included) and `~/.omp/agent/sessions`, buckets every accounting record by local calendar day, and caches each file against its size and mtime so a rescan only re-reads the session still being written. Each source is counted the way it records itself: Claude Code repeats one response across streaming updates and again in a forked transcript, so a response is keyed and billed once however many files it appears in; Codex logs a running session total, so the tab reads its growth and discards a jump larger than a turn could be, which is the counter a fork inherits from its parent rather than tokens anyone spent. A full scan of a few hundred megabytes of transcripts takes about a third of a second and runs on its own thread, at most once every two minutes while the tab is open.

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
sudo apt install ./dist/sysi-overlay_0.1.66_amd64.deb
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
