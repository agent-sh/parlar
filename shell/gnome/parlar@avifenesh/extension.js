// parlar indicator: the swarm, floating above every window. Click to stop or start the
// conversation, hover for the mute button, right-click for devices and to close the indicator.

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import {Link, request} from './daemon.js';
import {Swarm} from './swarm.js';

const SIZE = 104;
const FRAME_MS = 33;
// how long a lost daemon shows as connecting before it shows as stopped
const CONNECTING_GRACE_S = 8;

const MODES = {
    stopped: 'stopped', connecting: 'connecting', ready: 'idle', listening: 'listening',
    working: 'thinking', speaking: 'speaking', interrupting: 'both',
};

function configPath() {
    return GLib.build_filenamev([GLib.get_user_config_dir(), 'parlar', 'indicator.json']);
}

function loadPosition() {
    try {
        const [ok, bytes] = GLib.file_get_contents(configPath());
        if (ok)
            return JSON.parse(new TextDecoder().decode(bytes));
    } catch (_) {}
    return null;
}

// a saved spot can be off screen after a monitor was unplugged or rearranged
function clampToMonitors(x, y) {
    const monitors = Main.layoutManager.monitors;
    const cx = x + SIZE / 2, cy = y + SIZE / 2;
    const mon = monitors.find(m => cx >= m.x && cx < m.x + m.width && cy >= m.y && cy < m.y + m.height) ??
        Main.layoutManager.primaryMonitor;
    const clamp = (v, lo, len) => Math.max(lo, Math.min(v, lo + Math.max(0, len - SIZE)));
    return [clamp(x, mon.x, mon.width), clamp(y, mon.y, mon.height)];
}

function savePosition(x, y) {
    try {
        GLib.mkdir_with_parents(GLib.path_get_dirname(configPath()), 0o700);
        GLib.file_set_contents(configPath(), JSON.stringify({x, y}));
    } catch (e) {
        logError(e, 'parlar: could not save position');
    }
}

class Indicator {
    constructor(uuid) {
        this._uuid = uuid;
        this._phase = 'connecting';
        this._muted = false;
        this._voiceOff = false;
        this._connected = false;
        this._lostAt = now();
        this._user = 0;
        this._agent = 0;
        this._swarm = new Swarm();
        this._tickId = 0;
        this._graceId = 0;
        this._destroyed = false;
        this._last = now();

        this.actor = new St.Widget({
            reactive: true, track_hover: true, can_focus: true,
            width: SIZE, height: SIZE,
            accessible_name: 'parlar voice indicator',
        });
        this._area = new St.DrawingArea({width: SIZE, height: SIZE});
        this._area.connect('repaint', a => this._repaint(a));
        this.actor.add_child(this._area);

        this._muteIcon = new St.Icon({icon_name: 'microphone-sensitive-symbolic', icon_size: 12});
        this._muteBtn = new St.Button({
            style_class: 'parlar-mute', child: this._muteIcon, reactive: true, can_focus: true,
            x: SIZE - 26, y: SIZE - 26, opacity: 0, accessible_name: 'Mute microphone',
        });
        this._muteBtn.connect('clicked', () => this._setMuted(!this._muted));
        // after the button's own handling, so its press and release never reach the indicator
        // and start a drag or a toggle
        for (const sig of ['button-press-event', 'button-release-event'])
            this._muteBtn.connect_after(sig, () => Clutter.EVENT_STOP);
        this.actor.add_child(this._muteBtn);
        this.actor.connect('notify::hover', () => this._showMute());

        this.actor.connect('button-press-event', (_a, e) => this._press(e));
        this.actor.connect('motion-event', (_a, e) => this._motion(e));
        this.actor.connect('button-release-event', (_a, e) => this._release(e));
        this.actor.connect('key-press-event', (_a, e) => this._key(e));

        this._menu = new PopupMenu.PopupMenu(this.actor, 0.5, St.Side.TOP);
        Main.uiGroup.add_child(this._menu.actor);
        this._menu.actor.hide();
        this._menuManager = new PopupMenu.PopupMenuManager(this.actor);
        this._menuManager.addMenu(this._menu);

        Main.layoutManager.addTopChrome(this.actor, {trackFullscreen: true});
        const pos = loadPosition();
        const mon = Main.layoutManager.primaryMonitor;
        const [x, y] = Number.isFinite(pos?.x) && Number.isFinite(pos?.y)
            ? clampToMonitors(pos.x, pos.y)
            : [mon.x + mon.width - SIZE - 40, mon.y + 60];
        this.actor.set_position(x, y);

        this._link = new Link(ev => this._event(ev), on => this._onLink(on));
        this._link.start();
        this._wake();
    }

    destroy() {
        this._destroyed = true;
        this._link.stop();
        if (this._tickId)
            GLib.source_remove(this._tickId);
        this._tickId = 0;
        this._clearGrace();
        this._grab?.dismiss();
        this._grab = null;
        this._menu.destroy();
        Main.layoutManager.removeChrome(this.actor);
        this.actor.destroy();
    }

    // ---- daemon events ----

    _onLink(connected) {
        this._connected = connected;
        this._clearGrace();
        if (!connected) {
            this._lostAt = now();
            this._user = this._agent = 0;
            // the tick stops once the swarm settles; wake it when connecting turns into stopped
            this._graceId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, CONNECTING_GRACE_S, () => {
                this._graceId = 0;
                this._wake();
                return GLib.SOURCE_REMOVE;
            });
        }
        this._wake();
    }

    _clearGrace() {
        if (this._graceId)
            GLib.source_remove(this._graceId);
        this._graceId = 0;
    }

    _event(ev) {
        switch (ev.ui) {
        case 'phase':
            this._phase = ev.phase;
            this._muted = ev.mic_muted;
            this._voiceOff = ev.voice_off;
            this._muteIcon.icon_name = this._muted ? 'microphone-disabled-symbolic' : 'microphone-sensitive-symbolic';
            this._muteBtn.accessible_name = this._muted ? 'Unmute microphone' : 'Mute microphone';
            this._showMute();
            break;
        case 'levels':
            this._user = Math.max(this._user, ev.user);
            this._agent = Math.max(this._agent, ev.agent);
            break;
        case 'tool':
            this._swarm.flare(!ev.ok);
            break;
        }
        this._wake();
    }

    _mode() {
        if (!this._connected)
            return now() - this._lostAt < CONNECTING_GRACE_S ? 'connecting' : 'stopped';
        if (this._muted && (this._phase === 'ready' || this._phase === 'listening'))
            return 'muted';
        return MODES[this._phase] ?? 'idle';
    }

    // ---- animation ----

    _wake() {
        if (this._tickId)
            return;
        this._last = now();
        this._tickId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, FRAME_MS, () => this._tick());
    }

    _tick() {
        const t = now();
        const dt = Math.min(0.05, t - this._last);
        this._last = t;
        this._swarm.setMode(this._mode(), t);
        this._swarm.step(dt, t, this._user, this._agent, this._voiceOff);
        // levels arrive only while there is sound; let them fall between events
        this._user *= 0.85;
        this._agent *= 0.85;
        this._area.queue_repaint();
        // settled: an event or the connecting grace timeout wakes the tick again
        if (!this._swarm.busy) {
            this._tickId = 0;
            return GLib.SOURCE_REMOVE;
        }
        return GLib.SOURCE_CONTINUE;
    }

    _repaint(area) {
        const cr = area.get_context();
        const [w, h] = area.get_surface_size();
        this._swarm.draw(cr, w, h, this._muted && this._connected);
        cr.$dispose();
    }

    _showMute() {
        const show = this.actor.hover || this._muted || this._muteBtn.has_key_focus();
        this._muteBtn.ease({opacity: show ? 255 : 0, duration: 150, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
    }

    // ---- input ----

    _press(e) {
        const button = e.get_button();
        if (button === Clutter.BUTTON_SECONDARY) {
            this._openMenu();
            return Clutter.EVENT_STOP;
        }
        if (button !== Clutter.BUTTON_PRIMARY)
            return Clutter.EVENT_PROPAGATE;
        const [x, y] = e.get_coords();
        const [ax, ay] = this.actor.get_position();
        this._drag = {x, y, ax, ay, moved: false};
        // a press without its release (the pointer left mid-drag) must not leave a grab behind
        this._grab?.dismiss();
        this._grab = global.stage.grab(this.actor);
        return Clutter.EVENT_STOP;
    }

    _motion(e) {
        if (!this._drag)
            return Clutter.EVENT_PROPAGATE;
        const [x, y] = e.get_coords();
        const dx = x - this._drag.x, dy = y - this._drag.y;
        if (Math.abs(dx) + Math.abs(dy) > 4)
            this._drag.moved = true;
        if (this._drag.moved)
            this.actor.set_position(Math.round(this._drag.ax + dx), Math.round(this._drag.ay + dy));
        return Clutter.EVENT_STOP;
    }

    _release(e) {
        if (!this._drag || e.get_button() !== Clutter.BUTTON_PRIMARY)
            return Clutter.EVENT_PROPAGATE;
        this._grab?.dismiss();
        this._grab = null;
        const moved = this._drag.moved;
        this._drag = null;
        if (moved) {
            const [x, y] = this.actor.get_position();
            savePosition(x, y);
        } else {
            this._toggleActive();
        }
        return Clutter.EVENT_STOP;
    }

    _key(e) {
        const sym = e.get_key_symbol();
        if (sym === Clutter.KEY_Return || sym === Clutter.KEY_space) {
            this._toggleActive();
            return Clutter.EVENT_STOP;
        }
        if (sym === Clutter.KEY_m || sym === Clutter.KEY_M) {
            this._setMuted(!this._muted);
            return Clutter.EVENT_STOP;
        }
        const shift = (e.get_state() & Clutter.ModifierType.SHIFT_MASK) !== 0;
        if (sym === Clutter.KEY_Menu || (shift && sym === Clutter.KEY_F10)) {
            this._openMenu();
            return Clutter.EVENT_STOP;
        }
        return Clutter.EVENT_PROPAGATE;
    }

    // ---- actions ----

    _send(req) {
        request(req).then(r => {
            if (r.kind === 'error')
                Main.notify('parlar', r.message);
        }).catch(() => Main.notify('parlar', 'parlard is not running.'));
    }

    _toggleActive() {
        if (!this._connected) {
            Main.notify('parlar', 'parlard is not running. Start it with: parlar daemon');
            return;
        }
        const starting = this._phase === 'stopped';
        if (!starting) {
            this._send({op: 'set', active: false});
            return;
        }
        request({op: 'state'}).then(st => {
            if (!(st.sessions ?? []).some(x => x.focused)) {
                Main.notify('parlar', 'Pick a session first: run /parlar:talk in it, or choose one under Talk to.');
                return;
            }
            this._send({op: 'set', active: true});
        }).catch(() => Main.notify('parlar', 'parlard is not running.'));
    }

    _setMuted(muted) {
        this._send({op: 'set', mic_muted: muted});
    }

    // a PopupMenu refuses to open while empty, so fill it before opening
    async _openMenu() {
        if (this._menu.isOpen) {
            this._menu.close();
            return;
        }
        await this._fillMenu();
        // the extension may have been disabled while the daemon answered
        if (this._destroyed)
            return;
        this._menu.open();
    }

    async _fillMenu() {
        this._menu.removeAll();
        if (!this._connected) {
            this._menu.addMenuItem(new PopupMenu.PopupMenuItem('parlard is not running', {reactive: false}));
            this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
            this._addClose();
            return;
        }
        let devs = {inputs: [], outputs: []};
        let state = {sessions: []};
        try {
            [devs, state] = await Promise.all([request({op: 'devices'}), request({op: 'state'})]);
        } catch (_) {}
        if (this._destroyed)
            return;
        const sessions = (state.sessions ?? []).filter(x => x.session);
        if (sessions.length) {
            this._menu.addMenuItem(new PopupMenu.PopupMenuItem('Talk to', {reactive: false, style_class: 'parlar-menu-title'}));
            for (const x of sessions) {
                const folder = x.cwd ? GLib.path_get_basename(x.cwd) : 'session';
                const item = new PopupMenu.PopupMenuItem(`${folder} (${x.harness})`);
                item.setOrnament(x.focused ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);
                item.connect('activate', () => this._send({op: 'set', focus: x.session}));
                this._menu.addMenuItem(item);
            }
            this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        }
        const section = (title, list, key) => {
            this._menu.addMenuItem(new PopupMenu.PopupMenuItem(title, {reactive: false, style_class: 'parlar-menu-title'}));
            if (!list.length)
                this._menu.addMenuItem(new PopupMenu.PopupMenuItem('No devices', {reactive: false}));
            for (const d of list) {
                const item = new PopupMenu.PopupMenuItem(d.name);
                item.setOrnament(d.current ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NONE);
                item.connect('activate', () => this._send({op: 'set', [key]: d.id}));
                this._menu.addMenuItem(item);
            }
        };
        section('Input', devs.inputs ?? [], 'input');
        section('Output', devs.outputs ?? [], 'output');
        this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const mute = new PopupMenu.PopupSwitchMenuItem('Mute mic', this._muted);
        mute.connect('toggled', (_i, on) => this._setMuted(on));
        this._menu.addMenuItem(mute);
        const voice = new PopupMenu.PopupSwitchMenuItem('Voice off (text only)', this._voiceOff);
        voice.connect('toggled', (_i, on) => this._send({op: 'set', voice_off: on}));
        this._menu.addMenuItem(voice);
        this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const stopped = this._phase === 'stopped';
        const stop = new PopupMenu.PopupMenuItem(stopped ? 'Start conversation' : 'Stop conversation');
        stop.connect('activate', () => this._send({op: 'set', active: stopped}));
        this._menu.addMenuItem(stop);
        this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._addClose();
    }

    // Close = disable the extension, so it stays away until `gnome-extensions enable` or the
    // Extensions app brings it back. The conversation itself is not touched.
    _addClose() {
        const close = new PopupMenu.PopupMenuItem('Close indicator');
        close.connect('activate', () => Main.extensionManager.disableExtension(this._uuid));
        this._menu.addMenuItem(close);
    }
}

function now() {
    return GLib.get_monotonic_time() / 1e6;
}

export default class ParlarExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this.uuid);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
