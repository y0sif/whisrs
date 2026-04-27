import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const DBUS_INTERFACE = 'org.whisrs.Overlay';
const DBUS_PATH = '/org/whisrs/Overlay';
const STATE_SIGNAL = 'StateChanged';
const LEVEL_SIGNAL = 'LevelChanged';
const THEME_SIGNAL = 'ThemeChanged';

const OVERLAY_WIDTH = 100;
const OVERLAY_HEIGHT = 34;
const BOTTOM_MARGIN = 16;
const BAR_COUNT = 5;
const BAR_W = 6;
const BAR_GAP = 4;
const BAR_BASELINE = 3;

const SPAWN_IN_MS = 180;
const SPAWN_OUT_MS = 140;
const BARS_GRACE_MS = 80;

const KNOWN_THEMES = ['ember', 'carbon', 'cyan'];

export default class WhisrsOverlayExtension extends Extension {
    enable() {
        this._theme = 'carbon';

        this._actor = new St.Widget({
            style_class: 'whisrs-overlay whisrs-overlay-hidden whisrs-theme-carbon',
            layout_manager: new Clutter.FixedLayout(),
            reactive: false,
            visible: false,
        });

        this._barsBox = new St.BoxLayout({
            style_class: 'whisrs-overlay-bars',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._bars = [];
        for (let i = 0; i < BAR_COUNT; i++) {
            const bar = new St.Widget({
                style_class: 'whisrs-overlay-bar',
                y_align: Clutter.ActorAlign.CENTER,
                y_expand: false,
            });
            this._bars.push(bar);
            this._barsBox.add_child(bar);
        }

        this._actor.add_child(this._barsBox);
        Main.uiGroup.add_child(this._actor);

        this._monitorsChangedId = Main.layoutManager.connect(
            'monitors-changed',
            () => this._position()
        );

        this._signalId = Gio.DBus.session.signal_subscribe(
            null, DBUS_INTERFACE, STATE_SIGNAL, DBUS_PATH, null,
            Gio.DBusSignalFlags.NONE,
            (_c, _s, _p, _i, _sig, parameters) => {
                const [state] = parameters.deep_unpack();
                this._setState(state);
            }
        );
        this._levelSignalId = Gio.DBus.session.signal_subscribe(
            null, DBUS_INTERFACE, LEVEL_SIGNAL, DBUS_PATH, null,
            Gio.DBusSignalFlags.NONE,
            (_c, _s, _p, _i, _sig, parameters) => {
                const [level] = parameters.deep_unpack();
                this._setLevel(level);
            }
        );
        this._themeSignalId = Gio.DBus.session.signal_subscribe(
            null, DBUS_INTERFACE, THEME_SIGNAL, DBUS_PATH, null,
            Gio.DBusSignalFlags.NONE,
            (_c, _s, _p, _i, _sig, parameters) => {
                const [theme] = parameters.deep_unpack();
                this._setTheme(theme);
            }
        );

        this._state = 'idle';
        this._level = 0;
        this._targetLevel = 0;
        this._frame = 0;

        this._position();
    }

    disable() {
        this._stopAnimation();

        for (const id of [this._signalId, this._levelSignalId, this._themeSignalId]) {
            if (id) Gio.DBus.session.signal_unsubscribe(id);
        }
        this._signalId = 0;
        this._levelSignalId = 0;
        this._themeSignalId = 0;

        if (this._monitorsChangedId) {
            Main.layoutManager.disconnect(this._monitorsChangedId);
            this._monitorsChangedId = 0;
        }

        this._actor?.destroy();
        this._actor = null;
        this._bars = [];
        this._barsBox = null;
    }

    _setTheme(theme) {
        if (!this._actor) return;
        const next = KNOWN_THEMES.includes(String(theme)) ? String(theme) : 'carbon';
        if (next === this._theme) return;
        this._actor.remove_style_class_name(`whisrs-theme-${this._theme}`);
        this._actor.add_style_class_name(`whisrs-theme-${next}`);
        this._theme = next;
    }

    _setState(state) {
        if (!this._actor) return;

        const normalized = String(state).toLowerCase();
        const wasIdle = this._state === 'idle';
        const nowIdle = normalized === 'idle';

        this._actor.remove_style_class_name('whisrs-overlay-recording');
        this._actor.remove_style_class_name('whisrs-overlay-transcribing');
        this._actor.remove_style_class_name('whisrs-overlay-hidden');

        if (normalized === 'recording') {
            this._state = 'recording';
            this._actor.add_style_class_name('whisrs-overlay-recording');
            this._actor.visible = true;
            if (wasIdle) this._spawnIn();
            this._startAnimation();
        } else if (normalized === 'transcribing') {
            this._state = 'transcribing';
            this._actor.add_style_class_name('whisrs-overlay-transcribing');
            this._actor.visible = true;
            if (wasIdle) this._spawnIn();
            this._startAnimation();
        } else {
            this._state = 'idle';
            this._actor.add_style_class_name('whisrs-overlay-hidden');
            if (!wasIdle) this._spawnOut();
            this._stopAnimation();
        }

        if (!wasIdle && !nowIdle) {
            // Recording <-> Transcribing — keep the pill steady.
        }
    }

    /// Slide the pill up + scale up + fade in. Clutter applies the easing.
    _spawnIn() {
        if (!this._actor) return;
        // Snap to the start state without easing.
        this._actor.set_easing_duration(0);
        this._actor.translation_y = 8;
        this._actor.set_pivot_point(0.5, 0.5);
        this._actor.set_scale(0.92, 0.92);
        this._actor.opacity = 0;

        // Then animate to the rest state.
        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_OUT_CUBIC);
        this._actor.set_easing_duration(SPAWN_IN_MS);
        this._actor.translation_y = 0;
        this._actor.set_scale(1.0, 1.0);
        this._actor.opacity = 255;

        // Hold bars at baseline while the pill is still flying in.
        this._barsGraceUntil = Date.now() + BARS_GRACE_MS;
    }

    /// Reverse the spawn: slide down + slight shrink + fade out, then hide.
    _spawnOut() {
        if (!this._actor) return;
        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_IN_CUBIC);
        this._actor.set_easing_duration(SPAWN_OUT_MS);
        this._actor.translation_y = 8;
        this._actor.set_scale(0.96, 0.96);
        this._actor.opacity = 0;

        GLib.timeout_add(GLib.PRIORITY_DEFAULT, SPAWN_OUT_MS + 20, () => {
            if (this._state === 'idle' && this._actor) this._actor.visible = false;
            return GLib.SOURCE_REMOVE;
        });
    }

    _position() {
        if (!this._actor) return;

        const monitor = Main.layoutManager.primaryMonitor;
        const x = Math.floor(monitor.x + (monitor.width - OVERLAY_WIDTH) / 2);
        const y = Math.floor(monitor.y + monitor.height - OVERLAY_HEIGHT - BOTTOM_MARGIN);
        this._actor.set_position(Math.max(monitor.x, x), Math.max(monitor.y, y));
        this._actor.set_size(OVERLAY_WIDTH, OVERLAY_HEIGHT);

        const cy = Math.floor(OVERLAY_HEIGHT / 2);
        const barBlock = BAR_COUNT * BAR_W + (BAR_COUNT - 1) * BAR_GAP;
        const barsX = Math.floor((OVERLAY_WIDTH - barBlock) / 2);
        const maxH = OVERLAY_HEIGHT - 10;
        if (this._barsBox) {
            this._barsBox.set_position(barsX, cy - Math.floor(maxH / 2));
            this._barsBox.set_size(barBlock, maxH);
        }
    }

    _startAnimation() {
        if (this._animationId) return;

        this._animationId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 24, () => {
            this._frame++;
            const target = this._targetLevel ?? 0;
            this._level = target > this._level ? target : Math.max(0, this._level * 0.85);
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

    _taper(i) {
        if (BAR_COUNT <= 1) return 1;
        const center = (BAR_COUNT - 1) / 2;
        const d = (i - center) / center;
        return Math.exp(-d * d);
    }

    _updateBars() {
        if (!this._bars || this._bars.length === 0) return;

        const maxH = OVERLAY_HEIGHT - 10;

        if (this._state === 'recording') {
            const grace = this._barsGraceUntil && Date.now() < this._barsGraceUntil;
            const raw = grace ? 0 : (Number.isFinite(this._level) ? this._level : 0);
            const level = Math.max(0, Math.min(1, raw));
            for (let i = 0; i < this._bars.length; i++) {
                const taper = this._taper(i);
                const phase = Math.abs(Math.sin(this._frame / 5 + i * 0.7));
                const effective = Math.min(1, Math.max(0, level * taper));
                const dynamic = effective * (0.7 + 0.3 * phase);
                const h = Math.max(BAR_BASELINE, Math.round(BAR_BASELINE + dynamic * (maxH - BAR_BASELINE)));
                this._bars[i].set_height(h);
                this._bars[i].opacity = 255;
            }
        } else if (this._state === 'transcribing') {
            const cycle = BAR_COUNT * 2 - 2;
            const pos = Math.floor(this._frame / 3) % Math.max(1, cycle);
            const active = pos < BAR_COUNT ? pos : cycle - pos;
            for (let i = 0; i < this._bars.length; i++) {
                const taper = this._taper(i);
                const dist = Math.abs(i - active);
                const intensity = Math.max(0.15, Math.exp(-(dist * dist) / 4));
                const dynamic = intensity * taper;
                const h = Math.max(BAR_BASELINE, Math.round(BAR_BASELINE + dynamic * (maxH - BAR_BASELINE) * 0.85));
                this._bars[i].set_height(h);
                this._bars[i].opacity = Math.round(255 * (0.3 + 0.7 * intensity));
            }
        }
    }

    _setLevel(level) {
        const numeric = Number(level);
        if (!Number.isFinite(numeric)) return;
        this._targetLevel = Math.max(0, Math.min(1, numeric));
    }
}
