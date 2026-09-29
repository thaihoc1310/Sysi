// The timer, in the top bar. The strip's `timer` button opens a menu of
// presets and a field that takes `25m`, `1h30`, `10:00` or `@17:30`; while it
// runs, a small pill after SYSTEM's readings shows a disk of what is left, in
// the manner of Time Timer, and the time. At zero it rings: a notification
// that stays until it is answered, the theme's alarm sound until then (for a
// minute at most), and the pill turns amber. A click on the pill answers it.

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as MessageTray from 'resource:///org/gnome/shell/ui/messageTray.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import {formatTime, parseDuration} from './duration.js';
import {glassMenu} from './glass.js';

const PRESETS = [1, 3, 5, 10, 15, 25, 45, 60];
const MAX_SECONDS = 24 * 3600;
// The alarm is six seconds long; it plays again after a breath, for as long
// as nobody answers, up to a minute.
const RING_EVERY_MS = 7000;
const RING_FOR_MS = 60000;
const RING_SOUND = 'alarm-clock-elapsed';
// The pill keeps one width per run, so SYSTEM's readings do not shuffle as
// the time counts down: one for minutes, one for hours.
const PILL_WIDTH = 60;
const PILL_WIDTH_LONG = 74;
const PILL_GAP = 8;

const now = () => GLib.get_real_time() / 1000;

// The countdown lives here, in the module, not in the panel: GNOME turns the
// extension off while the screen is locked, and a break taken behind the lock
// screen still has to end with its alarm. It counts against the wall clock,
// so a laptop suspended mid-run wakes up to the right time.
const countdown = {
    total: 0,
    endsAt: 0,
    left: 0,
    running: false,
    ringing: false,
    _endId: 0,
    _ringId: 0,
    _notification: null,
    _listeners: new Set(),

    get active() {
        return this.running || this.left > 0 || this.ringing;
    },

    remaining() {
        return this.running ? Math.max(0, this.endsAt - now()) / 1000 : this.left;
    },

    start(seconds) {
        this._silence();
        this.total = seconds;
        this.left = 0;
        this.endsAt = now() + seconds * 1000;
        this.running = true;
        this._schedule();
        this._changed();
    },

    toggle() {
        if (this.ringing || !this.active)
            return;
        if (this.running) {
            this.left = this.remaining();
            this.running = false;
            this._unschedule();
        } else {
            this.endsAt = now() + this.left * 1000;
            this.left = 0;
            this.running = true;
            this._schedule();
        }
        this._changed();
    },

    add(seconds) {
        if (this.ringing || !this.active)
            return;
        this.total += seconds;
        if (this.running) {
            this.endsAt += seconds * 1000;
            this._schedule();
        } else {
            this.left += seconds;
        }
        this._changed();
    },

    cancel() {
        this._silence();
        this._unschedule();
        this.total = this.left = this.endsAt = 0;
        this.running = false;
        this._changed();
    },

    // Whether the end has come; the panel asks every second, which catches a
    // wake from suspend before the timeout below does.
    check() {
        if (this.running && now() >= this.endsAt)
            this._ring();
    },

    subscribe(listener) {
        this._listeners.add(listener);
        return () => this._listeners.delete(listener);
    },

    _changed() {
        for (const listener of this._listeners)
            listener();
    },

    _schedule() {
        this._unschedule();
        const ms = Math.max(0, Math.ceil(this.endsAt - now()));
        this._endId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
            this._endId = 0;
            this.check();
            if (this.running)
                this._schedule();
            return GLib.SOURCE_REMOVE;
        });
    },

    _unschedule() {
        if (this._endId)
            GLib.source_remove(this._endId);
        this._endId = 0;
    },

    _ring() {
        this._unschedule();
        this.running = false;
        this.left = 0;
        this.ringing = true;
        this._notify();
        const player = global.display.get_sound_player();
        const started = now();
        player.play_from_theme(RING_SOUND, "Time's up", null);
        this._ringId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RING_EVERY_MS, () => {
            if (now() - started >= RING_FOR_MS) {
                this._ringId = 0;
                return GLib.SOURCE_REMOVE;
            }
            player.play_from_theme(RING_SOUND, "Time's up", null);
            return GLib.SOURCE_CONTINUE;
        });
        this._changed();
    },

    _notify() {
        const source = new MessageTray.Source({title: 'Sysi', iconName: 'alarm-symbolic'});
        Main.messageTray.add(source);
        // Critical: it stays up until answered, and comes through Do Not
        // Disturb, as an alarm should.
        const notification = new MessageTray.Notification({
            source,
            title: "Time's up",
            body: `${formatTime(this.total)} timer`,
            urgency: MessageTray.Urgency.CRITICAL,
        });
        // Answered from the notification, which then takes itself down:
        // let go of it first, or it would be destroyed twice.
        const answer = action => () => {
            this._notification = null;
            action();
        };
        notification.addAction('Dismiss', answer(() => this.cancel()));
        notification.addAction('+1 min', answer(() => this.start(60)));
        notification.connect('activated', answer(() => this.cancel()));
        // Closed without an answer is an answer too.
        notification.connect('destroy', () => {
            if (this._notification === notification) {
                this._notification = null;
                if (this.ringing)
                    this.cancel();
            }
        });
        this._notification = notification;
        source.addNotification(notification);
    },

    // Stop the ringing and take the notification down.
    _silence() {
        if (this._ringId)
            GLib.source_remove(this._ringId);
        this._ringId = 0;
        this.ringing = false;
        const notification = this._notification;
        this._notification = null;
        notification?.destroy();
    },
};

export class TimerPanel {
    // `row` is the panel row the pill joins, after SYSTEM's readings;
    // `button` is the strip's `timer`; `systemPanel` is told how much of the
    // row the pill takes.
    constructor({row, button, systemPanel}) {
        this._button = button;
        this._systemPanel = systemPanel;
        this._stripOpen = false;
        this._tickId = 0;

        this._pill = new St.Button({
            style_class: 'sysi-timer-pill',
            y_align: Clutter.ActorAlign.CENTER,
            reactive: true,
            can_focus: true,
            track_hover: true,
            visible: false,
        });
        const inside = new St.BoxLayout({style_class: 'sysi-timer-pill-box', x_align: Clutter.ActorAlign.CENTER});
        this._disk = new St.DrawingArea({style_class: 'sysi-timer-disk', y_align: Clutter.ActorAlign.CENTER});
        this._disk.connect('repaint', area => this._paintDisk(area));
        this._time = new St.Label({style_class: 'sysi-timer-time', y_align: Clutter.ActorAlign.CENTER});
        inside.add_child(this._disk);
        inside.add_child(this._time);
        this._pill.set_child(inside);
        this._pill.connect('clicked', () => {
            if (countdown.ringing)
                countdown.cancel();
            else
                this._open(this._pill);
        });
        row.add_child(this._pill);

        this._buildMenu();
        button.connect('clicked', () => this._open(button));

        this._unsubscribe = countdown.subscribe(() => this._render());
        // Rung while the extension was off (behind the lock screen, say).
        countdown.check();
        this._render();
    }

    destroy() {
        this._unsubscribe?.();
        this._unsubscribe = null;
        this._stopTicking();
        this._menu?.destroy();
        this._menu = null;
        this._pill?.destroy();
        this._pill = null;
        this._systemPanel?.setReserve(0);
    }

    // The strip covers the row it sits in, pill included.
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
        this._menu = new PopupMenu.PopupMenu(this._button, 0.5, St.Side.TOP);
        this._menu.actor.add_style_class_name('sysi-settings-menu');
        this._menu.actor.add_style_class_name('sysi-timer-menu');
        glassMenu(this._menu);
        Main.uiGroup.add_child(this._menu.actor);
        this._menu.actor.hide();
        Main.panel.menuManager.addMenu(this._menu);

        const body = new St.BoxLayout({style_class: 'sysi-timer-body', vertical: true});
        for (let row = 0; row < PRESETS.length / 4; row++) {
            const chips = new St.BoxLayout({style_class: 'sysi-timer-chips'});
            for (const minutes of PRESETS.slice(row * 4, row * 4 + 4)) {
                const chip = new St.Button({
                    style_class: 'sysi-timer-chip',
                    label: minutes < 60 ? String(minutes) : '1h',
                    can_focus: true,
                });
                chip.connect('clicked', () => this._start(minutes * 60));
                chips.add_child(chip);
            }
            body.add_child(chips);
        }
        this._entry = new St.Entry({
            style_class: 'sysi-timer-entry',
            hint_text: '25m, 1h30, 10:00, @17:30',
            can_focus: true,
            x_expand: true,
        });
        this._entry.clutter_text.connect('activate', () => {
            const seconds = parseDuration(this._entry.text);
            if (!seconds || seconds > MAX_SECONDS) {
                this._entry.add_style_class_name('sysi-timer-wrong');
                return;
            }
            this._start(seconds);
        });
        this._entry.clutter_text.connect('text-changed', () => {
            this._entry.remove_style_class_name('sysi-timer-wrong');
        });
        body.add_child(this._entry);

        this._controls = new St.BoxLayout({style_class: 'sysi-timer-controls'});
        const control = (label, action) => {
            const button = new St.Button({style_class: 'sysi-timer-control', label, can_focus: true});
            button.connect('clicked', action);
            this._controls.add_child(button);
            return button;
        };
        this._pause = control('pause', () => countdown.toggle());
        control('+1 min', () => countdown.add(60));
        control('cancel', () => {
            countdown.cancel();
            this._menu.close();
        });
        body.add_child(this._controls);
        this._menu.box.add_child(body);

        this._menu.connect('open-state-changed', (_menu, open) => {
            if (!open)
                return;
            this._entry.text = '';
            this._renderMenu();
            // Typing goes straight into the field.
            this._entry.grab_key_focus();
        });
    }

    _open(sourceActor) {
        if (this._menu.isOpen && this._menu.sourceActor === sourceActor) {
            this._menu.close();
            return;
        }
        this._menu.close();
        this._menu.sourceActor = sourceActor;
        this._menu.open();
    }

    _start(seconds) {
        countdown.start(seconds);
        this._menu.close();
    }

    _render() {
        if (!this._pill)
            return;
        const active = countdown.active;
        const shown = active && !this._stripOpen;
        const long = countdown.total >= 3600 || countdown.remaining() >= 3600;
        const width = long ? PILL_WIDTH_LONG : PILL_WIDTH;
        this._pill.width = width;
        this._pill.visible = shown;
        this._systemPanel?.setReserve(active ? width + PILL_GAP : 0);
        for (const [name, on] of [
            ['sysi-timer-ringing', countdown.ringing],
            ['sysi-timer-paused', active && !countdown.running && !countdown.ringing],
        ]) {
            if (on)
                this._pill.add_style_class_name(name);
            else
                this._pill.remove_style_class_name(name);
        }
        const text = formatTime(countdown.remaining());
        if (this._time.text !== text)
            this._time.text = text;
        this._disk.queue_repaint();
        if (countdown.running)
            this._tickSoon();
        else
            this._stopTicking();
        if (this._menu?.isOpen)
            this._renderMenu();
    }

    _renderMenu() {
        // Pause, +1 min and cancel only mean something while a timer runs.
        this._controls.visible = countdown.active && !countdown.ringing;
        this._pause.label = countdown.running ? 'pause' : 'resume';
    }

    // Wake as the displayed second changes, not on a fixed beat that would
    // drift across it and show a second twice.
    _tickSoon() {
        if (this._tickId)
            return;
        const ms = Math.round(countdown.remaining() * 1000) % 1000 || 1000;
        this._tickId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms + 5, () => {
            this._tickId = 0;
            countdown.check();
            this._render();
            return GLib.SOURCE_REMOVE;
        });
    }

    _stopTicking() {
        if (this._tickId)
            GLib.source_remove(this._tickId);
        this._tickId = 0;
    }

    // What is left of the run, clockwise from twelve, in a thin ring.
    _paintDisk(area) {
        const cr = area.get_context();
        const [width, height] = area.get_surface_size();
        const color = area.get_theme_node().get_foreground_color();
        const r = Math.min(width, height) / 2;
        const cx = width / 2;
        const cy = height / 2;
        const scale = r / 6;
        const fraction = countdown.ringing ? 0
            : countdown.total > 0 ? Math.min(1, countdown.remaining() / countdown.total) : 0;
        cr.setSourceRGBA(color.red / 255, color.green / 255, color.blue / 255, 0.4);
        cr.setLineWidth(scale);
        cr.arc(cx, cy, r - scale / 2, 0, 2 * Math.PI);
        cr.stroke();
        if (fraction > 0) {
            const [red, green, blue] = countdown.running ? [0.949, 0.710, 0.227] : [color.red / 255, color.green / 255, color.blue / 255];
            cr.setSourceRGBA(red, green, blue, 1);
            const start = -Math.PI / 2;
            cr.moveTo(cx, cy);
            cr.arc(cx, cy, r - 2 * scale, start, start + fraction * 2 * Math.PI);
            cr.closePath();
            cr.fill();
        }
        cr.$dispose();
    }
}
