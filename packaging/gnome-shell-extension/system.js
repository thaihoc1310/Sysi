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
// reading fits is judged with every value at two digits, and one that would
// not fit cannot be turned on; another has to be turned off first.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Pango from 'gi://Pango';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import {glassMenu} from './glass.js';

// Clear space between the last reading and the clock.
const CLOCK_GAP = 2;
// The least clear space the row may ever leave the clock, however far a value
// has outgrown the two digits it was reckoned at. The clock's own padding
// keeps its words clear of the row even at none.
const CLOCK_CLEAR = 0;
// Between a group and the hairline on either side of it.
const GROUP_GAP = 8;
// Between two devices of one group.
const DEVICE_GAP = 8;
// Between a caption and its first value, and between two values.
const CAPTION_GAP = 5;
const VALUE_GAP = 7;
const HAIRLINE = 1;
// How long a click's choice is trusted over what the file says. Sysi writes
// the file every couple of seconds, and one written just before the click
// arrived would otherwise put a reading back the way it was for a moment.
const PENDING_MS = 3000;
// How many measured widths are kept before they are all forgotten.
const WIDTHS_KEPT = 512;

// A value as the bar shows it, in plain text. Not Pango markup: a kept label
// given new markup kept the old spans' byte offsets, which split a
// three-byte arrow and drew broken glyphs, and laid itself out a third too
// wide. The arrows of a network rate sit in labels of their own instead,
// small and faint like the captions, so the two numbers carry the reading.
function valueBox() {
    return new St.BoxLayout({style_class: 'sysi-system-value-box', y_align: Clutter.ActorAlign.CENTER});
}

function setValue(box, text) {
    text = String(text ?? '');
    if (box._sysiText === text)
        return;
    box._sysiText = text;
    const tokens = text.split(/([↓↑])/)
        .map(part => part.trim())
        .filter(Boolean);
    const labels = box.get_children();
    tokens.forEach((token, index) => {
        let label = labels[index];
        if (!label) {
            label = whole(new St.Label({y_align: Clutter.ActorAlign.CENTER}));
            box.add_child(label);
        }
        const arrow = token === '↓' || token === '↑';
        label.style_class = arrow
            ? index > 0 ? 'sysi-system-arrow sysi-system-arrow-apart' : 'sysi-system-arrow'
            : 'sysi-system-value';
        if (label.text !== token)
            label.text = token;
        label.visible = true;
    });
    for (const label of labels.slice(tokens.length))
        label.visible = false;
}

// A caption or value is always shown whole: never cut to an ellipsis, even
// if a rounding pixel leaves it a hair wider than the room it was given.
function whole(label) {
    label.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
    return label;
}

// Give a label the width worked out for it. What a kept label reports of
// itself cannot be trusted: after its text changes it may answer in the
// panel's default font, a third wider, which widened the row and had the
// panel squeeze its captions. Laid out to widths measured on fresh labels,
// the row is exactly as wide as it was reckoned.
function setWidth(label, width) {
    if (label._sysiWidth !== width) {
        label._sysiWidth = width;
        label.width = width;
    }
}

// Put a child at a place in its parent, if it is not there already.
function place(parent, child, index) {
    if (parent.get_child_at_index(index) !== child)
        parent.set_child_at_index(child, index);
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
        // The box and each label take their style in their own time; a width
        // measured before the last of them arrived is in the wrong font.
        this._forgetWidthsOnStyle(this._probe.box);
        this._forgetWidthsOnStyle(this._probe.caption);

        this._menu = new PopupMenu.PopupMenu(button, 0.5, St.Side.TOP);
        this._menu.actor.add_style_class_name('sysi-settings-menu');
        this._menu.actor.add_style_class_name('sysi-system-menu');
        glassMenu(this._menu);
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
    // little short of the panel's centre box, the clock's. That is where the
    // panel ends the left box it lends this row: run past it, even into the
    // clock button's empty padding, and the panel squeezes every caption in
    // the row to an ellipsis.
    _room() {
        const clock = Main.panel._centerBox ?? Main.panel.statusArea.dateMenu;
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
        whole(label);
        const values = new St.BoxLayout({style: `spacing: ${VALUE_GAP}px;`, y_align: Clutter.ActorAlign.CENTER});
        box.add_child(label);
        box.add_child(values);
        return {box, caption: label, values, cells: []};
    }

    _value() {
        return valueBox();
    }

    // How wide a caption or a value is, measured on a label made for the
    // purpose. A label that is kept and given new text could answer with a
    // width laid out in the panel's default font rather than its own (33px
    // for a 25px "88°C"), which left a reading its own slack before the next
    // hairline. A new one, styled before it is asked, always answers true.
    _measureNow(kind, text) {
        const label = kind === 'caption'
            ? whole(new St.Label({style_class: 'sysi-system-caption', text}))
            : this._value();
        this._probe.values.add_child(label);
        if (kind !== 'caption')
            setValue(label, text);
        label.ensure_style();
        const texts = kind === 'caption' ? [label] : label.get_children();
        for (const text of texts)
            text.ensure_style();
        // A label that has never been shown lays its text out at 1x; on the
        // panel, at 2x, the same text rounds up to as much as a pixel wider.
        // Counting that pixel for each label keeps the reckoning from ever
        // falling short: a row reckoned a few pixels narrow was squeezed by
        // the panel to ellipses.
        const width = Math.ceil(label.get_preferred_width(-1)[1]) + texts.length;
        label.destroy();
        return width;
    }

    // The same, remembered. Values come round again and again (45°C, 12%),
    // so a sample mostly costs no new actors; the few hundred a day of
    // readings brings are forgotten together and measured again as seen.
    _text(kind, text) {
        const key = `${kind}\t${text}`;
        const known = this._widths.get(key);
        if (known)
            return known;
        const width = this._measureNow(kind, text);
        // Nothing to measure with while the panel row is off the stage.
        if (width > 0 && this._probe.box.mapped) {
            if (this._widths.size >= WIDTHS_KEPT)
                this._widths.clear();
            this._widths.set(key, width);
        }
        return width;
    }

    // A theme or text-scale change restyles the probe: every width is
    // measured again.
    _forgetWidthsOnStyle(actor) {
        actor.connect('style-changed', () => {
            this._widths.clear();
            this._renderLater();
        });
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
        // Each value is reckoned at the widest it can ever be (↓888M ↑888M,
        // 888W), so a group that is let in never has to step out again when
        // a download or the draw climbs. The row itself hugs what it shows.
        const reckoned = cell => cell.widest;
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
        // A device Sysi no longer lists (PWR once unplugged turns BAT, a
        // drive unmounted) would otherwise stay on the row as it last was.
        const listed = new Set(this._data.groups.flatMap(group =>
            group.devices.map(device => `${group.key}\t${device.label}`)));
        for (const [key, device] of this._devices) {
            if (!listed.has(key)) {
                device.box.destroy();
                this._devices.delete(key);
            }
        }
        const shown = new Map(layout.map(entry => [entry.group.key, entry]));
        let first = true;
        // The row's width, added up from the widths every label is given
        // below: it is laid out to exactly these, so this is what it takes.
        let width = 0;
        this._data.groups.forEach((group, index) => {
            // In the order Sysi lists them. Actors are made as groups first
            // appear, and a group a newer Sysi added (power, after a file
            // from an older one) would otherwise land at the end of the row.
            const actors = this._group(group.key);
            place(this._readout, actors.hairline, 2 * index);
            place(this._readout, actors.box, 2 * index + 1);
        });
        for (const group of this._data.groups) {
            const entry = shown.get(group.key);
            const actors = this._group(group.key);
            actors.box.visible = Boolean(entry);
            actors.hairline.visible = Boolean(entry) && !first;
            if (!entry)
                continue;
            width += first ? 0 : 2 * GROUP_GAP + HAIRLINE;
            first = false;
            let devices = 0;
            group.devices.forEach((device, index) => {
                place(actors.box, this._device(group.key, device.label).box, index);
            });
            for (const device of group.devices) {
                const actors = this._device(group.key, device.label);
                const placed = entry.devices.find(shown => shown.device.label === device.label);
                actors.box.visible = Boolean(placed);
                if (!placed)
                    continue;
                const captionWidth = this._text('caption', device.label);
                setWidth(actors.caption, captionWidth);
                width += (devices++ ? DEVICE_GAP : 0) + captionWidth + CAPTION_GAP;
                placed.cells.forEach((cell, index) => {
                    let label = actors.cells[index];
                    if (!label) {
                        label = actors.cells[index] = this._value();
                        actors.values.add_child(label);
                    }
                    label.visible = true;
                    // Room for two digits keeps a load or a temperature still
                    // as it goes from 9 to 10. A used/total value has no such
                    // floor (its usual is its widest): its length barely
                    // changes, and a floor sized for 888G left 6.6G/14G a gap.
                    const least = cell.usual && cell.usual !== cell.widest
                        ? this._text('value', cell.usual)
                        : 0;
                    const text = cell.value ?? '–';
                    const valueWidth = Math.max(least, this._text('value', text));
                    setValue(label, text);
                    setWidth(label, valueWidth);
                    width += (index ? VALUE_GAP : 0) + valueWidth;
                });
                for (const label of actors.cells.slice(placed.cells.length))
                    label.visible = false;
            }
        }
        // Nothing outgrows its reckoning, but a rounding pixel or a font the
        // probe did not see could. Rather than let the row run into the
        // clock, the last group steps out until it shrinks back.
        const last = this._groups.get(layout[layout.length - 1].group.key);
        if (layout.length > 1 && width > this._room() + CLOCK_GAP - CLOCK_CLEAR) {
            last.box.visible = false;
            last.hairline.visible = false;
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
