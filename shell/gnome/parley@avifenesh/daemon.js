// Connection to parleyd: a subscription that reconnects, and one-shot requests.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';

const enc = new TextEncoder();

export function socketPath() {
    return GLib.build_filenamev([GLib.get_user_runtime_dir(), 'parley', 'parley.sock']);
}

function address() {
    return Gio.UnixSocketAddress.new(socketPath());
}

export class Link {
    constructor(onEvent, onConnected) {
        this._onEvent = onEvent;
        this._onConnected = onConnected;
        this._stopped = false;
        this._retryId = 0;
        this._conn = null;
    }

    start() {
        this._connect();
    }

    stop() {
        this._stopped = true;
        if (this._retryId)
            GLib.source_remove(this._retryId);
        this._retryId = 0;
        this._cancel?.cancel();
        this._close();
    }

    _connect() {
        if (this._stopped)
            return;
        this._cancel = new Gio.Cancellable();
        const client = new Gio.SocketClient();
        client.connect_async(address(), this._cancel, (c, res) => {
            try {
                this._conn = c.connect_finish(res);
            } catch (_) {
                this._retry();
                return;
            }
            try {
                this._conn.get_output_stream().write_all(
                    enc.encode(`${JSON.stringify({op: 'subscribe'})}\n`), null);
            } catch (_) {
                this._drop();
                return;
            }
            this._in = new Gio.DataInputStream({
                base_stream: this._conn.get_input_stream(),
                close_base_stream: true,
            });
            this._onConnected(true);
            this._read();
        });
    }

    _read() {
        this._in.read_line_async(GLib.PRIORITY_DEFAULT, this._cancel, (s, res) => {
            let line;
            try {
                [line] = s.read_line_finish_utf8(res);
            } catch (_) {
                this._drop();
                return;
            }
            if (line === null) {
                this._drop();
                return;
            }
            try {
                this._onEvent(JSON.parse(line));
            } catch (e) {
                logError(e, 'parley: bad event');
            }
            this._read();
        });
    }

    _close() {
        try {
            this._conn?.close(null);
        } catch (_) {}
        this._conn = null;
    }

    _drop() {
        this._close();
        if (this._stopped)
            return;
        this._onConnected(false);
        this._retry();
    }

    _retry() {
        if (this._stopped || this._retryId)
            return;
        this._retryId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1500, () => {
            this._retryId = 0;
            this._connect();
            return GLib.SOURCE_REMOVE;
        });
    }
}

const REQUEST_TIMEOUT_MS = 2000;

/** Send one request and resolve with the reply, or reject after REQUEST_TIMEOUT_MS. */
export function request(req) {
    return new Promise((resolve, reject) => {
        const cancel = new Gio.Cancellable();
        let conn = null;
        let timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, REQUEST_TIMEOUT_MS, () => {
            timeoutId = 0;
            cancel.cancel();
            return GLib.SOURCE_REMOVE;
        });
        const finish = () => {
            if (timeoutId)
                GLib.source_remove(timeoutId);
            timeoutId = 0;
            try {
                conn?.close(null);
            } catch (_) {}
            conn = null;
        };
        const client = new Gio.SocketClient();
        client.connect_async(address(), cancel, (c, res) => {
            try {
                conn = c.connect_finish(res);
                conn.get_output_stream().write_all(enc.encode(`${JSON.stringify(req)}\n`), cancel);
            } catch (e) {
                finish();
                reject(e);
                return;
            }
            const din = new Gio.DataInputStream({base_stream: conn.get_input_stream()});
            din.read_line_async(GLib.PRIORITY_DEFAULT, cancel, (s, r) => {
                try {
                    const [line] = s.read_line_finish_utf8(r);
                    if (line === null)
                        throw new Error('parleyd closed the connection');
                    resolve(JSON.parse(line));
                } catch (e) {
                    reject(e);
                } finally {
                    finish();
                }
            });
        });
    });
}
