import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';

const UUID = 'sysi-panel@thaihoc';

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
            style_class: 'panel-status-menu-box',
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
            style_class: 'system-status-icon',
        }));
        this._content.add_child(this._gear);

        this._strip = new St.BoxLayout({
            style_class: 'panel-status-menu-box',
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
        this._addAction('trans', 'toggle-translate');
        this._addAction('quit', 'quit');

        this._gear.connect('clicked', () => {
            this._strip.visible = !this._strip.visible;
        });
        this._lock.connect('clicked', () => {
            this._lockLabel.text = this._lockLabel.text === 'lock' ? 'unlock' : 'lock';
        });
        this._mode.connect('clicked', () => {
            this._modeLabel.text = this._nextColorMode(this._modeLabel.text);
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
        this._syncVisibility();
    }

    disable() {
        this._pidMonitor?.cancel();
        this._pidMonitor = null;
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
    }

    _addAction(label, action) {
        // A rule stands between neighbours, never at the ends of the row.
        if (this._strip.get_n_children() > 0)
            this._addSeparator();
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
        button.connect('clicked', () => this._runAction(action));
        this._strip.add_child(button);
        if (action === 'toggle-lock')
            this._lockLabel = text;
        if (action === 'next-color-mode')
            this._modeLabel = text;
        return button;
    }

    _addSeparator() {
        const rules = new St.BoxLayout({
            style_class: 'sysi-panel-separator',
            y_expand: true,
            y_align: Clutter.ActorAlign.FILL,
        });
        for (let index = 0; index < 2; index++) {
            rules.add_child(new St.Widget({
                style_class: 'sysi-panel-rule',
                y_expand: true,
                y_align: Clutter.ActorAlign.FILL,
            }));
        }
        this._strip.add_child(rules);
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
            return ['light', 'gray', 'dark'].includes(mode) ? mode : 'gray';
        } catch (_) {
            return 'gray';
        }
    }

    _nextColorMode(current) {
        const modes = ['light', 'gray', 'dark'];
        return modes[(modes.indexOf(current) + 1) % modes.length];
    }

    _runAction(action) {
        try {
            GLib.spawn_async(
                null,
                ['sysi', '--panel-action', action],
                null,
                GLib.SpawnFlags.SEARCH_PATH,
                null,
            );
        } catch (error) {
            logError(error, `Sysi panel action ${action} failed`);
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
}
