// Dalker remote-session border for GNOME Shell (Wayland has no other way to draw a
// click-through, always-on-top frame).
//
// D-Bus: name io.github.ivanmakhnyk.Dalker, object /io/github/ivanmakhnyk/Dalker/Border,
//   interface io.github.ivanmakhnyk.Dalker.Border
//   SetActive(b)   true = show the frame (the agent re-sends true about once a second);
//                  the frame hides itself 3 s after the last true, or immediately on false.
//   Active (b)     current state.
//
//   ClipboardSerial() -> u      bumped whenever the clipboard changes
//   GetClipboardText() -> s     clipboard text ("" when it holds none)
//   SetClipboardText(s)
//                  The agent shares the clipboard with viewers through these: under Wayland
//                  nothing outside the compositor can reach it. Reading and writing it only
//                  works while the frame is shown, i.e. while someone is viewing the desktop.
//
//   TypeText(s)    Types characters from a viewer's "Direct text" keyboard mode, whatever
//                  the keyboard layout here is: committed through the input method to the
//                  focused text field, or, for apps that take no input-method text (X11
//                  apps), pressed as keysyms on a virtual keyboard (those must exist in the
//                  current layout). Also only while the frame is shown.
//
// It also runs on the lock screen ("session-modes"), so a connected viewer can see and unlock it.

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BORDER_PX = 5;
const TIMEOUT_MS = 3000;
const INACTIVE_ERROR = 'io.github.ivanmakhnyk.Dalker.Border.Error.Inactive';

const IFACE = `<node>
  <interface name="io.github.ivanmakhnyk.Dalker.Border">
    <method name="SetActive">
      <arg type="b" direction="in" name="active"/>
    </method>
    <property name="Active" type="b" access="read"/>
    <method name="ClipboardSerial">
      <arg type="u" direction="out" name="serial"/>
    </method>
    <method name="GetClipboardText">
      <arg type="s" direction="out" name="text"/>
    </method>
    <method name="SetClipboardText">
      <arg type="s" direction="in" name="text"/>
    </method>
    <method name="TypeText">
      <arg type="s" direction="in" name="text"/>
    </method>
  </interface>
</node>`;

export default class DalkerBorder extends Extension {
    enable() {
        this._active = false;
        this._released = false;
        this._edges = [];
        this._timeoutId = 0;

        this._buildEdges();
        this._monitorsChangedId = Main.layoutManager.connect('monitors-changed', () => this._buildEdges());

        this._clipSerial = 0;
        this._selection = global.display.get_selection();
        this._ownerChangedId = this._selection.connect('owner-changed', (_sel, type) => {
            if (type === Meta.SelectionType.SELECTION_CLIPBOARD)
                this._clipSerial = (this._clipSerial + 1) >>> 0;
        });

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, '/io/github/ivanmakhnyk/Dalker/Border');
        this._ownerId = Gio.bus_own_name(
            Gio.BusType.SESSION, 'io.github.ivanmakhnyk.Dalker', Gio.BusNameOwnerFlags.NONE, null, null, null);
    }

    disable() {
        if (this._ownerId) {
            Gio.bus_unown_name(this._ownerId);
            this._ownerId = 0;
        }
        if (this._dbus) {
            this._dbus.unexport();
            this._dbus = null;
        }
        if (this._monitorsChangedId) {
            Main.layoutManager.disconnect(this._monitorsChangedId);
            this._monitorsChangedId = 0;
        }
        if (this._ownerChangedId) {
            this._selection.disconnect(this._ownerChangedId);
            this._ownerChangedId = 0;
            this._selection = null;
        }
        this._clearTimeout();
        this._active = false;
        this._updateRemoteAccess();
        this._destroyEdges();
        this._keyboard = null;
    }

    // --- D-Bus interface ---------------------------------------------------------------

    get Active() {
        return this._active;
    }

    SetActive(active) {
        this._clearTimeout();
        this._active = !!active;
        for (const e of this._edges)
            e.visible = this._active;
        if (this._active) {
            this._raise();
            // Dead-man switch: no ping for 3 s (agent crashed or stopped) hides the frame.
            this._timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, TIMEOUT_MS, () => {
                this._timeoutId = 0;
                this._active = false;
                for (const e of this._edges)
                    e.visible = false;
                this._updateRemoteAccess();
                return GLib.SOURCE_REMOVE;
            });
        }
        this._updateRemoteAccess();
    }

    ClipboardSerial() {
        return this._clipSerial;
    }

    GetClipboardTextAsync(_params, invocation) {
        if (!this._active) {
            invocation.return_dbus_error(INACTIVE_ERROR, 'nobody is viewing the desktop');
            return;
        }
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_clip, text) => {
            invocation.return_value(new GLib.Variant('(s)', [text ?? '']));
        });
    }

    SetClipboardTextAsync([text], invocation) {
        if (!this._active) {
            invocation.return_dbus_error(INACTIVE_ERROR, 'nobody is viewing the desktop');
            return;
        }
        St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
        invocation.return_value(null);
    }

    TypeTextAsync([text], invocation) {
        if (!this._active) {
            invocation.return_dbus_error(INACTIVE_ERROR, 'nobody is viewing the desktop');
            return;
        }
        const im = Main.inputMethod;
        if (im?.currentFocus) {
            im.commit(text);
        } else {
            this._keyboard ??= global.stage.context.get_backend().get_default_seat()
                .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
            for (const ch of text) {
                const keyval = Clutter.unicode_to_keysym(ch.codePointAt(0));
                const time = GLib.get_monotonic_time();
                this._keyboard.notify_keyval(time, keyval, Clutter.KeyState.PRESSED);
                this._keyboard.notify_keyval(time, keyval, Clutter.KeyState.RELEASED);
            }
        }
        invocation.return_value(null);
    }

    // --- lock screen -------------------------------------------------------------------

    // GNOME Shell inhibits remote access while locked, which ends every screen cast. While a
    // Dalker viewer is connected (and the frame is shown above the lock screen), lift that
    // so the agent can stream the lock screen and the viewer can unlock it; restore it as
    // soon as nobody is viewing. On unlock the shell lifts its own inhibition (a no-op once
    // we have lifted it).
    _updateRemoteAccess() {
        const controller = global.backend.get_remote_access_controller();
        if (!controller)
            return;
        const locked = Main.sessionMode.isLocked;
        const want = locked && this._active;
        if (want && !this._released) {
            controller.uninhibit_remote_access();
            this._released = true;
        } else if (!want && this._released) {
            if (locked)
                controller.inhibit_remote_access();
            this._released = false;
        }
    }

    // --- drawing -----------------------------------------------------------------------

    // The lock screen is streamed too ("session-modes" keeps this extension on there), so the
    // frame must sit above it: move the edges to the top of their layer.
    _raise() {
        for (const e of this._edges) {
            const parent = e.get_parent();
            if (parent)
                parent.set_child_above_sibling(e, null);
        }
    }

    _clearTimeout() {
        if (this._timeoutId) {
            GLib.source_remove(this._timeoutId);
            this._timeoutId = 0;
        }
    }

    _destroyEdges() {
        for (const e of this._edges) {
            Main.layoutManager.removeChrome(e);
            e.destroy();
        }
        this._edges = [];
    }

    _buildEdges() {
        this._destroyEdges();
        const t = BORDER_PX;
        for (const m of Main.layoutManager.monitors) {
            const rects = [
                [m.x, m.y, m.width, t],                         // top
                [m.x, m.y + m.height - t, m.width, t],          // bottom
                [m.x, m.y + t, t, m.height - 2 * t],            // left
                [m.x + m.width - t, m.y + t, t, m.height - 2 * t], // right
            ];
            for (const [x, y, width, height] of rects) {
                const edge = new St.Widget({
                    style: 'background-color: rgb(255, 0, 0);',
                    reactive: false,
                    can_focus: false,
                    track_hover: false,
                    x, y, width, height,
                    visible: this._active,
                });
                // Above windows, stays visible over fullscreen apps. The widget is not
                // reactive, so pointer events fall through to whatever is underneath.
                Main.layoutManager.addTopChrome(edge, {
                    affectsStruts: false,
                    trackFullscreen: false,
                });
                this._edges.push(edge);
            }
        }
    }
}
