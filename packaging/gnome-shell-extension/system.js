// SYSTEM in the top bar.
//
// Sysi samples the machine and writes what the bar should show to
// $XDG_RUNTIME_DIR/sysi/system.json (see src/panel_system.rs). This lays
// those readings out beside the gear, and gives the strip's SYSTEM button a
// menu to switch them: one switch for the whole row, and a chip per reading.
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
const CHIPS_PER_ROW = 2;

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
        this._chips = new Map();
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
        const toggle = new PopupMenu.PopupSwitchMenuItem('show in top bar', false);
        toggle.label.x_expand = true;
        // Not PopupSwitchMenuItem's own activate: that closes the menu, and
        // the switch is set from what Sysi publishes, not from this click.
        toggle.activate = () => this._runAction('toggle-system', this._button);
        this._menu.addMenuItem(toggle);
        this._toggle = toggle;

        const section = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        section.add_style_class_name('sysi-system-chips-item');
        this._grid = new St.BoxLayout({vertical: true, style_class: 'sysi-system-chips', x_expand: true});
        section.add_child(this._grid);
        this._menu.addMenuItem(section);

        const hint = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        this._hint = new St.Label({style_class: 'sysi-system-hint', x_expand: true});
        this._hint.clutter_text.line_wrap = true;
        hint.add_child(this._hint);
        this._menu.addMenuItem(hint);
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
        const known = this._widths.get(metric.key);
        if (known)
            return known;
        this._probe.caption.text = metric.label;
        this._probe.value.text = metric.widest;
        const width = Math.ceil(this._probe.box.get_preferred_width(-1)[1]);
        // Nothing to measure with while the panel row is off the stage.
        if (width > 0 && this._probe.box.mapped)
            this._widths.set(metric.key, width);
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
            if (!metric.on || !metric.available)
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
            item.value.text = metric.value ?? '–';
        }
        this._readout.visible = Boolean(this._data.on) && !this._stripOpen && shown.length > 0;
        this._renderMenu(shown, fits);
    }

    _renderMenu(shown, fits) {
        this._toggle.setToggleState(Boolean(this._data.on));
        const offered = this._data.metrics.filter(metric => metric.available);
        let blocked = false;
        this._grid.destroy_all_children();
        this._chips.clear();
        let line = null;
        offered.forEach((metric, index) => {
            if (index % CHIPS_PER_ROW === 0) {
                line = new St.BoxLayout({style_class: 'sysi-system-chip-row', x_expand: true});
                // Two even columns, whatever each name's length.
                line.layout_manager.homogeneous = true;
                this._grid.add_child(line);
            }
            const on = shown.includes(metric.key);
            const full = !on && !fits(metric);
            blocked ||= full;
            const chip = new St.Button({
                label: metric.name,
                style_class: 'sysi-system-chip',
                can_focus: !full,
                reactive: !full,
                x_expand: true,
                accessible_name: `${metric.name}: ${on ? 'shown' : 'hidden'}`,
            });
            if (on)
                chip.add_style_pseudo_class('checked');
            if (full)
                chip.add_style_class_name('sysi-system-chip-full');
            chip.connect('clicked', () => this._runAction(`system-metric:${metric.key}`, this._button));
            line.add_child(chip);
            this._chips.set(metric.key, chip);
        });
        this._grid.opacity = this._data.on ? 255 : 128;
        this._hint.text = blocked
            ? 'The top bar is full up to the clock. Turn one off to add another.'
            : 'Tap a reading to show or hide it.';
    }
}
