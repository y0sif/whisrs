// Narrow session API for uinput typing. No Eval, settings writes or key injection.
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import * as Keyboard from 'resource:///org/gnome/shell/ui/status/keyboard.js';
import * as KeyboardManager from 'resource:///org/gnome/shell/misc/keyboardManager.js';

const XML = `<node><interface name="org.whisrs.Keyboard">
<method name="Snapshot"><arg type="ay" direction="out"/></method>
<method name="Select"><arg type="s" direction="in"/><arg type="ay" direction="out"/></method>
</interface></node>`;

export class KeyboardControl {
    constructor() {
        this._pending = Promise.resolve();
        this._object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._object.export(Gio.DBus.session, '/org/whisrs/Keyboard');
    }

    destroy() {
        this._object.unexport();
    }

    Snapshot() {
        return new TextEncoder().encode(this._snapshot());
    }

    _snapshot(requireCurrent = true) {
        const manager = Keyboard.getInputSourceManager();
        const keyboard = KeyboardManager.getKeyboardManager();
        // GNOME 50's async apply completes only after Mutter has changed the
        // keymap/group. Older APIs cannot provide this acknowledgement here.
        if (typeof keyboard._doApply !== 'function' || keyboard.isLocked() || keyboard.isExternal())
            throw new Error('Acknowledged XKB switching unavailable');
        const sources = Object.values(manager.inputSources).map(source => {
            if (source.type !== 'xkb')
                throw new Error('IBus input methods require the legacy input path');
            const info = keyboard._layoutInfos[source.xkbId];
            if (!info)
                throw new Error('Missing XKB source');
            const [layouts, variants] = keyboard._buildGroupStrings(info.group);
            return {
                id: source.id, layouts, variants, group: info.groupIndex,
                model: keyboard._xkbModel, options: keyboard._buildOptionsString(),
            };
        });
        if (!manager.currentSource || (requireCurrent && keyboard.currentLayout?.id !== manager.currentSource.xkbId))
            throw new Error('Input source change in progress');
        return JSON.stringify({current: manager.currentSource.id, sources});
    }

    SelectAsync(parameters, invocation) {
        // A client timeout does not cancel Mutter's apply. Queue restores behind
        // that apply so a late completion cannot overwrite the original source.
        const select = () => this._select(parameters, invocation);
        this._pending = this._pending.then(select, select);
        return this._pending;
    }

    async _select([id], invocation) {
        try {
            // A failed previous apply may have left the source manager and
            // Mutter out of step. Permit a restore to repair that state.
            this._snapshot(false);
            const manager = Keyboard.getInputSourceManager();
            const source = Object.values(manager.inputSources).find(s => s.type === 'xkb' && s.id === id);
            if (!source)
                throw new Error('Unknown XKB source');
            const keyboard = KeyboardManager.getKeyboardManager();
            if (manager.currentSource !== source || keyboard.currentLayout?.id !== source.xkbId) {
                // Apply first, then update the source manager. Its activate
                // handler reuses the already applied map, avoiding two
                // concurrent Mutter operations for the same switch.
                await keyboard._doApply(source.xkbId);
                source.activate(false);
            }
            invocation.return_value(new GLib.Variant('(ay)', [this.Snapshot()]));
        } catch (error) {
            invocation.return_dbus_error('org.whisrs.Keyboard.Unavailable', String(error));
        }
    }
}
