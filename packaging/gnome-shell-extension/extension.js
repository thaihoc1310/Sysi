import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {getInputSourceManager} from 'resource:///org/gnome/shell/ui/status/keyboard.js';

import {GlassManager, glassMenu} from './glass.js';
import {SystemPanel} from './system.js';
import {TimerPanel} from './timer.js';

const UUID = 'sysi-panel@thaihoc';
// What the colour item switches every card to from each mode, in the order
// Sysi cycles them (ColorMode::next).
const NEXT_MODE = {glass: 'light', light: 'dark', dark: 'glass'};
// The voice models in Sysi's order (VoiceModel::ALL), each with the short
// name the settings menu shows for it.
const VOICE_MODEL_NAMES = {
    'flash-lite': '3.5-fl',
    'lite-latest': 'fl-latest',
    'transcribe': '3.5-trans',
    'groq': 'groq',
};

function formatVoiceModel(raw) {
    if (!raw) return 'flash-lite';
    if (raw.includes('transcribe')) return 'transcribe';
    if (raw.includes('lite-latest')) return 'lite-latest';
    if (raw.includes('groq') || raw.includes('whisper')) return 'groq';
    return 'flash-lite';
}
// Opens and closes the Notes palette. Ctrl+Alt+N until 0.1.77.
const NOTES_BINDING = '<Super><Shift>l';
const NOTES_LEGACY_BINDING = '<control><alt>n';
// How long another input source (Vietnamese) stays on before the first one
// in the list (English) comes back.
const IME_REVERT_SECONDS = 60;

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

        // Opens SYSTEM's menu; see system.js.
        this._system = this._buildPanelButton('system');
        // Opens the timer's menu; see timer.js.
        this._timer = this._buildPanelButton('timer');
        this._buildNotes();
        this._addAction('usage', 'toggle-usage');
        this._addAction('dict', 'toggle-translate');
        this._addAction('ocr', 'ocr');
        this._voiceButton = this._addAction('voice', 'voice');
        this._buildSettings();

        this._gear.connect('clicked', () => {
            this._syncPanelState();
            this._setStripOpen(!this._strip.visible);
        });

        // Append after Ubuntu's left-side indicator instead of prepending it.
        Main.panel.addToStatusArea(UUID, this._indicator, -1, 'left');
        // After the indicator is on the panel: the readings are measured with
        // the panel's own style.
        this._systemPanel = new SystemPanel({
            row: this._content,
            button: this._system,
            gear: this._gear,
            runAction: (action, button) => this._runAction(action, button),
        });
        this._timerPanel = new TimerPanel({
            row: this._content,
            button: this._timer,
            systemPanel: this._systemPanel,
            // A timer just set is what the row should show next.
            onSet: () => this._setStripOpen(false),
        });
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
        // without going near this strip — locking with the hotkey,
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
        // DICTATE asks for one rectangle at a time, on its own file.
        this._dictateRequestFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'dictate-request']),
        );
        if (!this._dictateRequestFile.query_exists(null))
            GLib.file_set_contents(this._dictateRequestFile.get_path(), '');
        this._dictateCapturing = false;
        this._dictateNonce = null;
        try {
            this._dictateRequestMonitor = this._dictateRequestFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._dictateRequestMonitor.connect('changed', () => {
                this._queueDictateCapture();
            });
        } catch (error) {
            logError(error, 'Sysi could not watch dictate requests');
        }
        this._focusRequestFile = Gio.File.new_for_path(
            GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', 'focus-request']),
        );
        if (!this._focusRequestFile.query_exists(null))
            GLib.file_set_contents(this._focusRequestFile.get_path(), '');
        try {
            this._focusRequestMonitor = this._focusRequestFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._focusRequestMonitor.connect('changed', () => this._activateOverlaySoon());
        } catch (error) {
            logError(error, 'Sysi panel gear could not watch focus requests');
        }
        this._ocrSelectingFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'ocr-selecting']),
        );
        if (!this._ocrSelectingFile.query_exists(null))
            GLib.file_set_contents(this._ocrSelectingFile.get_path(), '');
        try {
            this._ocrSelectingMonitor = this._ocrSelectingFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._ocrSelectingMonitor.connect('changed', () => this._syncOcrEscape());
        } catch (error) {
            logError(error, 'Sysi could not watch OCR Escape');
        }
        this._voiceStateFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'voice-state']),
        );
        if (!this._voiceStateFile.query_exists(null))
            GLib.file_set_contents(this._voiceStateFile.get_path(), '');
        try {
            this._voiceStateMonitor = this._voiceStateFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._voiceStateMonitor.connect('changed', () => this._syncVoiceState());
        } catch (error) {
            logError(error, 'Sysi could not watch voice state');
        }
        this._voicePasteFile = Gio.File.new_for_path(
            GLib.build_filenamev([cacheDir, 'voice-paste']),
        );
        if (!this._voicePasteFile.query_exists(null))
            GLib.file_set_contents(this._voicePasteFile.get_path(), '');
        try {
            this._voicePasteMonitor = this._voicePasteFile.monitor_file(
                Gio.FileMonitorFlags.NONE,
                null,
            );
            this._voicePasteMonitor.connect('changed', () => this._handleVoicePaste());
        } catch (error) {
            logError(error, 'Sysi could not watch voice paste');
        }
        this._bindVoiceHotkey();
        this._bindNotesHotkey();
        this._bindOcrHotkey();
        // Xwayland never sees Super while a Wayland app has focus. Grab these here instead.
        this._plainGrabs = [
            this._grabKey('<Super><Shift>o', 'toggle-lock'),
            this._grabKey('<Super><Shift>h', 'toggle-hidden'),
            this._grabKey('<Super><Shift>n', 'new-note'),
            this._grabKey('<Super><Shift>d', 'new-dictionary'),
            this._grabKey('<Super><Shift>u', 'toggle-usage'),
        ].filter(Boolean);
        this._syncOcrEscape();
        this._syncVoiceState();
        this._syncPanelState();
        this._syncVisibility();
        this._glass = new GlassManager();
        this._glass.enable();
        this._inputSources = getInputSourceManager();
        // Also emitted when the current IBus engine merely updates its
        // properties, so only a real switch restarts the countdown.
        this._inputSourceId = this._inputSources.connect('current-source-changed', () => {
            if (this._inputSources.currentSource !== this._lastInputSource)
                this._scheduleImeRevert();
        });
        this._scheduleImeRevert();
    }

    // Through the shell's own manager, so the top bar indicator and the
    // Super+Space order stay in step with the switch.
    _scheduleImeRevert() {
        this._clearImeRevert();
        this._lastInputSource = this._inputSources.currentSource;
        const first = this._inputSources.inputSources[0];
        if (!first || this._lastInputSource === first)
            return;
        this._imeRevertId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, IME_REVERT_SECONDS, () => {
            this._imeRevertId = 0;
            this._inputSources.inputSources[0]?.activate(true);
            return GLib.SOURCE_REMOVE;
        });
    }

    _clearImeRevert() {
        if (this._imeRevertId)
            GLib.source_remove(this._imeRevertId);
        this._imeRevertId = 0;
    }

    disable() {
        if (this._inputSourceId)
            this._inputSources.disconnect(this._inputSourceId);
        this._inputSourceId = 0;
        this._clearImeRevert();
        this._inputSources = null;
        this._lastInputSource = null;
        this._glass?.destroy();
        this._glass = null;
        this._timerPanel?.destroy();
        this._timerPanel = null;
        this._systemPanel?.destroy();
        this._systemPanel = null;
        this._pidMonitor?.cancel();
        this._pidMonitor = null;
        this._panelStateMonitor?.cancel();
        this._panelStateMonitor = null;
        this._unbindNotesHotkey();
        this._unbindOcrHotkey();
        this._unbindVoiceHotkey();
        this._unbindVoiceEscape();
        this._voiceStateMonitor?.cancel();
        this._voiceStateMonitor = null;
        this._voiceStateFile = null;
        this._voicePasteMonitor?.cancel();
        this._voicePasteMonitor = null;
        this._voicePasteFile = null;
        this._voiceButton = null;
        this._virtualKeyboard = null;
        for (const grab of this._plainGrabs ?? [])
            this._ungrabKey(grab);
        this._plainGrabs = [];
        this._unbindOcrEscape();
        this._ocrSelectingMonitor?.cancel();
        this._ocrSelectingMonitor = null;
        this._ocrSelectingFile = null;
        this._focusRequestMonitor?.cancel();
        this._focusRequestMonitor = null;
        this._focusRequestFile = null;
        this._dictateRequestMonitor?.cancel();
        this._dictateRequestMonitor = null;
        this._dictateRequestFile = null;
        this._dictateCapturing = false;
        this._dictateNonce = null;
        this._notesMenu?.destroy();
        this._notesMenu = null;
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
        this._voiceModelLabel = null;
        this._voiceItems = null;
        this._lockLabel = null;
        this._hideLabel = null;
        this._pidFile = null;
        this._panelStateFile = null;
    }

    // A small menu like settings': a new note, or the list of them.
    _buildNotes() {
        const button = this._buildPanelButton('notes');
        this._notesMenu = new PopupMenu.PopupMenu(button, 0.5, St.Side.TOP);
        this._notesMenu.actor.add_style_class_name('sysi-settings-menu');
        glassMenu(this._notesMenu);
        Main.uiGroup.add_child(this._notesMenu.actor);
        this._notesMenu.actor.hide();
        Main.panel.menuManager.addMenu(this._notesMenu);
        button.connect('clicked', () => this._notesMenu.toggle());
        for (const [label, action] of [['new', 'new-note'], ['list', 'toggle-notes']]) {
            const item = new PopupMenu.PopupMenuItem(label);
            item.label.x_align = Clutter.ActorAlign.CENTER;
            item.label.x_expand = true;
            item.connect('activate', () => this._runAction(action, button));
            this._notesMenu.addMenuItem(item);
        }
    }

    _buildSettings() {
        const button = this._buildPanelButton('settings');
        this._settingsMenu = new PopupMenu.PopupMenu(button, 0.5, St.Side.TOP);
        this._settingsMenu.actor.add_style_class_name('sysi-settings-menu');
        glassMenu(this._settingsMenu);
        Main.uiGroup.add_child(this._settingsMenu.actor);
        this._settingsMenu.actor.hide();
        Main.panel.menuManager.addMenu(this._settingsMenu);
        button.connect('clicked', () => {
            this._syncPanelState();
            this._settingsMenu.toggle();
        });
        // Says what a click turns every card to, the way lock / unlock
        // does and a card's own menu does.
        const mode = new PopupMenu.PopupMenuItem(NEXT_MODE[this._readColorMode()]);
        this._modeLabel = mode.label;
        mode.label.x_align = Clutter.ActorAlign.CENTER;
        mode.label.x_expand = true;
        // Do not emit PopupMenuItem's activate signal: it closes the menu.
        mode.activate = () => this._runAction('next-color-mode', button);
        this._settingsMenu.addMenuItem(mode);

        // A pick of four rather than a toggle: the row names the model in
        // use and opens on a click to the others, each switching to itself.
        const voice = new PopupMenu.PopupSubMenuMenuItem('');
        voice.add_style_class_name('sysi-voice-model');
        this._voiceModelLabel = voice.label;
        voice.label.x_align = Clutter.ActorAlign.CENTER;
        voice.label.x_expand = true;
        // Only the word, centred like the rows around it: no expander, no arrow.
        for (const child of voice.get_children()) {
            if (child !== voice.label)
                child.hide();
        }
        this._voiceItems = {};
        for (const [model, name] of Object.entries(VOICE_MODEL_NAMES)) {
            const item = new PopupMenu.PopupMenuItem(name);
            item.add_style_class_name('sysi-voice-option');
            item.label.x_align = Clutter.ActorAlign.CENTER;
            item.label.x_expand = true;
            item.activate = () => {
                // Sysi writes the new model a moment later; show it now.
                this._showVoiceModel(model);
                this._runAction(`set-voice-model:${model}`, button);
                voice.menu.close(true);
            };
            voice.menu.addMenuItem(item);
            this._voiceItems[model] = item;
        }
        this._settingsMenu.addMenuItem(voice);
        this._showVoiceModel(formatVoiceModel(this._readVoiceModel()));

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
        const hide = new PopupMenu.PopupMenuItem('hide');
        this._hideLabel = hide.label;
        hide.label.x_align = Clutter.ActorAlign.CENTER;
        hide.label.x_expand = true;
        hide.connect('activate', () => this._runAction('toggle-hidden', button));
        this._settingsMenu.addMenuItem(hide);
        const quit = new PopupMenu.PopupMenuItem('quit');
        quit.label.x_align = Clutter.ActorAlign.CENTER;
        quit.label.x_expand = true;
        quit.connect('activate', () => this._runAction('quit', button));
        this._settingsMenu.addMenuItem(quit);
    }

    // The strip covers the readings and the timer while it is open.
    _setStripOpen(open) {
        this._strip.visible = open;
        if (!open) {
            this._settingsMenu?.close();
            this._notesMenu?.close();
        }
        this._systemPanel?.setStripOpen(open);
        this._timerPanel?.setStripOpen(open);
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
            return ['light', 'dark', 'glass'].includes(mode) ? mode : 'glass';
        } catch (_) {
            return 'glass';
        }
    }

    // The row names the model in use, so its list holds only the others.
    _showVoiceModel(model) {
        this._voiceModelLabel.text = VOICE_MODEL_NAMES[model];
        for (const [name, item] of Object.entries(this._voiceItems ?? {}))
            item.visible = name !== model;
    }

    _readVoiceModel() {
        try {
            const cachePath = GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', 'voice-model']);
            const [cacheOk, cacheContents] = GLib.file_get_contents(cachePath);
            if (cacheOk) {
                const text = new TextDecoder().decode(cacheContents).trim();
                if (text) return text;
            }
            const path = GLib.build_filenamev([
                GLib.get_user_config_dir(),
                'sysi',
                'state.json',
            ]);
            const [ok, contents] = GLib.file_get_contents(path);
            const model = ok
                ? JSON.parse(new TextDecoder().decode(contents))?.settings?.voice_model
                : null;
            return model || 'flash-lite';
        } catch (_) {
            return 'flash-lite';
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
        // Snapshot the target before we steal focus. Activating the
        // overlay first made Notes see Sysi itself (or the mouse) and
        // walk the palette to the other monitor a frame later.
        if (action === 'voice')
            this._lastFocusedWindow = global.display.focus_window;
        const notes = action === 'toggle-notes' || action === 'toggle-history';
        const ocr = action === 'ocr' || action === 'dictate';
        const voice = action === 'voice';
        const typing = action === 'new-note' || action === 'new-dictionary';
        const cancelOcr = action === 'cancel-ocr';
        const anchor = button && !voice
            ? this._anchorOf(button)
            : (notes || voice) ? this._focusAnchor() : this._pointerAnchor();
        if (notes || ocr || typing)
            this._activateOverlaySoon();
        if (cancelOcr) {
            const argv = ['sysi', '--panel-action', action];
            try {
                GLib.spawn_async(null, argv, null, GLib.SpawnFlags.SEARCH_PATH, null);
            } catch (error) {
                logError(error, `Sysi panel action ${action} failed`);
            }
            return;
        }
        const argv = ['sysi', '--panel-action', action];
        if (anchor)
            argv.push('--at', anchor);
        try {
            GLib.spawn_async(null, argv, null, GLib.SpawnFlags.SEARCH_PATH, null);
        } catch (error) {
            logError(error, `Sysi panel action ${action} failed`);
        }
    }

    _overlayWindow() {
        const actors = global.get_window_actors();
        for (const actor of actors) {
            const win = actor.meta_window;
            if (!win)
                continue;
            if (this._isOverlayWindow(win))
                return win;
        }
        return null;
    }

    _activateOverlay(timestamp) {
        const win = this._overlayWindow();
        if (!win)
            return;
        const time = timestamp || global.get_current_time();
        Main.activateWindow(win, time);
    }

    _activateOverlaySoon(timestamp) {
        this._activateOverlay(timestamp);
        // The overlay refuses the keyboard until Notes is open. The helper
        // process has not finished that yet, so try once more after it has.
        if (this._activateTimeout)
            GLib.source_remove(this._activateTimeout);
        this._activateTimeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 80, () => {
            this._activateTimeout = 0;
            this._activateOverlay();
            return GLib.SOURCE_REMOVE;
        });
    }

    _notesShortcutSettings() {
        try {
            return new Gio.Settings({
                schema_id: 'org.gnome.settings-daemon.plugins.media-keys.custom-keybinding',
                path: '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/sysi-notes/',
            });
        } catch (_) {
            return null;
        }
    }

    _silenceCustomNotesShortcut() {
        const settings = this._notesShortcutSettings();
        if (!settings)
            return;
        if (settings.get_string('command') !== 'sysi --panel-action toggle-notes')
            return;
        const binding = settings.get_string('binding');
        if (!binding)
            return;
        this._notesShortcutBinding = binding;
        settings.set_string('binding', '');
    }

    _restoreCustomNotesShortcut() {
        const settings = this._notesShortcutSettings();
        if (!settings || !this._notesShortcutBinding)
            return;
        // Ctrl+Alt+N was the default before Super+Shift+L; hand the fallback
        // the new one rather than the key Notes no longer answers to.
        const binding = this._notesShortcutBinding.toLowerCase() === NOTES_LEGACY_BINDING
            ? NOTES_BINDING
            : this._notesShortcutBinding;
        if (!settings.get_string('binding'))
            settings.set_string('binding', binding);
        this._notesShortcutBinding = null;
    }

    _bindNotesHotkey() {
        // The GNOME custom shortcut only launches a helper process. That
        // process cannot take the keyboard. Grabbing here runs inside the
        // compositor, so Notes can type even when a Wayland app had focus.
        // Silence our own custom shortcut first; otherwise the grab fails
        // and we are back to a helper that cannot focus the overlay.
        this._silenceCustomNotesShortcut();
        try {
            this._notesAccelAction = global.display.grab_accelerator(
                NOTES_BINDING,
                Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
            );
        } catch (error) {
            logError(error, 'Sysi could not grab Super+Shift+L');
            this._notesAccelAction = 0;
            this._restoreCustomNotesShortcut();
            return;
        }
        if (!this._notesAccelAction || this._notesAccelAction === Meta.KeyBindingAction.NONE) {
            this._notesAccelAction = 0;
            this._restoreCustomNotesShortcut();
            return;
        }
        this._notesAccelName = Meta.external_binding_name_for_action(this._notesAccelAction);
        Main.wm.allowKeybinding(
            this._notesAccelName,
            Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
        );
        this._notesAccelId = global.display.connect(
            'accelerator-activated',
            (_display, action, _deviceId, timestamp) => {
                if (action !== this._notesAccelAction)
                    return;
                this._runAction('toggle-notes', null);
            },
        );
    }

    _unbindNotesHotkey() {
        if (this._activateTimeout) {
            GLib.source_remove(this._activateTimeout);
            this._activateTimeout = 0;
        }
        if (this._notesAccelId) {
            global.display.disconnect(this._notesAccelId);
            this._notesAccelId = 0;
        }
        if (this._notesAccelName)
            Main.wm.allowKeybinding(this._notesAccelName, Shell.ActionMode.NONE);
        if (this._notesAccelAction)
            global.display.ungrab_accelerator(this._notesAccelAction);
        this._notesAccelAction = 0;
        this._notesAccelName = null;
        this._restoreCustomNotesShortcut();
    }

    _ocrShortcutSettings() {
        try {
            return new Gio.Settings({
                schema_id: 'org.gnome.settings-daemon.plugins.media-keys.custom-keybinding',
                path: '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/sysi-ocr/',
            });
        } catch (_) {
            return null;
        }
    }

    _silenceCustomOcrShortcut() {
        const settings = this._ocrShortcutSettings();
        if (!settings)
            return;
        if (settings.get_string('command') !== 'sysi --panel-action ocr')
            return;
        const binding = settings.get_string('binding');
        if (!binding)
            return;
        this._ocrShortcutBinding = binding;
        settings.set_string('binding', '');
    }

    _restoreCustomOcrShortcut() {
        const settings = this._ocrShortcutSettings();
        if (!settings || !this._ocrShortcutBinding)
            return;
        if (!settings.get_string('binding'))
            settings.set_string('binding', this._ocrShortcutBinding);
        this._ocrShortcutBinding = null;
    }

    _bindOcrHotkey() {
        // Super is owned by the compositor. An X11 grab never sees it, so
        // this grab is what makes Super+Shift+A start OCR from any app.
        this._silenceCustomOcrShortcut();
        try {
            this._ocrAccelAction = global.display.grab_accelerator(
                '<Super><Shift>a',
                Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
            );
        } catch (error) {
            logError(error, 'Sysi could not grab Super+Shift+A');
            this._ocrAccelAction = 0;
            this._restoreCustomOcrShortcut();
            return;
        }
        if (!this._ocrAccelAction || this._ocrAccelAction === Meta.KeyBindingAction.NONE) {
            this._ocrAccelAction = 0;
            this._restoreCustomOcrShortcut();
            return;
        }
        this._ocrAccelName = Meta.external_binding_name_for_action(this._ocrAccelAction);
        Main.wm.allowKeybinding(
            this._ocrAccelName,
            Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
        );
        this._ocrAccelId = global.display.connect(
            'accelerator-activated',
            (_display, action, _deviceId, timestamp) => {
                if (action !== this._ocrAccelAction)
                    return;
                this._activateOverlay(timestamp);
                this._runAction('ocr', null);
            },
        );
    }

    _ocrSelecting() {
        if (!this._ocrSelectingFile || this._readPid() <= 0)
            return false;
        try {
            const [ok, contents] = GLib.file_get_contents(this._ocrSelectingFile.get_path());
            return ok && new TextDecoder().decode(contents).trim().length > 0;
        } catch (_) {
            return false;
        }
    }

    _syncOcrEscape() {
        if (this._ocrSelecting())
            this._bindOcrEscape();
        else
            this._unbindOcrEscape();
    }

    _bindOcrEscape() {
        if (this._ocrEscapeAction)
            return;
        try {
            this._ocrEscapeAction = global.display.grab_accelerator(
                'Escape',
                Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
            );
        } catch (error) {
            logError(error, 'Sysi could not grab Escape for OCR');
            this._ocrEscapeAction = 0;
            return;
        }
        if (!this._ocrEscapeAction || this._ocrEscapeAction === Meta.KeyBindingAction.NONE) {
            try {
                this._ocrEscapeAction = global.display.grab_accelerator(
                    '<Escape>',
                    Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
                );
            } catch (error) {
                logError(error, 'Sysi could not grab <Escape> for OCR');
                this._ocrEscapeAction = 0;
                return;
            }
        }
        if (!this._ocrEscapeAction || this._ocrEscapeAction === Meta.KeyBindingAction.NONE) {
            this._ocrEscapeAction = 0;
            return;
        }
        this._ocrEscapeName = Meta.external_binding_name_for_action(this._ocrEscapeAction);
        Main.wm.allowKeybinding(
            this._ocrEscapeName,
            Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
        );
        this._ocrEscapeId = global.display.connect(
            'accelerator-activated',
            (_display, action) => {
                if (action !== this._ocrEscapeAction)
                    return;
                this._runAction('cancel-ocr', null);
            },
        );
    }

    _unbindOcrEscape() {
        if (this._ocrEscapeId) {
            global.display.disconnect(this._ocrEscapeId);
            this._ocrEscapeId = 0;
        }
        if (this._ocrEscapeName)
            Main.wm.allowKeybinding(this._ocrEscapeName, Shell.ActionMode.NONE);
        if (this._ocrEscapeAction)
            global.display.ungrab_accelerator(this._ocrEscapeAction);
        this._ocrEscapeAction = 0;
        this._ocrEscapeName = null;
    }

    // A compositor grab with no GNOME custom-shortcut fallback to juggle.
    _grabKey(accelerator, sysiAction) {
        let action = 0;
        try {
            action = global.display.grab_accelerator(accelerator, Meta.KeyBindingFlags.IGNORE_AUTOREPEAT);
        } catch (error) {
            logError(error, `Sysi could not grab ${accelerator}`);
        }
        if (!action || action === Meta.KeyBindingAction.NONE)
            return null;
        const name = Meta.external_binding_name_for_action(action);
        Main.wm.allowKeybinding(name, Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW);
        const id = global.display.connect('accelerator-activated', (_display, activated) => {
            if (activated === action)
                this._runAction(sysiAction, null);
        });
        return {action, name, id};
    }

    _ungrabKey({action, name, id}) {
        global.display.disconnect(id);
        Main.wm.allowKeybinding(name, Shell.ActionMode.NONE);
        global.display.ungrab_accelerator(action);
    }

    _unbindOcrHotkey() {
        if (this._ocrAccelId) {
            global.display.disconnect(this._ocrAccelId);
            this._ocrAccelId = 0;
        }
        if (this._ocrAccelName)
            Main.wm.allowKeybinding(this._ocrAccelName, Shell.ActionMode.NONE);
        if (this._ocrAccelAction)
            global.display.ungrab_accelerator(this._ocrAccelAction);
        this._ocrAccelAction = 0;
        this._ocrAccelName = null;
        this._restoreCustomOcrShortcut();
    }

    _voiceShortcutSettings() {
        try {
            return new Gio.Settings({
                schema_id: 'org.gnome.settings-daemon.plugins.media-keys.custom-keybinding',
                path: '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/sysi-voice/',
            });
        } catch (_) {
            return null;
        }
    }

    _silenceCustomVoiceShortcut() {
        const settings = this._voiceShortcutSettings();
        if (!settings)
            return;
        if (settings.get_string('command') !== 'sysi --panel-action voice')
            return;
        const binding = settings.get_string('binding');
        if (!binding)
            return;
        this._voiceShortcutBinding = binding;
        settings.set_string('binding', '');
    }

    _restoreCustomVoiceShortcut() {
        const settings = this._voiceShortcutSettings();
        if (!settings || !this._voiceShortcutBinding)
            return;
        if (!settings.get_string('binding'))
            settings.set_string('binding', this._voiceShortcutBinding);
        this._voiceShortcutBinding = null;
    }

    _bindVoiceHotkey() {
        this._silenceCustomVoiceShortcut();
        try {
            this._voiceAccelAction = global.display.grab_accelerator(
                '<Super><Shift>v',
                Meta.KeyBindingFlags.IGNORE_AUTOREPEAT,
            );
        } catch (error) {
            logError(error, 'Sysi could not grab Super+Shift+V');
            this._voiceAccelAction = 0;
            this._restoreCustomVoiceShortcut();
            return;
        }
        if (!this._voiceAccelAction || this._voiceAccelAction === Meta.KeyBindingAction.NONE) {
            this._voiceAccelAction = 0;
            this._restoreCustomVoiceShortcut();
            return;
        }
        this._voiceAccelName = Meta.external_binding_name_for_action(this._voiceAccelAction);
        Main.wm.allowKeybinding(
            this._voiceAccelName,
            Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
        );
        this._voiceAccelId = global.display.connect(
            'accelerator-activated',
            (_display, action) => {
                if (action !== this._voiceAccelAction)
                    return;
                this._lastFocusedWindow = global.display.focus_window;
                this._runAction('voice', null);
            },
        );
    }

    _unbindVoiceHotkey() {
        if (this._voiceAccelId) {
            global.display.disconnect(this._voiceAccelId);
            this._voiceAccelId = 0;
        }
        if (this._voiceAccelName)
            Main.wm.allowKeybinding(this._voiceAccelName, Shell.ActionMode.NONE);
        if (this._voiceAccelAction)
            global.display.ungrab_accelerator(this._voiceAccelAction);
        this._voiceAccelAction = 0;
        this._voiceAccelName = null;
        this._restoreCustomVoiceShortcut();
    }

    _voiceRecording() {
        if (!this._voiceStateFile || this._readPid() <= 0)
            return false;
        try {
            const [ok, contents] = GLib.file_get_contents(this._voiceStateFile.get_path());
            return ok && new TextDecoder().decode(contents).trim().startsWith('recording');
        } catch (_) {
            return false;
        }
    }

    _syncVoiceState() {
        const recording = this._voiceRecording();
        if (this._voiceButton) {
            const label = this._voiceButton.get_first_child();
            if (label) {
                if (recording) {
                    label.text = '● voice';
                    this._voiceButton.add_style_class_name('sysi-voice-recording');
                } else {
                    label.text = 'voice';
                    this._voiceButton.remove_style_class_name('sysi-voice-recording');
                }
            }
        }
        if (recording)
            this._bindVoiceEscape();
        else
            this._unbindVoiceEscape();
    }

    _bindVoiceEscape() {
        this._voiceEscapeGrab ??= this._grabKey('Escape', 'cancel-voice') ??
            this._grabKey('<Escape>', 'cancel-voice');
    }

    _unbindVoiceEscape() {
        if (this._voiceEscapeGrab)
            this._ungrabKey(this._voiceEscapeGrab);
        this._voiceEscapeGrab = null;
    }

    _handleVoicePaste() {
        if (!this._voicePasteFile || this._readPid() <= 0)
            return;
        try {
            const [ok, contents] = GLib.file_get_contents(this._voicePasteFile.get_path());
            if (!ok)
                return;
            const text = new TextDecoder().decode(contents).trim();
            if (!text)
                return;
            const tabIdx = text.indexOf('\t');
            if (tabIdx === -1)
                return;
            const nonce = text.substring(0, tabIdx);
            if (this._lastPasteNonce === nonce)
                return;
            this._lastPasteNonce = nonce;
            const payload = text.substring(tabIdx + 1);
            // The dictation is on the clipboard now; keep none of it on disk.
            GLib.file_set_contents(this._voicePasteFile.get_path(), '');

            St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, payload);

            if (this._lastFocusedWindow)
                this._lastFocusedWindow.activate(global.get_current_time());

            const targetWin = global.display.focus_window || this._lastFocusedWindow;
            const wmClass = targetWin ? (targetWin.get_wm_class() || '').toLowerCase() : '';
            const isTerminal = /terminal|kitty|ghostty|alacritty|wezterm|ptyxis|foot|xterm/.test(wmClass);

            this._simulatePaste(isTerminal);
        } catch (error) {
            logError(error, 'Sysi voice paste failed');
        }
    }

    _simulatePaste(isTerminal) {
        try {
            const seat = Clutter.get_default_backend().get_default_seat();
            if (!this._virtualKeyboard) {
                this._virtualKeyboard = seat.create_virtual_device(
                    Clutter.InputDeviceType.KEYBOARD_DEVICE,
                );
            }
            const vk = this._virtualKeyboard;
            const KEY_CTRL = Clutter.KEY_Control_L || 0xffe3;
            const KEY_SHIFT = Clutter.KEY_Shift_L || 0xffe1;
            const KEY_V = Clutter.KEY_v || 0x0076;

            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 40, () => {
                const t1 = GLib.get_monotonic_time();
                vk.notify_keyval(t1, KEY_CTRL, Clutter.KeyState.PRESSED);
                if (isTerminal)
                    vk.notify_keyval(t1, KEY_SHIFT, Clutter.KeyState.PRESSED);

                const t2 = t1 + 8000;
                vk.notify_keyval(t2, KEY_V, Clutter.KeyState.PRESSED);

                const t3 = t2 + 8000;
                vk.notify_keyval(t3, KEY_V, Clutter.KeyState.RELEASED);

                const t4 = t3 + 8000;
                if (isTerminal)
                    vk.notify_keyval(t4, KEY_SHIFT, Clutter.KeyState.RELEASED);
                vk.notify_keyval(t4, KEY_CTRL, Clutter.KeyState.RELEASED);

                return GLib.SOURCE_REMOVE;
            });
        } catch (error) {
            logError(error, 'Sysi voice virtual keyboard paste simulation failed');
        }
    }

    _writeAnchor(text) {
        try {
            GLib.file_set_contents(
                GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', 'pointer']),
                text,
            );
            return text;
        } catch (error) {
            logError(error, 'Sysi could not write the pointer');
            return null;
        }
    }

    _pointerAnchor() {
        try {
            const [x, y] = global.get_pointer();
            if (![x, y].every(Number.isFinite))
                return null;
            return this._writeAnchor(`${Math.round(x)},${Math.round(y)}`);
        } catch (_) {
            return null;
        }
    }

    _isOverlayWindow(win) {
        if (!win)
            return false;
        const title = win.get_title() ?? '';
        const wmClass = (win.get_wm_class() ?? '').toLowerCase();
        const gtkId = win.get_gtk_application_id?.() ?? '';
        return title === 'Sysi Overlay' || wmClass === 'sysi' || gtkId === 'io.sysi.Overlay';
    }

    // Monitor of the focused window — the one the user last clicked.
    _focusAnchor() {
        const win = global.display.focus_window;
        if (win && !this._isOverlayWindow(win)) {
            try {
                const index = win.get_monitor();
                const monitor = Main.layoutManager.monitors[index];
                if (monitor)
                    return this._writeAnchor(
                        `${Math.round(monitor.x + monitor.width / 2)},${Math.round(monitor.y + monitor.height / 2)}`,
                    );
            } catch (_) {}
            try {
                const rect = win.get_frame_rect();
                if (rect)
                    return this._writeAnchor(
                        `${Math.round(rect.x + rect.width / 2)},${Math.round(rect.y + rect.height / 2)}`,
                    );
            } catch (_) {}
        }
        return this._pointerAnchor();
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
            this._setStripOpen(false);
            this._unbindOcrEscape();
        } else {
            this._syncOcrEscape();
        }
    }

    // `<editing|locked> <light|dark|glass> <font-size>`, written by Sysi whenever
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
                ['light', 'dark', 'glass'].includes(mode) ? mode : null,
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
        // Hidden means unmapped, and an unmapped window has no actor.
        if (this._hideLabel)
            this._hideLabel.text = this._overlayWindow() ? 'hide' : 'show';
        if (this._modeLabel)
            this._modeLabel.text = NEXT_MODE[mode ?? this._readColorMode()];
        if (this._voiceModelLabel)
            this._showVoiceModel(formatVoiceModel(this._readVoiceModel()));
    }

    _writeCacheFile(name, contents) {
        GLib.file_set_contents(
            GLib.build_filenamev([GLib.get_user_cache_dir(), 'sysi', name]),
            contents,
        );
    }

    // Photograph the rectangle Sysi has just been dragged out over, so its
    // DICTATE can read text off a Wayland window. Through Xwayland Sysi's own
    // root window holds X11 clients only, so a selection over anything native
    // would come back as wallpaper.
    _queueDictateCapture() {
        if (!this._dictateRequestFile || Main.layoutManager._startingUp)
            return;
        // A second region while one grab is outstanding is dropped rather than
        // queued: both would write the same picture, and Sysi is only ever
        // waiting for the nonce it asked with.
        if (this._dictateCapturing)
            return;
        const request = this._readDictateRequest();
        // A write arrives as several change events, and the file is created
        // empty at startup. Only a region Sysi has not already been given a
        // picture of is worth hiding every one of its windows for.
        if (!request || request.nonce === this._dictateNonce)
            return;
        this._dictateNonce = request.nonce;
        this._dictateCapturing = true;
        this._captureDictateRegion(request)
            .catch(error => logError(error, 'Sysi could not capture a dictate region'))
            .finally(() => (this._dictateCapturing = false));
    }

    _readDictateRequest() {
        let raw;
        try {
            const [ok, contents] = GLib.file_get_contents(
                this._dictateRequestFile.get_path(),
            );
            if (!ok)
                return null;
            raw = new TextDecoder().decode(contents);
        } catch (_) {
            return null;
        }
        const [nonce, geometry] = raw.trim().split('\t');
        const values = geometry?.split(',').map(Number) ?? [];
        if (!nonce || values.length !== 4 || !values.every(Number.isFinite))
            return null;
        const [x, y, width, height] = values;
        return width > 0 && height > 0 ? {nonce, x, y, width, height} : null;
    }

    async _captureDictateRegion({nonce, x, y, width, height}) {
        const rect = this._clampToMonitor({x, y, width, height});
        if (!rect)
            return;
        const path = GLib.build_filenamev([
            GLib.get_user_runtime_dir(), 'sysi', 'dictate.png',
        ]);
        const file = Gio.File.new_for_path(path);
        let stream = null;
        let done = null;
        // The dimmed selection layer is still on screen at this point, and it
        // is Sysi's own window: without hiding it the OCR would be handed a
        // photograph of the dimming rather than of the text underneath.
        this._withSysiHidden(() => {
            try {
                stream = file.replace(
                    null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
                done = new Shell.Screenshot().screenshot_area(
                    rect.x, rect.y, rect.width, rect.height, stream);
            } catch (error) {
                this._closeQuietly(stream);
                stream = null;
                done = null;
                logError(error, 'Sysi could not start a dictate capture');
            }
        });
        if (!done)
            return;
        try {
            await done;
        } finally {
            // The rename onto the real name only happens on close.
            this._closeQuietly(stream);
        }
        this._writeCacheFile('dictate-result', `${nonce}\t${path}\n`);
    }

    _closeQuietly(stream) {
        try {
            stream?.close(null);
        } catch (_) {
            // Already closed, or the write failed; either way there is nothing
            // left to do with it.
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
}
