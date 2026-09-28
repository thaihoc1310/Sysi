// SYSTEM in the top bar.
//
// Sysi samples the machine and writes what the bar should show to
// $XDG_RUNTIME_DIR/sysi/system.json (see src/panel_system.rs). This lays
// those readings out beside the gear, and gives the strip's SYSTEM button a
// menu in the style of settings: enable or disable the row, then one line per
// reading, bright while it is on and faint while it is off.
//
// The row never runs into the clock. Each reading has a fixed width, taken
// from the widest value it can show, so the row does not shuffle as digits
// change, and a reading that would not fit before the clock cannot be turned
// on: another has to be turned off first.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

// Clear space between the last reading and the clock.
const CLOCK_GAP = 24;
// How long a click's choice is trusted over what the file says. Sysi writes
// the file every couple of seconds, and one written just before the click
// arrived would otherwise put a reading back the way it was for a moment.
const PENDING_MS = 3000;

// A value as the bar shows it. The arrows of a network rate are drawn small
// and faint, like the captions, so the two numbers carry the reading.
function setValue(label, text) {
    const escaped = GLib.markup_escape_text(text, -1);
    const markup = escaped
        .replace(/([↓↑])/g, '<span alpha="55%" size="85%">$1</span>\u2009')
        .replace(/ (?=<span)/g, '  ');
    if (label._sysiMarkup === markup)
        return;
    label._sysiMarkup = markup;
    label.clutter_text.set_markup(markup);
}

export class SystemPanel {
    // `row` holds the readings; `button` is the strip's SYSTEM button;
    // `gear` is where the row starts from; `runAction` sends Sysi an action.
    constructor({row, button, gear, runAction}) {
        this._gear = gear;
        this._runAction = runAction;
        this._button = button;
        this._data = null;
        this._widths = new Map();
        this._items = new Map();
        this._rows = new Map();
        this._pending = new Map();
        this._stripOpen = false;

        this._readout = new St.BoxLayout({
            style_class: 'sysi-system-readout',
            y_align: Clutter.ActorAlign.CENTER,
            visible: false,
        });
        row.add_child(this._readout);
        // Where every reading is measured. A hidden actor has no style, and
        // the readings that are off, or all of them while the strip covers
        // the row, are hidden exactly when the menu asks whether one fits.
        // This one is always on the panel, and never seen.
        this._probe = this._reading('', '');
        this._probeBox = new St.Bin({
            child: this._probe.box,
            opacity: 0,
            width: 0,
            clip_to_allocation: true,
            reactive: false,
        });
        row.add_child(this._probeBox);
        this._probe.box.connect('style-changed', () => {
            this._widths.clear();
            this._renderLater();
        });

        this._menu = new PopupMenu.PopupMenu(button, 0.5, St.Side.TOP);
        this._menu.actor.add_style_class_name('sysi-settings-menu');
        this._menu.actor.add_style_class_name('sysi-system-menu');
        Main.uiGroup.add_child(this._menu.actor);
        this._menu.actor.hide();
        Main.panel.menuManager.addMenu(this._menu);
        this._buildMenu();
        button.connect('clicked', () => {
            this._render();
            this._menu.toggle();
        });

        this._file = Gio.File.new_for_path(GLib.build_filenamev([
            GLib.get_user_runtime_dir(), 'sysi', 'system.json',
        ]));
        try {
            this._monitor = this._file.monitor_file(Gio.FileMonitorFlags.NONE, null);
            this._monitor.connect('changed', () => this._reload());
        } catch (error) {
            logError(error, 'Sysi could not watch SYSTEM readings');
        }
        // Another monitor, or a new scale, moves the clock.
        this._monitorsId = Main.layoutManager.connect('monitors-changed', () => this._render());
        this._reload();
    }

    destroy() {
        if (this._renderId)
            GLib.source_remove(this._renderId);
        this._renderId = 0;
        this._monitor?.cancel();
        this._monitor = null;
        if (this._monitorsId)
            Main.layoutManager.disconnect(this._monitorsId);
        this._monitorsId = 0;
        this._menu?.destroy();
        this._menu = null;
        this._readout?.destroy();
        this._readout = null;
        this._probeBox?.destroy();
        this._probeBox = null;
    }

    // The strip and the readings share the space beside the gear: the strip
    // covers them while it is open, and they come back when it closes.
    setStripOpen(open) {
        this._stripOpen = open;
        if (!open)
            this._menu?.close();
        this._render();
    }

    close() {
        this._menu?.close();
    }

    _buildMenu() {
        this._enable = this._menuItem('enable', () => {
            this._choose('system', !this._isOn('system'));
        });
        this._menu.addMenuItem(this._enable);
        this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._list = new PopupMenu.PopupMenuSection();
        this._menu.addMenuItem(this._list);
    }

    // A centred line like the settings menu's. Its activate is replaced: the
    // stock one closes the menu, and a reading is picked several at a time.
    _menuItem(text, action) {
        const item = new PopupMenu.PopupMenuItem(text);
        item.label.x_align = Clutter.ActorAlign.CENTER;
        item.label.x_expand = true;
        item.activate = () => action();
        return item;
    }

    // One line per reading this machine has. Built once, then only restyled:
    // rebuilding them every time Sysi wrote the file swallowed the clicks that
    // landed while it happened.
    _buildRows() {
        const keys = this._data.metrics.filter(metric => metric.available).map(metric => metric.key);
        if (keys.join() === [...this._rows.keys()].join())
            return;
        this._list.removeAll();
        this._rows.clear();
        for (const key of keys) {
            const row = this._menuItem('', () => this._choose(key, !this._isOn(key)));
            row.add_style_class_name('sysi-system-row');
            this._list.addMenuItem(row);
            this._rows.set(key, row);
        }
    }

    // Say which way a reading should go, rather than asking for a flip, and
    // show it that way at once.
    _choose(key, on) {
        this._pending.set(key, {on, until: GLib.get_monotonic_time() / 1000 + PENDING_MS});
        this._render();
        const action = key === 'system'
            ? `system:${on ? 'on' : 'off'}`
            : `system-metric:${key}:${on ? 'on' : 'off'}`;
        this._runAction(action, this._button);
    }

    // What a reading is, as far as the user knows: their last click while
    // Sysi catches up, and what Sysi published after that.
    _isOn(key) {
        const published = key === 'system'
            ? Boolean(this._data?.on)
            : Boolean(this._data?.metrics.find(metric => metric.key === key)?.on);
        const pending = this._pending.get(key);
        if (!pending)
            return published;
        if (pending.on === published || GLib.get_monotonic_time() / 1000 > pending.until) {
            this._pending.delete(key);
            return published;
        }
        return pending.on;
    }

    _reload() {
        try {
            const [ok, contents] = GLib.file_get_contents(this._file.get_path());
            if (!ok)
                return;
            const data = JSON.parse(new TextDecoder().decode(contents));
            if (!Array.isArray(data?.metrics))
                return;
            this._data = data;
        } catch (_) {
            // Missing, or read halfway through a write: the next write
            // brings a whole one.
            return;
        }
        this._render();
    }

    // How much of the bar the readings may take: from beside the gear to a
    // little short of the clock.
    _room() {
        const clock = Main.panel.statusArea.dateMenu ?? Main.panel._centerBox;
        const [clockX] = clock.get_transformed_position();
        const [gearX] = this._gear.get_transformed_position();
        const start = gearX + this._gear.width + this._readout.get_theme_node().get_margin(St.Side.LEFT);
        return Math.max(0, clockX - start - CLOCK_GAP);
    }

    _item(metric) {
        let item = this._items.get(metric.key);
        if (item)
            return item;
        item = this._reading(metric.label, metric.widest);
        this._readout.add_child(item.box);
        this._items.set(metric.key, item);
        return item;
    }

    _reading(label, text) {
        const box = new St.BoxLayout({style_class: 'sysi-system-item', y_align: Clutter.ActorAlign.CENTER});
        const caption = new St.Label({
            text: label,
            style_class: 'sysi-system-caption',
            y_align: Clutter.ActorAlign.CENTER,
        });
        const value = new St.Label({
            text,
            style_class: 'sysi-system-value',
            y_align: Clutter.ActorAlign.CENTER,
        });
        box.add_child(caption);
        box.add_child(value);
        return {box, caption, value};
    }

    // A reading's width, set from the widest value it can take. Measured once
    // it has a style to measure with, which it only has on the stage.
    _width(metric) {
        // By what is measured, not by key: a reading gains width with every
        // GPU or drive it covers.
        const measured = `${metric.label}\t${metric.widest}`;
        const known = this._widths.get(measured);
        if (known)
            return known;
        this._probe.caption.text = metric.label;
        setValue(this._probe.value, metric.widest);
        const width = Math.ceil(this._probe.box.get_preferred_width(-1)[1]);
        // Nothing to measure with while the panel row is off the stage.
        if (width > 0 && this._probe.box.mapped)
            this._widths.set(measured, width);
        return width;
    }

    _renderLater() {
        if (this._renderId)
            return;
        this._renderId = GLib.idle_add(GLib.PRIORITY_DEFAULT, () => {
            this._renderId = 0;
            this._render();
            return GLib.SOURCE_REMOVE;
        });
    }

    _spacing() {
        return this._readout.get_theme_node().get_length('spacing');
    }

    // The readings that fit, in order, and whether each off one could join.
    _layout() {
        const room = this._room();
        const spacing = this._spacing();
        const shown = [];
        let used = 0;
        for (const metric of this._data.metrics) {
            if (!this._isOn(metric.key) || !metric.available)
                continue;
            const width = this._width(metric);
            const next = used + (shown.length ? spacing : 0) + width;
            // Kept out rather than drawn into the clock, should the room
            // shrink under readings chosen on a wider screen.
            if (next > room)
                continue;
            shown.push(metric.key);
            used = next;
        }
        const fits = metric =>
            used + (shown.length ? spacing : 0) + this._width(metric) <= room;
        return {shown, fits};
    }

    _render() {
        if (!this._readout || !this._data)
            return;
        const {shown, fits} = this._layout();
        for (const metric of this._data.metrics) {
            const item = this._item(metric);
            item.box.visible = shown.includes(metric.key);
            if (item.box.visible)
                item.box.width = this._width(metric);
            setValue(item.value, metric.value ?? '–');
        }
        this._readout.visible = this._isOn('system') && !this._stripOpen && shown.length > 0;
        this._renderMenu(shown, fits);
    }

    _renderMenu(shown, fits) {
        this._buildRows();
        const enabled = this._isOn('system');
        this._enable.label.text = enabled ? 'disable' : 'enable';
        for (const metric of this._data.metrics) {
            const row = this._rows.get(metric.key);
            if (!row)
                continue;
            const on = this._isOn(metric.key);
            // On, but squeezed out by the clock: say so, so it is not
            // mistaken for off.
            const squeezed = on && !shown.includes(metric.key);
            const full = !on && !fits(metric);
            row.label.text = squeezed ? `${metric.name} · no room` : metric.name;
            row.setSensitive(!full);
            for (const [name, active] of [
                ['sysi-system-on', on],
                ['sysi-system-full', full],
            ]) {
                if (active)
                    row.add_style_class_name(name);
                else
                    row.remove_style_class_name(name);
            }
        }
        this._list.actor.opacity = enabled ? 255 : 110;
    }
}
