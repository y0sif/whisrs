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
const OVERLAY_HEIGHT = 40;
const BOTTOM_MARGIN = 16;
const BAR_COUNT = 7;
const BAR_W = 3;
const BAR_GAP = 2;
const BAR_BASELINE = 6;
const BAR_VPAD = 6;

// Spawn animation: pill height morphs from a 4-px sliver to its full
// height, anchored to the bottom of its placement. Slight overshoot via
// EASE_OUT_BACK for a "physical arrival" pop.
const SPAWN_IN_MS = 220;
const SPAWN_OUT_MS = 140;
const SPAWN_PILL_MIN_H = 4;
const BARS_GRACE_MS = 80;
const BARS_FADE_MS = 80;

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
        this._levelVelocity = 0;
        this._lastUpdateMs = 0;
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

    /// Pill "draws out" from a thin line at the bottom edge, growing to
    /// full height with EASE_OUT_BACK overshoot. Bars stay invisible
    /// during the grow, then fade in once the pill is mostly settled.
    _spawnIn() {
        if (!this._actor) return;

        // Snap to the start state.
        this._actor.set_easing_duration(0);
        this._actor.set_pivot_point(0.5, 1.0); // bottom-center
        this._actor.set_scale(1.0, SPAWN_PILL_MIN_H / OVERLAY_HEIGHT);
        this._actor.opacity = 0;
        if (this._barsBox) this._barsBox.opacity = 0;

        // Then animate to full size — Clutter's EASE_OUT_BACK gives the
        // small overshoot that makes the arrival feel physical.
        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_OUT_BACK);
        this._actor.set_easing_duration(SPAWN_IN_MS);
        this._actor.set_scale(1.0, 1.0);

        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_OUT_QUAD);
        this._actor.set_easing_duration(Math.round(SPAWN_IN_MS * 0.64));
        this._actor.opacity = 255;

        // Bars: grace, then a quick fade-in once the pill is mostly grown.
        this._barsGraceUntil = Date.now() + BARS_GRACE_MS + BARS_FADE_MS;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, BARS_GRACE_MS, () => {
            if (this._barsBox && this._state !== 'idle') {
                this._barsBox.set_easing_mode(Clutter.AnimationMode.EASE_OUT_QUAD);
                this._barsBox.set_easing_duration(BARS_FADE_MS);
                this._barsBox.opacity = 255;
            }
            return GLib.SOURCE_REMOVE;
        });
    }

    /// Reverse: collapse height back to a sliver while fading out, then
    /// hide. EASE_IN_CUBIC for a sharp accelerated dismiss.
    _spawnOut() {
        if (!this._actor) return;

        if (this._barsBox) {
            this._barsBox.set_easing_mode(Clutter.AnimationMode.EASE_IN_QUAD);
            this._barsBox.set_easing_duration(Math.round(SPAWN_OUT_MS * 0.7));
            this._barsBox.opacity = 0;
        }

        this._actor.set_easing_mode(Clutter.AnimationMode.EASE_IN_CUBIC);
        this._actor.set_easing_duration(SPAWN_OUT_MS);
        this._actor.set_scale(1.0, SPAWN_PILL_MIN_H / OVERLAY_HEIGHT);
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
        const maxH = OVERLAY_HEIGHT - BAR_VPAD * 2;
        if (this._barsBox) {
            this._barsBox.set_position(barsX, cy - Math.floor(maxH / 2));
            this._barsBox.set_size(barBlock, maxH);
        }
    }

    _startAnimation() {
        if (this._animationId) return;

        // Critically-damped-ish spring on the displayed level, stepped
        // with real wall-clock dt so the time constants are real-world ms,
        // not "per tick". Tuned to settle in ~150–200 ms with no
        // perceptible overshoot — the bars *track* the voice rather than
        // chase it. ~16 ms tick ≈ 60 fps.
        const STIFFNESS = 360;
        const DAMPING = 32;
        this._lastUpdateMs = GLib.get_monotonic_time() / 1000;
        this._animationId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 16, () => {
            this._frame++;
            const nowMs = GLib.get_monotonic_time() / 1000;
            const dt = Math.min(0.1, Math.max(0, (nowMs - this._lastUpdateMs) / 1000));
            this._lastUpdateMs = nowMs;
            const target = this._targetLevel ?? 0;
            if (dt > 0) {
                const force = (target - this._level) * STIFFNESS;
                const drag = this._levelVelocity * DAMPING;
                this._levelVelocity += (force - drag) * dt;
                this._level = Math.max(0, Math.min(1, this._level + this._levelVelocity * dt));
            }
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
        this._levelVelocity = 0;
        this._level = 0;
    }

    /// Wavy taper across the bar row — center bar at ~100 %, with a cosine
    /// modulation so adjacent bars alternate between "taller" and "shorter"
    /// inside a gaussian envelope. Reads as an equalizer pattern instead
    /// of a smooth bell.
    _taper(i) {
        if (BAR_COUNT <= 1) return 1;
        const center = (BAR_COUNT - 1) / 2;
        const d = (i - center) / center;
        const envelope = Math.exp(-d * d);
        const wave = 0.75 + 0.25 * Math.cos(Math.PI * (i - center));
        return envelope * wave;
    }

    _updateBars() {
        if (!this._bars || this._bars.length === 0) return;

        const maxH = OVERLAY_HEIGHT - 10;

        if (this._state === 'recording') {
            const grace = this._barsGraceUntil && Date.now() < this._barsGraceUntil;
            const raw = grace ? 0 : (Number.isFinite(this._level) ? this._level : 0);
            const level = Math.max(0, Math.min(1, raw));
            // Pure level-driven: each bar stays anchored at the pill
            // center and grows symmetrically. No per-frame phase wobble.
            for (let i = 0; i < this._bars.length; i++) {
                const taper = this._taper(i);
                const effective = Math.min(1, Math.max(0, level * taper));
                const h = Math.max(BAR_BASELINE, Math.round(BAR_BASELINE + effective * (maxH - BAR_BASELINE)));
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
