# Sysi Overlay

Sysi is a lightweight, native Ubuntu desktop overlay built with Rust and GTK 3. It draws directly onto a transparent, always-on-top window without a webview, browser engine, or full-screen backdrop.

## Features

- `SYSTEM` in the GNOME top bar, a little way past the gear, in groups split by a hairline with a caption per device: `CPU 13% 56°C | RAM 48%  SWAP 1% | NVI 12% 45°C  AMD 38°C | SAM 13% 42°C  UMI 33% 41°C | PWR 7.9W | NET ↓9K ↑28K`. GPUs and drives are captioned by the first three letters of their maker; a GPU's percentage is how busy it is, and its memory (`20M/8G`) is always shown used over total so it cannot be read as a second load; a drive's percentage covers every filesystem mounted from it. `PWR` is what the processor package and the GPUs draw on mains; unplugged it becomes `BAT`, the battery's discharge, the whole machine's draw. `power time`, a reading of its own, follows the watts with how long the battery lasts (`BAT 22W 2h30`: 55 Wh left ÷ 22 W) or, charging, how long until it is full (`PWR 8.0W 16m`, or until a charge limit set below full); plugged in and full, there is no time to show. It is worked out at the rate of the last couple of minutes, not the moment's, and rounded to five minutes past the first hour, since it is only as good as the next few minutes' use. Percentages stop at 99% and temperatures at 99°C. The values are set in the panel's own face and size, like the clock.
  The gear's `system` button opens a menu in the style of settings: `enable` / `disable` for the whole row, `used/total` / `percent` for RAM, swap and drives (`12G/16G`, `90G/1T`: memory in powers of two, drives in the powers of ten they are sold in), then a line per reading — cpu, cpu temp, ram, swap, gpu, gpu temp, gpu memory, ssd, ssd temp, power, power time, network — bright while it is on and faint while it is off. Readings the machine cannot give are not offered. The row stops short of the clock: each reading is reckoned at the widest it can ever be (`↓888M ↑888M`), so one let in never drops out as a download climbs, and once the next reading would not fit it greys out; turn another off to make room. A reading that is on but squeezed out (after a monitor change, say) says `no room`. While the gear strip is open it covers the row, and the row comes back when it closes.

  Sysi samples every two seconds, only the readings that are on and only while the row is on, on a thread of its own, and writes them to a small file in `$XDG_RUNTIME_DIR` for the extension, only when something changed.
- Focus countdown with four visual styles, hover controls in both modes, and a persistent animated alarm that must be dismissed.
- A timer in the GNOME top bar. The strip's `timer` opens a menu of presets (1, 3, 5, 10, 15, 25, 45 minutes, 1 hour) and a field that takes `25m`, `1h30`, `10:00` or `@17:30` (count down to that time of day), set with Enter. A timer is set, not started: a pill after SYSTEM's readings shows its full length, dimmed, until the menu's `start`. Running, the pill shows a disk of what is left and the time, white until the last five minutes (or the last quarter of a shorter run) and amber from there. Once one is set the menu offers `start` (then `pause` / `resume`), `+1 min` and `cancel`. At zero the pill turns red, a notification that stays until answered offers `Dismiss` and `+1 min`, and the theme's `alarm-clock-elapsed` sound repeats until then, for a minute at most. A click on the pill answers it too. It counts against the wall clock and keeps running while the screen is locked or the machine sleeps.
- Notes palette: `Super+Shift+L` opens a centred command palette of every note on the monitor of the window you last clicked. Search shows the matching passage, a preview reads the note in place, and Enter pins it on the desk.
  Right-click a note for `TAG`, `PIN` and `DELETE`. `TAG` opens every tag in use, the most used first, with the note's own lit and on top; a click puts the note in a tag or takes it out, and the list stays open for the next. It scrolls past eight tags. Under it, a faint `new tag` field: click it, type a name and press Enter, and the note has that tag (a name in use, but for case, joins that tag). A note on the desk has the same `TAG` in its right-click menu. A note's tags follow its age on the row (`ON DESK · 2d · #speaking`). The `tags` button beside the count narrows the list to notes with every tag picked there, `#speaking +1` saying what it holds; `SHOW ALL` lifts it. There, a tag under the pointer shows `×` in place of its count: a click asks `delete?`, a second takes the tag off every note, and moving to another line takes the question back. A tag no note has any more is gone.
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
- Create multiple independent notes from the `NOTE` action. Hiding a blank note deletes it; notes containing text or images stay in Notes.
- A chatbot answer pasted as Markdown comes in as the text it reads as: `**bold**`, `*italics*`, headings, `<br>` and HTML entities are unwrapped, bullets become `•`, links keep their URL in brackets, and LaTeX such as `$\rightarrow$` or `x^2` becomes `→` and `x²`. A pipe table is redrawn with box characters in a fixed-width face, its columns wrapped to fit the note; a table too wide even for that becomes one short block per row. Code fences and inline code come in verbatim. Text without an unmistakable Markdown mark (a table, `**bold**`, `<br>`, TeX, a link, or a fence) pastes unchanged, so code with `# comments` or `**kwargs` is safe. `Ctrl+Shift+V` pastes exactly what was copied, and `Ctrl+Z` undoes a cleaned paste in one step.
- The panel’s final `settings` button opens colour mode, font size (`− number +`), lock/unlock, and quit. Each window also has its own font-size controls in its right-click menu. Changing the global font size clears the individual overrides; sizes persist across restarts.
- Click-through lock mode. Mouse events pass through everywhere except the windows whose content can still be scrolled.
- `GLASS`, the default, puts every window (notes, usage, dictionaries, the Notes palette) on liquid glass in the style of Apple's Liquid Glass: the desktop behind is frosted, bent at the rim, lit along its edge and from within while you drag it by its title bar, and dimmed just enough that the white text stays at 5:1 contrast or better whatever lies behind it. Windows grow 14px corners in this mode, and a right-click menu or a note's search options opened on one sit on glass too. In Edit Mode, right-click any widget to override its mode; the single colour entry cycles LIGHT → DARK → GLASS → LIGHT. Using the Settings mode button resets every widget to the selected global mode.
  The glass is drawn by the GNOME Shell extension, the only component on Wayland that can see what is behind a window, and it follows a card as it is dragged or resized. Until the extension is running, and whenever GNOME's high-contrast setting is on, `GLASS` cards wear a frosted dark plate instead.
- `OCR` dims the screen so you can drag out a rectangle. The compositor photographs that region with Sysi hidden, `tesseract -l eng+vie` reads it, and the text lands on the clipboard. It needs `tesseract-ocr` and `tesseract-ocr-vie` installed, and on Wayland it needs the panel extension since only the compositor can see Wayland windows.
- HiDPI-aware placement: widgets stop below Ubuntu's top panel and stay inside the real bottom edge of the display.
- Native HiDPI and multi-monitor placement, including 200% scaling.
- A note's header carries a highlighter. Its menu puts the pen down and picks one of four colours; with the pen down, selecting words highlights them, and drawing over a stretch recolours it. With the pen up, select and right-click instead: the menu offers `HIGHLIGHT` above everything else, or `REMOVE` and a colour when the click landed inside a stretch. The wash is translucent, so one set of colours reads on `LIGHT`, `DARK` and `GLASS` alike, and the pen is shared by every note.
- Persistent notes, positions, widget sizes, and visibility settings.
- Automatic startup through XDG Autostart.

## Controls

- `Super+Shift+O` — lock or unlock interaction.
- `Super+Shift+N` — create a new note, ready to type into (same as the panel `NOTE` button). There is only ever one new note: until something is written in it, pressing again brings that note to the pointer instead of making another.
- `Super+Shift+H` — hide or show every Sysi window at once (also `hide` / `show` in the panel's settings menu). Any other panel action or hotkey shows Sysi again first.
- `Super+Shift+L` — open or close the Notes palette (it was `Ctrl+Alt+N` before 0.1.77). Search, move with Up/Down, Enter to open on the desk, Ctrl+P to pin, Delete to delete, Escape to close.
- `Super+Shift+A` — start OCR (same as the panel `ocr` button). Drag a rectangle; the text is copied. Press again, right-click, or Escape to cancel. Super+Shift+A because Super+A is Show Apps and Super+Shift+S is the screenshot UI.
- `Super+Shift+D` — open a new dictionary, ready to type a word into. Like a new note, a dictionary that has not looked anything up yet is still the new one and is brought back rather than doubled.
- `Super+Shift+U` — open or close USAGE.
- `Ctrl+Shift+V` — paste into a note exactly as copied, skipping the Markdown clean-up.
- `Ctrl+F` — find text in the focused note. Use `Enter` / `Shift+Enter` (or
  `F3` / `Shift+F3`) to move between matches and `Escape` to close the panel.
- `Escape` — cancel an OCR selection if one is up; otherwise close the Notes palette, a note's find panel or a dictionary's query panel. It never locks the overlay; that is `Super+Shift+O` or the panel's `LOCK`.
- `sysi --toggle` — toggle interaction from a terminal or a custom desktop shortcut.
- `sysi --quit` — stop the running overlay.

Sysi opens in Edit Mode. Drag a widget to move it. Notes, `USAGE` and `DICTIONARY` resize from any side or corner, the way a window does: the pointer turns into a resize arrow along the edge, and dragging the left or top side keeps the opposite side where it was. Notes show their title bar only in Edit Mode and use it as the move handle. A short click still activates buttons and note editing. While Sysi is running, the gear in the GNOME panel expands to `SYSTEM`, `TIMER`, `NOTE`, `NOTES`, `USAGE`, `DICTIONARY`, `OCR`, and `SETTINGS` directly in the panel.

Quota sources are the CLIs already installed on the machine: `codex app-server` for Codex's `account/rateLimits/read`, `claude -p /usage --output-format json` for Claude Code, and `omp usage --json` for OMP. Claude Code answers `/usage` from what it already knows, at no token cost, but it refuses to run at all while the account is over its limit — so Claude has a second source, Anthropic's OAuth usage endpoint read with the credential Claude Code stored. Either can be shut out by the very quota it reports on, so they cover for each other: the endpoint is asked first because it answers in one request instead of booting a CLI, after that whichever answered last is asked first, a failure falls through to the other, and a working fallback becomes the preferred source, so the pair swaps rather than hammering the blocked side. The stored Claude token is read to send that one request and never written, refreshed or persisted; no other credential is touched. Each source has its own in-flight guard and retry cooldown (2, 4, 8, then 15 minutes on errors). Switching sources or reopening keeps each source's last successful rows visible while its refresh is in flight; failed reads clear that source's quota because account ownership cannot be verified. OMP rows retain per-limit account identifiers in RAM and all rows are scrollable; no raw JSON is saved. Reset labels update locally and each elapsed reset triggers one fetch, subject to cooldown. The card names the signed-in account by the address each CLI stores locally (`~/.codex/auth.json`, `~/.claude.json`, the OMP report metadata) rather than an opaque account id, and a manual refresh runs `omp usage invalidate` first so OMP re-reads the providers instead of replaying its cached report. A missing login, unsupported plan, or provider without quota data is shown as a status message instead of being converted to a fake percentage — for Claude that message is the CLI's own opening line, which says whether the credential is a subscription or an API key.

The `TOKENS` tab talks to nothing. It reads `~/.codex/sessions`, `~/.claude/projects` (subagent transcripts included) and `~/.omp/agent/sessions`, buckets every accounting record by local calendar day, and caches each file against its size and mtime so a rescan only re-reads the session still being written. Each source is counted the way it records itself: Claude Code repeats one response across streaming updates and again in a forked transcript, so a response is keyed and billed once however many files it appears in; Codex logs a running session total, so the tab reads its growth and discards a jump larger than a turn could be, which is the counter a fork inherits from its parent rather than tokens anyone spent. A full scan of a few hundred megabytes of transcripts takes about a third of a second and runs on its own thread, at most once every two minutes while the tab is open.

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

Sysi refreshes this small per-user copy when it starts, so upgrades stay in sync. The panel gear is visible only while Sysi runs. It watches the PID file through a GNOME event monitor—there is no polling loop. Clicking it expands the controls directly in the GNOME panel. Its `LOCK` / `UNLOCK` button changes the desktop widgets between Edit and Lock mode; `Super+Shift+O` still does the same without disabling the panel controls.

## Display support

Sysi is an X11 overlay: always-on-top placement, sticky multi-monitor coverage, and click-through input shaping are all X11 window management. GDK 3 would otherwise pick its Wayland backend whenever `WAYLAND_DISPLAY` is set, where those calls are silent no-ops and the overlay behaves like an ordinary window. So Sysi asks for the X11 backend itself when a display is available, and runs through Xwayland on a Wayland session. A parent shell that exports `GDK_BACKEND=wayland` is ignored so the overlay cannot shrink to one ordinary window; set `SYSI_GDK_BACKEND` to override.

Ubuntu 26.04 (GNOME 50) ships no Xorg session at all — `/usr/share/xsessions` is gone — so this is the path every 26.04 desktop takes.

Xwayland cannot grab a key while a Wayland window is focused, and an Xwayland client cannot take the keyboard from a native Wayland app either. The panel extension therefore grabs `Super+Shift+L`, `Super+Shift+O`, `Super+Shift+A`, `Super+Shift+H`, `Super+Shift+N`, `Super+Shift+D` and `Super+Shift+U` inside the compositor and activates the overlay before Notes or OCR starts. If the extension is off, Sysi falls back to GNOME custom shortcuts (`sysi --panel-action toggle-notes` / `sysi --panel-action ocr`). Notes opened that way may leave the caret outside the search field; OCR still starts, but on Wayland it cannot photograph native windows without the extension. Without the extension `Super+Shift+O`, `Super+Shift+H`, `Super+Shift+N`, `Super+Shift+D` and `Super+Shift+U` do nothing on Wayland; bind `sysi --toggle` / `sysi --panel-action toggle-hidden` / `new-note` / `new-dictionary` / `toggle-usage` yourself. The panel strip's `LOCK` / `UNLOCK`, `NOTES`, and `OCR` buttons are unaffected.
