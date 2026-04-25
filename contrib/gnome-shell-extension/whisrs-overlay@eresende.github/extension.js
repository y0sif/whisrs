import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Pango from 'gi://Pango';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const DBUS_INTERFACE = 'org.whisrs.Overlay';
const DBUS_PATH = '/org/whisrs/Overlay';
const STATE_SIGNAL = 'StateChanged';
const LEVEL_SIGNAL = 'LevelChanged';
const OVERLAY_WIDTH = 260;
const OVERLAY_HEIGHT = 62;

export default class WhisrsOverlayExtension extends Extension {
    enable() {
        this._actor = new St.Widget({
            style_class: 'whisrs-overlay whisrs-overlay-hidden',
            layout_manager: new Clutter.FixedLayout(),
            reactive: false,
            visible: false,
        });

        this._dot = new St.Widget({style_class: 'whisrs-overlay-dot'});
        this._bars = [];
        const bars = new St.BoxLayout({
            style_class: 'whisrs-overlay-bars',
            y_align: Clutter.ActorAlign.CENTER,
        });
        for (let i = 0; i < 6; i++) {
            const bar = new St.Widget({
                style_class: 'whisrs-overlay-bar',
                y_align: Clutter.ActorAlign.CENTER,
                y_expand: false,
            });
            this._bars.push(bar);
            bars.add_child(bar);
        }

        this._label = new St.Label({
            style_class: 'whisrs-overlay-label',
            text: '',
        });
        this._label.clutter_text.set_ellipsize(Pango.EllipsizeMode.NONE);
        this._label.clutter_text.set_line_wrap(false);

        this._actor.add_child(this._dot);
        this._actor.add_child(bars);
        this._actor.add_child(this._label);
        Main.uiGroup.add_child(this._actor);

        this._monitorsChangedId = Main.layoutManager.connect(
            'monitors-changed',
            () => this._position()
        );
        this._allocationChangedId = this._actor.connect(
            'notify::allocation',
            () => this._position()
        );

        this._signalId = Gio.DBus.session.signal_subscribe(
            null,
            DBUS_INTERFACE,
            STATE_SIGNAL,
            DBUS_PATH,
            null,
            Gio.DBusSignalFlags.NONE,
            (_connection, _sender, _path, _iface, _signal, parameters) => {
                const [state] = parameters.deep_unpack();
                this._setState(state);
            }
        );
        this._levelSignalId = Gio.DBus.session.signal_subscribe(
            null,
            DBUS_INTERFACE,
            LEVEL_SIGNAL,
            DBUS_PATH,
            null,
            Gio.DBusSignalFlags.NONE,
            (_connection, _sender, _path, _iface, _signal, parameters) => {
                const [level] = parameters.deep_unpack();
                this._setLevel(level);
            }
        );

        this._position();
    }

    disable() {
        this._stopAnimation();

        if (this._signalId) {
            Gio.DBus.session.signal_unsubscribe(this._signalId);
            this._signalId = 0;
        }

        if (this._levelSignalId) {
            Gio.DBus.session.signal_unsubscribe(this._levelSignalId);
            this._levelSignalId = 0;
        }

        if (this._monitorsChangedId) {
            Main.layoutManager.disconnect(this._monitorsChangedId);
            this._monitorsChangedId = 0;
        }

        if (this._allocationChangedId) {
            this._actor.disconnect(this._allocationChangedId);
            this._allocationChangedId = 0;
        }

        this._actor?.destroy();
        this._actor = null;
        this._dot = null;
        this._label = null;
        this._bars = [];
    }

    _setState(state) {
        if (!this._actor || !this._label)
            return;

        const normalized = String(state).toLowerCase();
        this._actor.remove_style_class_name('whisrs-overlay-recording');
        this._actor.remove_style_class_name('whisrs-overlay-transcribing');
        this._actor.remove_style_class_name('whisrs-overlay-hidden');

        if (normalized === 'recording') {
            this._state = 'recording';
            this._label.text = 'RECORDING';
            this._actor.add_style_class_name('whisrs-overlay-recording');
            this._actor.visible = true;
            this._startAnimation();
            this._position();
        } else if (normalized === 'transcribing') {
            this._state = 'transcribing';
            this._label.text = 'TRANSCRIBING';
            this._actor.add_style_class_name('whisrs-overlay-transcribing');
            this._actor.visible = true;
            this._startAnimation();
            this._position();
        } else {
            this._state = 'idle';
            this._label.text = '';
            this._actor.add_style_class_name('whisrs-overlay-hidden');
            this._actor.visible = false;
            this._stopAnimation();
        }
    }

    _position() {
        if (!this._actor)
            return;

        const monitor = Main.layoutManager.primaryMonitor;
        const width = OVERLAY_WIDTH;
        const height = OVERLAY_HEIGHT;
        const x = Math.floor(monitor.x + (monitor.width - width) / 2);
        const y = Math.floor(monitor.y + monitor.height - height - 34);
        this._actor.set_position(Math.max(monitor.x, x), Math.max(monitor.y, y));
        this._actor.set_size(width, height);
        this._layoutChildren();
        this._actor.set_pivot_point(0.5, 0.5);
        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_OUT_QUAD);
        this._actor.set_easing_duration(120);
    }

    _layoutChildren() {
        if (!this._actor || !this._dot || !this._label)
            return;

        this._dot.set_position(18, 26);
        this._dot.set_size(10, 10);

        const bars = this._bars?.[0]?.get_parent();
        if (bars) {
            bars.set_position(42, 15);
            bars.set_size(66, 32);
        }

        this._label.set_position(122, 22);
        this._label.set_size(120, 20);
    }

    _startAnimation() {
        if (this._animationId)
            return;

        this._frame = 0;
        this._level = 0;
        this._targetLevel = 0;
        this._animationId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 24, () => {
            this._frame++;
            // Snap up instantly, decay smoothly
            const target = this._targetLevel ?? 0;
            if (target > this._level)
                this._level = target;
            else
                this._level = Math.max(0, this._level * 0.85);
            this._updateBars();
            return GLib.SOURCE_CONTINUE;
        });
        this._updateBars();
    }

    _stopAnimation() {
        if (this._animationId) {
            GLib.Source.remove(this._animationId);
            this._animationId = 0;
        }
    }

    _updateBars() {
        if (!this._bars)
            return;

        for (let i = 0; i < this._bars.length; i++) {
            let level = 0;
            if (this._state === 'recording') {
                const raw = Number.isFinite(this._level) ? this._level : 0;
                // Noise gate: below 0.1 treat as silence, above it scale to full range
                level = raw < 0.1 ? 0 : Math.min(1, (raw - 0.1) / 0.6);
            } else if (this._state === 'transcribing') {
                // Animated wave during transcription
                const phase = ((this._frame + i * 5) % 24) / 24;
                level = Math.abs(Math.sin(phase * Math.PI * 2));
            }
            const variance = 0.7 + (((i * 7 + 3) % 6) / 6) * 0.3;
            const height = 4 + Math.round(Math.min(1, level * variance) * 28);
            this._bars[i].set_height(height);
        }
    }

    _setLevel(level) {
        const numeric = Number(level);
        if (!Number.isFinite(numeric))
            return;

        this._targetLevel = Math.max(0, Math.min(1, numeric));
        if (this._state === 'recording')
            this._updateBars();
    }
}
