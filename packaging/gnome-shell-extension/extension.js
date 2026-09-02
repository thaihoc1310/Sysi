import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';

const UUID = 'sysi-panel@thaihoc';

Gio._promisify(Shell.Screenshot.prototype, 'pick_color');

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
        this._mode = this._addAction(this._readColorMode(), 'next-color-mode');
        this._lock = this._addAction('lock', 'toggle-lock');
        this._addAction('+ note', 'new-note');
        this._addAction('history', 'toggle-history');
        this._addAction('dictionary', 'toggle-translate');
        this._addAction('quit', 'quit');

        this._gear.connect('clicked', () => {
            this._syncPanelState();
            this._strip.visible = !this._strip.visible;
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
        this._autoColorRequestFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'auto-color-request']),
        );
        if (!this._autoColorRequestFile.query_exists(null))
            GLib.file_set_contents(this._autoColorRequestFile.get_path(), '');
        this._autoColorGeneration = 1;
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
        this._indicator?.destroy();
        this._indicator = null;
        this._content = null;
        this._strip = null;
        this._gear = null;
        this._system = null;
        this._timer = null;
        this._mode = null;
        this._modeLabel = null;
        this._lock = null;
        this._lockLabel = null;
        this._pidFile = null;
        this._panelStateFile = null;
    }

    _addAction(label, action) {
        const button = new St.Button({
            style_class: 'sysi-panel-action',
            reactive: true,
            can_focus: true,
            track_hover: true,
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        // The font is set here so the label measures itself with the same face
        // it is painted in — inheriting it left every word ellipsized down to a
        // couple of characters. Colour is deliberately left out: an inline
        // colour would outrank the stylesheet and the text would stay pale
        // against the inverted hover block.
        const text = new St.Label({
            text: label,
            y_align: Clutter.ActorAlign.CENTER,
            style: 'font-family: Noto Sans, sans-serif; font-size: 11px; font-weight: 500; text-shadow: none;',
        });
        button.add_child(text);
        button.connect('clicked', () => this._runAction(action, button));
        this._strip.add_child(button);
        if (action === 'toggle-lock')
            this._lockLabel = text;
        if (action === 'next-color-mode')
            this._modeLabel = text;
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
            return ['auto', 'light', 'dark'].includes(mode) ? mode : 'auto';
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

    _syncVisibility() {
        let running = false;
        try {
            const [ok, contents] = GLib.file_get_contents(this._pidFile.get_path());
            running = ok && new TextDecoder().decode(contents).trim().length > 0;
        } catch (_) {
            running = false;
        }
        this._indicator.visible = running;
        if (!running)
            this._strip.visible = false;
    }

    // `<editing|locked> <auto|light|dark>`, written by Sysi whenever either
    // changes and removed when it exits. With no file to read — Sysi is not
    // running — the labels fall back to what the saved settings say.
    _readPanelState() {
        try {
            const [ok, contents] = GLib.file_get_contents(this._panelStateFile.get_path());
            if (!ok)
                return [null, null];
            const [interaction, mode] = new TextDecoder().decode(contents).trim().split(/\s+/);
            return [
                interaction === 'locked' || interaction === 'editing' ? interaction : null,
                ['auto', 'light', 'dark'].includes(mode) ? mode : null,
            ];
        } catch (_) {
            return [null, null];
        }
    }

    _syncPanelState() {
        if (!this._panelStateFile)
            return;
        const [interaction, mode] = this._readPanelState();
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
            const [key, geometry] = line.trim().split('\t');
            const values = geometry?.split(',').map(Number) ?? [];
            if (!key || values.length !== 4 || !values.every(Number.isFinite))
                return [];
            const [x, y, width, height] = values;
            return width > 0 && height > 0 ? [{key, x, y, width, height}] : [];
        });
        const results = [];
        for (const request of requests) {
            const luminance = await this._sampleRectLuminance(request);
            if (Number.isFinite(luminance))
                results.push(`${request.key}\t${luminance.toFixed(6)}`);
        }
        if (generation !== this._autoColorGeneration)
            return;
        const path = GLib.build_filenamev([
            GLib.get_user_cache_dir(),
            'sysi',
            'auto-color-result',
        ]);
        GLib.file_set_contents(path, `${results.join('\n')}\n`);
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
