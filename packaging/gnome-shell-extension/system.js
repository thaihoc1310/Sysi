// SYSTEM in the top bar.
//
// Sysi samples the machine and writes what the bar should show to
// $XDG_RUNTIME_DIR/sysi/system.json (see src/panel_system.rs). This lays it
// out beside the gear, in groups split by a hairline, with a caption per
// device:
//
//   CPU 13% 56°C | RAM 48%  SWAP 1% | NVI 12% 45°C  AMD 38°C | SAM 13% 42°C
//
// and gives the strip's SYSTEM button a menu in the style of settings:
// enable or disable the row, percentages or used/total, then one line per
// reading, bright while it is on and faint while it is off.
//
// Everything sits at its natural width with even gaps, like a flex row. A
// value is never narrower than two digits of itself, so the row only moves
// when one grows a third. The row stops short of the clock: whether the next
// reading fits is judged with every value at two digits (the network rate at
// its widest), and one that would not fit cannot be turned on; another has to
// be turned off first.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

// Clear space between the last reading and the clock.
const CLOCK_GAP = 24;
// Between a group and the hairline on either side of it.
const GROUP_GAP = 10;
// Between two devices of one group.
const DEVICE_GAP = 12;
// Between a caption and its first value, and between two values.
const CAPTION_GAP = 5;
const VALUE_GAP = 7;
const HAIRLINE = 1;
// How long a click's choice is trusted over what the file says. Sysi writes
// the file every couple of seconds, and one written just before the click
// arrived would otherwise put a reading back the way it was for a moment.
const PENDING_MS = 3000;

// A value as the bar shows it. The arrows of a network rate are drawn small
// and faint, like the captions, so the two numbers carry the reading.
function setValue(label, text) {
    const escaped = GLib.markup_escape_text(String(text ?? ''), -1);
    const markup = escaped
        .replace(/([↓↑])/g, '<span alpha="55%" size="85%">$1</span> ')
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
        this._groups = new Map();
        this._devices = new Map();
        this._rows = new Map();
        this._pending = new Map();
        this._stripOpen = false;

        this._readout = new St.BoxLayout({
            style_class: 'sysi-system-readout',
            style: `spacing: ${GROUP_GAP}px;`,
            y_align: Clutter.ActorAlign.CENTER,
            visible: false,
        });
        row.add_child(this._readout);
        // Where every device is measured. A hidden actor has no style, and the
        // devices that are off, or all of them while the strip covers the row,
        // are hidden exactly when the menu asks whether one fits. This one is
        // always on the panel, and never seen.
        this._probe = this._reading('');
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
        button.connect('clicked', () => this._menu.toggle());
        // The menu is only brought up to date while it is open.
        this._menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._render();
        });

        this._file = Gio.File.new_for_path(GLib.build_filenamev([
            GLib.get_user_runtime_dir(), 'sysi', 'system.json',
        ]));
        try {
            this._monitor = this._file.monitor_file(Gio.FileMonitorFlags.NONE, null);
            // Once per write: a write arrives as several change events, and
            // only the last says the file is whole.
            this._monitor.connect('changed', (_monitor, _file, _other, event) => {
                if (event === Gio.FileMonitorEvent.CHANGES_DONE_HINT ||
                    event === Gio.FileMonitorEvent.CREATED)
                    this._reload();
            });
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
        this._amounts = this._menuItem('used/total', () => {
            this._choose('amounts', !this._isOn('amounts'));
        });
        this._menu.addMenuItem(this._amounts);
        this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._list = new PopupMenu.PopupMenuSection();
        this._menu.addMenuItem(this._list);
    }

    // A centred line like the settings menu's. Its activate is replaced: the
    // stock one closes the menu, and readings are picked several at a time.
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

    // Say which way something should go, rather than asking for a flip, and
    // show it that way at once.
    _choose(key, on) {
        this._pending.set(key, {on, until: GLib.get_monotonic_time() / 1000 + PENDING_MS});
        this._render();
        const state = on ? 'on' : 'off';
        const action = key === 'system' ? `system:${state}`
            : key === 'amounts' ? `system-amounts:${state}`
                : `system-metric:${key}:${state}`;
        this._runAction(action, this._button);
    }

    _published(key) {
        if (key === 'system')
            return Boolean(this._data?.on);
        if (key === 'amounts')
            return Boolean(this._data?.amounts);
        return Boolean(this._data?.metrics.find(metric => metric.key === key)?.on);
    }

    // What something is, as far as the user knows: their last click while
    // Sysi catches up, and what Sysi published after that.
    _isOn(key) {
        const published = this._published(key);
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
            if (!Array.isArray(data?.metrics) || !Array.isArray(data?.groups))
                return;
            this._data = data;
        } catch (_) {
            // Missing, or not written by this version of Sysi yet.
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

    _reading(caption) {
        const box = new St.BoxLayout({
            style: `spacing: ${CAPTION_GAP}px;`,
            y_align: Clutter.ActorAlign.CENTER,
        });
        const label = new St.Label({
            text: caption,
            style_class: 'sysi-system-caption',
            y_align: Clutter.ActorAlign.CENTER,
        });
        const values = new St.BoxLayout({style: `spacing: ${VALUE_GAP}px;`, y_align: Clutter.ActorAlign.CENTER});
        box.add_child(label);
        box.add_child(values);
        return {box, caption: label, values, cells: []};
    }

    _value() {
        return new St.Label({style_class: 'sysi-system-value', y_align: Clutter.ActorAlign.CENTER});
    }

    // How wide a caption or a value is, measured on the probe.
    _text(kind, text) {
        const key = `${kind}\t${text}`;
        const known = this._widths.get(key);
        if (known)
            return known;
        let label = this._probe.caption;
        if (kind === 'caption') {
            label.text = text;
        } else {
            label = this._probe.cells[0] ??= this._value();
            if (!label.get_parent())
                this._probe.values.add_child(label);
            setValue(label, text);
        }
        const width = Math.ceil(label.get_preferred_width(-1)[1]);
        // Nothing to measure with while the panel row is off the stage.
        if (width > 0 && this._probe.box.mapped)
            this._widths.set(key, width);
        return width;
    }

    // A device's width with these values in it.
    _measure(caption, values) {
        return this._text('caption', caption) + CAPTION_GAP +
            values.reduce((sum, value) => sum + this._text('value', value), 0) +
            VALUE_GAP * (values.length - 1);
    }

    // The groups the row would show with these readings on, fitted into the
    // room before the clock in order. A group that does not fit is left out
    // whole, rather than drawn into the clock.
    _layout(isOn) {
        // What each value is given in the reckoning: two digits of itself,
        // which is what the row shows but for a rare 100% or 100°C, and the
        // clock gap takes that digit. The network rate is the exception: it
        // runs from kilobytes to megabytes all day, and reckoned at its usual
        // width it would drop out of the row whenever a download started.
        const reckoned = cell => cell.metric === 'network'
            ? cell.widest
            : cell.usual ?? cell.widest;
        const room = this._room();
        const shown = [];
        let used = 0;
        for (const group of this._data.groups) {
            const devices = group.devices
                .map(device => ({device, cells: device.cells.filter(cell => isOn(cell.metric))}))
                .filter(({cells}) => cells.length > 0)
                .map(entry => ({
                    ...entry,
                    width: this._measure(entry.device.label, entry.cells.map(reckoned)),
                }));
            if (!devices.length)
                continue;
            const width = devices.reduce((sum, {width}) => sum + width, 0) +
                DEVICE_GAP * (devices.length - 1);
            const cost = width + (shown.length ? 2 * GROUP_GAP + HAIRLINE : 0);
            if (used + cost > room)
                continue;
            shown.push({group, devices, width});
            used += cost;
        }
        return shown;
    }

    // Where the row shows a reading: in any group that made it in.
    static _shows(layout, metric) {
        return layout.some(({devices}) => devices.some(({cells}) =>
            cells.some(cell => cell.metric === metric)));
    }

    // Whether turning a reading on would keep everything already shown and
    // show it too.
    _fits(layout, metric) {
        const next = this._layout(key => key === metric || this._isOn(key));
        const before = layout.map(({group}) => group.key);
        const after = next.map(({group}) => group.key);
        return before.every(key => after.includes(key)) && SystemPanel._shows(next, metric);
    }

    _group(key) {
        let group = this._groups.get(key);
        if (!group) {
            const hairline = new St.Widget({style_class: 'sysi-system-hairline', y_align: Clutter.ActorAlign.CENTER});
            const box = new St.BoxLayout({style: `spacing: ${DEVICE_GAP}px;`, y_align: Clutter.ActorAlign.CENTER});
            this._readout.add_child(hairline);
            this._readout.add_child(box);
            group = {hairline, box};
            this._groups.set(key, group);
        }
        return group;
    }

    _device(groupKey, label) {
        const key = `${groupKey}\t${label}`;
        let device = this._devices.get(key);
        if (!device) {
            device = this._reading(label);
            this._group(groupKey).box.add_child(device.box);
            this._devices.set(key, device);
        }
        return device;
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

    _render() {
        if (!this._readout || !this._data)
            return;
        const layout = this._layout(key => this._isOn(key));
        const visible = this._isOn('system') && !this._stripOpen && layout.length > 0;
        this._readout.visible = visible;
        if (this._menu.isOpen)
            this._renderMenu(layout);
        // A row out of sight is left as it is: it is laid out afresh the
        // moment it comes back (see setStripOpen).
        if (!visible)
            return;
        const shown = new Map(layout.map(entry => [entry.group.key, entry]));
        let first = true;
        for (const group of this._data.groups) {
            const entry = shown.get(group.key);
            const actors = this._group(group.key);
            actors.box.visible = Boolean(entry);
            actors.hairline.visible = Boolean(entry) && !first;
            if (entry)
                first = false;
            for (const device of group.devices) {
                const actors = this._device(group.key, device.label);
                const placed = entry?.devices.find(shown => shown.device.label === device.label);
                actors.box.visible = Boolean(placed);
                if (!placed)
                    continue;
                placed.cells.forEach((cell, index) => {
                    let label = actors.cells[index];
                    if (!label) {
                        label = actors.cells[index] = this._value();
                        actors.values.add_child(label);
                    }
                    label.visible = true;
                    // A file from an older Sysi has no `usual`.
                    const least = this._text('value', cell.usual ?? cell.widest);
                    if (label._sysiLeast !== least) {
                        label._sysiLeast = least;
                        label.style = `min-width: ${least}px;`;
                    }
                    setValue(label, cell.value ?? '–');
                });
                for (const label of actors.cells.slice(placed.cells.length))
                    label.visible = false;
            }
        }
    }

    _renderMenu(layout) {
        this._buildRows();
        const enabled = this._isOn('system');
        this._enable.label.text = enabled ? 'disable' : 'enable';
        // Says what a click switches to, the way lock / unlock does.
        this._amounts.label.text = this._isOn('amounts') ? 'percent' : 'used/total';
        for (const metric of this._data.metrics) {
            const row = this._rows.get(metric.key);
            if (!row)
                continue;
            const on = this._isOn(metric.key);
            // On, but squeezed out by the clock: say so, so it is not
            // mistaken for off.
            const squeezed = on && !SystemPanel._shows(layout, metric.key);
            const full = !on && !this._fits(layout, metric.key);
            const text = squeezed ? `${metric.name} · no room` : metric.name;
            if (row.label.text !== text)
                row.label.text = text;
            if (row.getSensitive() === full) {
                // A line that greys out under the pointer or the keyboard
                // would stay lit, looking picked, while another is hovered.
                if (full && (row.active || row.has_key_focus())) {
                    row.active = false;
                    this._menu.actor.grab_key_focus();
                }
                row.setSensitive(!full);
            }
            for (const [name, active] of [['sysi-system-on', on], ['sysi-system-full', full]]) {
                if (active)
                    row.add_style_class_name(name);
                else
                    row.remove_style_class_name(name);
            }
        }
        this._list.actor.opacity = enabled ? 255 : 110;
    }
}
