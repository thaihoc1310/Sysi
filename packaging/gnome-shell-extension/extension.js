import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const UUID = 'sysi-panel@thaihoc';

Gio._promisify(Shell.Screenshot.prototype, 'pick_color');
Gio._promisify(Shell.Screenshot.prototype, 'screenshot_area');

export default class SysiPanelExtension extends Extension {
    enable() {
        this._indicator = new PanelMenu.Button(0.0, 'Sysi', true);
        // The parent only lays out the controls. Individual buttons own their
        // hover state, so moving across the strip never lights the whole row.
        this._indicator.remove_style_class_name('panel-button');
        this._indicator.reactive = false;
        this._indicator.can_focus = false;
        this._indicator.track_hover = false;
        // FILL, not CENTER: the hover block is drawn on the button's own
        // allocation, so the row has to run the whole height of the panel for
        // that block to reach the top and bottom edges.
        this._content = new St.BoxLayout({
            style_class: 'sysi-panel-row',
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        this._indicator.add_child(this._content);

        this._gear = new St.Button({
            style_class: 'sysi-panel-gear',
            reactive: true,
            can_focus: true,
            track_hover: true,
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        this._gear.add_child(new St.Icon({
            icon_name: 'preferences-system-symbolic',
            // Not `system-status-icon`: that class carries the shell's own
            // sizing, which is what made the gear tower over the text beside
            // it. The replacement class must set an icon-size of its own —
            // with none in force the icon draws at nothing at all.
            icon_size: 12,
            style_class: 'sysi-panel-gear-icon',
        }));
        this._content.add_child(this._gear);

        this._strip = new St.BoxLayout({
            style_class: 'sysi-panel-row',
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        this._content.add_child(this._strip);
        this._strip.visible = false;

        this._system = this._addAction('system', 'toggle-system');
        this._timer = this._addAction('timer', 'toggle-timer');
        this._addAction('+ note', 'new-note');
        this._addAction('history', 'toggle-history');
        this._addAction('usage', 'toggle-usage');
        this._addAction('dictionary', 'toggle-translate');
        this._buildSettings();

        this._gear.connect('clicked', () => {
            this._syncPanelState();
            this._strip.visible = !this._strip.visible;
            if (!this._strip.visible)
                this._settingsMenu.close();
        });

        // Append after Ubuntu's left-side indicator instead of prepending it.
        Main.panel.addToStatusArea(UUID, this._indicator, -1, 'left');
        this._pidFile = Gio.File.new_for_path(
            GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', 'pid']),
        );
        try {
            this._pidMonitor = this._pidFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._pidMonitor.connect('changed', () => this._syncVisibility());
        } catch (error) {
            logError(error, 'Sysi panel gear could not watch the app state');
        }
        // Both labels describe state Sysi owns, and either can be changed
        // without going near this strip — locking with Escape or the hotkey,
        // cycling the colour from the widget picker. So the strip never guesses
        // from its own clicks; it reads what Sysi published.
        this._panelStateFile = Gio.File.new_for_path(
            GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', 'panel-state']),
        );
        try {
            this._panelStateMonitor = this._panelStateFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._panelStateMonitor.connect('changed', () => this._syncPanelState());
        } catch (error) {
            logError(error, 'Sysi panel gear could not watch the overlay state');
        }
        const cacheDir = GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi']);
        GLib.mkdir_with_parents(cacheDir, 0o700);
        // One PNG per INVERT widget, rewritten every sample. The runtime dir
        // is a tmpfs, so a mode left on all day never touches the disk, and
        // the pictures go away with the session. GLib falls back to the cache
        // directory when there is no runtime one.
        this._invertDir = GLib.build_filenamev([
            GLib.get_user_runtime_dir(), 'sysi', 'invert',
        ]);
        GLib.mkdir_with_parents(this._invertDir, 0o700);
        this._autoColorRequestFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'auto-color-request']),
        );
        if (!this._autoColorRequestFile.query_exists(null))
            GLib.file_set_contents(this._autoColorRequestFile.get_path(), '');
        this._autoColorGeneration = (this._autoColorGeneration ?? 0) + 1;
        this._autoColorSampling = false;
        this._autoColorPending = false;
        try {
            this._autoColorRequestMonitor = this._autoColorRequestFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._autoColorRequestMonitor.connect('changed', () => {
                this._queueAutoColorSampling();
            });
        } catch (error) {
            logError(error, 'Sysi could not watch auto-colour requests');
        }
        this._syncPanelState();
        this._syncVisibility();
        // Sampling before the shell has laid out its monitors makes
        // Shell.Screenshot paint a 0x0 buffer, and pick_color_finish then
        // dereferences the missing image and takes the whole shell down.
        if (Main.layoutManager._startingUp) {
            this._startupCompleteId = Main.layoutManager.connect('startup-complete', () => {
                Main.layoutManager.disconnect(this._startupCompleteId);
                this._startupCompleteId = 0;
                this._queueAutoColorSampling();
            });
        } else {
            this._queueAutoColorSampling();
        }
    }

    disable() {
        this._pidMonitor?.cancel();
        this._pidMonitor = null;
        this._panelStateMonitor?.cancel();
        this._panelStateMonitor = null;
        if (this._startupCompleteId) {
            Main.layoutManager.disconnect(this._startupCompleteId);
            this._startupCompleteId = 0;
        }
        this._autoColorRequestMonitor?.cancel();
        this._autoColorRequestMonitor = null;
        this._autoColorRequestFile = null;
        this._autoColorGeneration++;
        this._autoColorSampling = false;
        this._autoColorPending = false;
        this._settingsMenu?.destroy();
        this._settingsMenu = null;
        this._fontLabel = null;
        this._indicator?.destroy();
        this._indicator = null;
        this._content = null;
        this._strip = null;
        this._gear = null;
        this._system = null;
        this._timer = null;
        this._modeLabel = null;
        this._lockLabel = null;
        this._pidFile = null;
        this._panelStateFile = null;
        this._invertDir = null;
    }

    _buildSettings() {
        const button = this._buildPanelButton('settings');
        this._settingsMenu = new PopupMenu.PopupMenu(button, 0.5, St.Side.TOP);
        this._settingsMenu.actor.add_style_class_name('sysi-settings-menu');
        Main.uiGroup.add_child(this._settingsMenu.actor);
        this._settingsMenu.actor.hide();
        Main.panel.menuManager.addMenu(this._settingsMenu);
        button.connect('clicked', () => {
            this._syncPanelState();
            this._settingsMenu.toggle();
        });
        const mode = new PopupMenu.PopupMenuItem(this._readColorMode());
        this._modeLabel = mode.label;
        mode.label.x_align = Clutter.ActorAlign.CENTER;
        mode.label.x_expand = true;
        // Do not emit PopupMenuItem's activate signal: it closes the menu.
        mode.activate = () => this._runAction('next-color-mode', button);
        this._settingsMenu.addMenuItem(mode);

        const row = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        row.add_style_class_name('sysi-font-row');
        row.add_child(new St.Widget({x_expand: true}));
        this._fontLabel = new St.Label({text: '13', y_align: Clutter.ActorAlign.CENTER});
        for (const [label, action] of [['−', 'font-smaller'], ['+', 'font-larger']]) {
            const control = new St.Button({label, style_class: 'sysi-font-control', can_focus: true, accessible_name: action === 'font-smaller' ? 'Decrease font size' : 'Increase font size'});
            control.connect('clicked', () => this._runAction(action, button));
            row.add_child(control);
            if (action === 'font-smaller')
                row.add_child(this._fontLabel);
        }
        row.add_child(new St.Widget({x_expand: true}));
        this._settingsMenu.addMenuItem(row);
        const lock = new PopupMenu.PopupMenuItem('lock');
        this._lockLabel = lock.label;
        lock.label.x_align = Clutter.ActorAlign.CENTER;
        lock.label.x_expand = true;
        lock.connect('activate', () => this._runAction('toggle-lock', button));
        this._settingsMenu.addMenuItem(lock);
        const quit = new PopupMenu.PopupMenuItem('quit');
        quit.label.x_align = Clutter.ActorAlign.CENTER;
        quit.label.x_expand = true;
        quit.connect('activate', () => this._runAction('quit', button));
        this._settingsMenu.addMenuItem(quit);
    }

    _buildPanelButton(label) {
        const button = new St.Button({
            style_class: 'sysi-panel-action',
            reactive: true,
            can_focus: true,
            track_hover: true,
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        // Set the font on the label too so it measures with the same face it
        // paints in; inheriting it makes Shell ellipsize otherwise-wide words.
        const text = new St.Label({
            text: label,
            y_align: Clutter.ActorAlign.CENTER,
            style: 'font-family: Noto Sans, sans-serif; font-size: 11px; font-weight: 500; text-shadow: none;',
        });
        button.add_child(text);
        this._strip.add_child(button);
        return button;
    }

    _addAction(label, action) {
        const button = this._buildPanelButton(label);
        button.connect('clicked', () => this._runAction(action, button));
        return button;
    }

    _readColorMode() {
        try {
            const path = GLib.build_filenamev([
                GLib.get_user_config_dir(),
                'sysi',
                'state.json',
            ]);
            const [ok, contents] = GLib.file_get_contents(path);
            const mode = ok
                ? JSON.parse(new TextDecoder().decode(contents))?.settings?.color_mode
                : null;
            return ['auto', 'light', 'dark', 'invert'].includes(mode) ? mode : 'auto';
        } catch (_) {
            return 'auto';
        }
    }

    // The overlay cannot work out where this button is on its own. The panel is
    // the compositor's own surface, so while the pointer is over it the X server
    // sees nothing — asking it returns wherever the mouse last crossed an X
    // window, which is what sent widgets off to the far side of the screen
    // instead of opening them under the button that asked. Send the button's
    // own place on the stage, which is already in the logical coordinates the
    // overlay lays its widgets out in.
    _runAction(action, button) {
        const argv = ['sysi', '--panel-action', action];
        const anchor = this._anchorOf(button);
        if (anchor)
            argv.push('--at', anchor);
        try {
            GLib.spawn_async(null, argv, null, GLib.SpawnFlags.SEARCH_PATH, null);
        } catch (error) {
            logError(error, `Sysi panel action ${action} failed`);
        }
    }

    // The middle of the button's bottom edge: the overlay centres the widget on
    // it and drops it clear of the panel.
    _anchorOf(button) {
        try {
            const [x, y] = button.get_transformed_position();
            const [width, height] = button.get_transformed_size();
            if (![x, y, width, height].every(Number.isFinite))
                return null;
            return `${Math.round(x + width / 2)},${Math.round(y + height)}`;
        } catch (error) {
            logError(error, 'Sysi panel gear could not locate its button');
            return null;
        }
    }

    _readPid() {
        try {
            const [ok, contents] = GLib.file_get_contents(this._pidFile.get_path());
            if (!ok)
                return 0;
            return Number(new TextDecoder().decode(contents).trim()) || 0;
        } catch (_) {
            return 0;
        }
    }

    _syncVisibility() {
        const running = this._readPid() > 0;
        this._indicator.visible = running;
        if (!running) {
            this._strip.visible = false;
            this._settingsMenu?.close();
        }
    }

    // `<editing|locked> <auto|light|dark|invert> <font-size>`, written by Sysi whenever
    // one changes and removed when it exits. With no file to read — Sysi is not
    // running — labels fall back to saved settings or defaults.
    _readPanelState() {
        try {
            const [ok, contents] = GLib.file_get_contents(this._panelStateFile.get_path());
            if (!ok)
                return [null, null, null];
            const [interaction, mode, fontSize] =
                new TextDecoder().decode(contents).trim().split(/\s+/);
            return [
                interaction === 'locked' || interaction === 'editing' ? interaction : null,
                ['auto', 'light', 'dark', 'invert'].includes(mode) ? mode : null,
                Math.min(26, Math.max(8, Number(fontSize) || 13)),
            ];
        } catch (_) {
            return [null, null, null];
        }
    }

    _syncPanelState() {
        if (!this._panelStateFile)
            return;
        const [interaction, mode, fontSize] = this._readPanelState();
        if (this._fontLabel && fontSize !== null)
            this._fontLabel.text = String(fontSize);
        if (this._lockLabel)
            this._lockLabel.text = interaction === 'locked' ? 'unlock' : 'lock';
        if (this._modeLabel)
            this._modeLabel.text = mode ?? this._readColorMode();
    }

    _queueAutoColorSampling() {
        if (!this._autoColorRequestFile || Main.layoutManager._startingUp)
            return;
        if (this._autoColorSampling) {
            this._autoColorPending = true;
            return;
        }
        this._autoColorSampling = true;
        const generation = this._autoColorGeneration;
        this._sampleAutoColors(generation)
            .catch(error => logError(error, 'Sysi auto-colour sampling failed'))
            .finally(() => {
                if (generation !== this._autoColorGeneration)
                    return;
                this._autoColorSampling = false;
                if (this._autoColorPending) {
                    this._autoColorPending = false;
                    this._queueAutoColorSampling();
                }
            });
    }

    async _sampleAutoColors(generation) {
        let raw;
        try {
            const [ok, contents] = GLib.file_get_contents(
                this._autoColorRequestFile.get_path(),
            );
            if (!ok)
                return;
            raw = new TextDecoder().decode(contents);
        } catch (_) {
            return;
        }

        const requests = raw.split('\n').flatMap(line => {
            const [key, geometry, kind] = line.trim().split('\t');
            const values = geometry?.split(',').map(Number) ?? [];
            if (!key || values.length !== 4 || !values.every(Number.isFinite))
                return [];
            const [x, y, width, height] = values;
            return width > 0 && height > 0
                ? [{key, x, y, width, height, kind: kind === 'invert' ? 'invert' : 'auto'}]
                : [];
        });
        if (requests.length === 0)
            return;

        // The INVERT captures all start inside one hidden window, so do them
        // before the AUTO picks rather than interleaving the two.
        await this._captureInvertRects(
            requests.filter(request => request.kind === 'invert'),
            generation,
        );

        const results = [];
        for (const request of requests.filter(request => request.kind === 'auto')) {
            const luminance = await this._sampleRectLuminance(request);
            if (Number.isFinite(luminance))
                results.push(`${request.key}\t${luminance.toFixed(6)}`);
        }
        if (generation !== this._autoColorGeneration)
            return;
        if (results.length === 0)
            return;
        this._writeCacheFile('auto-color-result', `${results.join('\n')}\n`);
    }

    _writeCacheFile(name, contents) {
        GLib.file_set_contents(
            GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', name]),
            contents,
        );
    }

    // Hand Sysi a picture of the desktop under each of its INVERT widgets, so
    // it can contrast with two different windows at once instead of picking one
    // foreground for the whole card.
    async _captureInvertRects(requests, generation) {
        if (requests.length === 0 || !this._invertDir)
            return;
        // Every grab is started with Sysi's own window invisible, so the
        // picture holds the desktop and not Sysi's own glyphs — sampling those
        // would feed the widget's colours straight back into the decision.
        const captures = this._withSysiHidden(() => requests.flatMap(request => {
            const rect = this._clampToMonitor(request);
            if (!rect)
                return [];
            const file = Gio.File.new_for_path(GLib.build_filenamev([
                this._invertDir,
                this._invertFileName(request.key),
            ]));
            let stream = null;
            try {
                // REPLACE_DESTINATION writes a temporary and renames on close,
                // so Sysi never opens a half-written picture.
                stream = file.replace(
                    null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
                // One Shell.Screenshot per grab: each owns the single image
                // buffer its own capture painted into.
                const done = new Shell.Screenshot().screenshot_area(
                    rect.x, rect.y, rect.width, rect.height, stream);
                return [{request, file, stream, done}];
            } catch (error) {
                // The grab never started, so nothing downstream will ever
                // close the stream that was opened for it.
                this._closeQuietly(stream);
                logError(error, 'Sysi could not start an invert capture');
                return [];
            }
        }));

        const index = [];
        for (const capture of captures) {
            try {
                // Only the PNG encoding is still outstanding here; the pixels
                // were painted synchronously while the window was hidden.
                const [area] = await capture.done;
                index.push([
                    capture.request.key,
                    `${area.x},${area.y},${area.width},${area.height}`,
                    capture.file.get_path(),
                ].join('\t'));
            } catch (error) {
                logError(error, 'Sysi could not finish an invert capture');
            } finally {
                // The rename onto the real name only happens on close, and an
                // unclosed stream holds a descriptor inside the compositor for
                // as long as it lives.
                this._closeQuietly(capture.stream);
            }
        }
        // Every grab has finished, so anything else in there belongs to a
        // widget that has left INVERT or stopped existing. The pictures sit on
        // a tmpfs, which is memory.
        this._pruneInvertCaptures(new Set(
            requests.map(request => this._invertFileName(request.key))));
        if (generation !== this._autoColorGeneration || index.length === 0)
            return;
        this._writeCacheFile('invert-result', `${index.join('\n')}\n`);
    }

    _invertFileName(key) {
        return `${key.replace(/[^\w-]/g, '_')}.png`;
    }

    _closeQuietly(stream) {
        try {
            stream?.close(null);
        } catch (_) {
            // Already closed, or the write failed; either way there is nothing
            // left to do with it.
        }
    }

    _pruneInvertCaptures(keep) {
        const directory = Gio.File.new_for_path(this._invertDir);
        let children;
        try {
            children = directory.enumerate_children(
                'standard::name', Gio.FileQueryInfoFlags.NONE, null);
        } catch (_) {
            return;
        }
        try {
            let info;
            while ((info = children.next_file(null)) !== null) {
                const name = info.get_name();
                if (keep.has(name))
                    continue;
                try {
                    directory.get_child(name).delete(null);
                } catch (_) {
                    // Something else removed it first.
                }
            }
        } finally {
            this._closeQuietly(children);
        }
    }

    // Run `fn` with every Sysi window painted at zero opacity. Clutter skips a
    // fully transparent actor, and Shell.Screenshot paints the stage inside the
    // call rather than on a later frame, so a grab started here sees the
    // desktop without Sysi and no frame is ever shown in this state. `fn` must
    // not await: opacity is restored the moment it returns.
    _withSysiHidden(fn) {
        const pid = this._readPid();
        const actors = (global.get_window_actors?.() ??
            global.compositor.get_window_actors()).filter(actor => {
            const window = actor.get_meta_window?.();
            if (!window)
                return false;
            return (pid > 0 && window.get_pid() === pid) ||
                window.get_wm_class()?.toLowerCase() === 'sysi';
        });
        const opacities = actors.map(actor => actor.opacity);
        actors.forEach(actor => (actor.opacity = 0));
        try {
            return fn();
        } finally {
            actors.forEach((actor, index) => (actor.opacity = opacities[index]));
        }
    }

    // Painting a rectangle that reaches past every monitor fails the grab, so
    // trim the widget to the monitor holding its middle.
    _clampToMonitor({x, y, width, height}) {
        const centreX = x + width / 2;
        const centreY = y + height / 2;
        const monitor = Main.layoutManager.monitors.find(candidate =>
            centreX >= candidate.x && centreY >= candidate.y &&
            centreX < candidate.x + candidate.width &&
            centreY < candidate.y + candidate.height);
        if (!monitor)
            return null;
        const left = Math.max(x, monitor.x);
        const top = Math.max(y, monitor.y);
        const right = Math.min(x + width, monitor.x + monitor.width);
        const bottom = Math.min(y + height, monitor.y + monitor.height);
        return right > left && bottom > top
            ? {x: left, y: top, width: right - left, height: bottom - top}
            : null;
    }

    async _sampleRectLuminance({x, y, width, height}) {
        // Only pixels that a monitor really shows can be painted to a buffer;
        // a point in the gap between monitors would fail the grab and crash
        // the shell inside pick_color_finish.
        const monitors = Main.layoutManager.monitors;
        const shown = ([px, py]) => monitors.some(monitor =>
            px >= monitor.x && py >= monitor.y &&
            px < monitor.x + monitor.width && py < monitor.y + monitor.height);
        // Read just outside the transparent widget. That sees the same nearby
        // browser/wallpaper without accidentally sampling Sysi's own glyphs.
        const xs = [0.2, 0.5, 0.8].map(fraction => Math.round(x + width * fraction));
        const above = Math.round(y - 3);
        const below = Math.round(y + height + 3);
        const points = [
            ...xs.map(px => [px, above]),
            ...xs.map(px => [px, below]),
        ].filter(shown);
        if (points.length === 0)
            return null;
        // One Shell.Screenshot, one pick at a time: each pick overwrites the
        // object's single image buffer, so they must not overlap.
        const screenshot = new Shell.Screenshot();
        const samples = [];
        for (const [px, py] of points) {
            try {
                const [color] = await screenshot.pick_color(px, py);
                samples.push(this._relativeLuminance(color.red, color.green, color.blue));
            } catch (_) {
                // A failed pick leaves this point out of the median.
            }
        }
        samples.sort((a, b) => a - b);
        return samples.length > 0 ? samples[Math.floor(samples.length / 2)] : null;
    }

    _relativeLuminance(red, green, blue) {
        const linear = channel => {
            const value = channel / 255;
            return value <= 0.04045
                ? value / 12.92
                : ((value + 0.055) / 1.055) ** 2.4;
        };
        return 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue);
    }
}
