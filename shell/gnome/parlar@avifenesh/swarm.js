// The swarm: forty fireflies whose motion shows the conversation state. Blue is the user, amber
// is the agent, red is a failed tool call. Same model as design/indicator-lab.html.

import Cairo from 'cairo';

const YOU = [143, 216, 255];
const AGENT = [255, 191, 105];
const NEUTRAL = [246, 234, 208];
const ERR = [255, 90, 79];
const WHITE = [255, 255, 255];
const N = 40;

function hash(n) {
    const x = Math.sin(n * 127.1 + 311.7) * 43758.5453;
    return x - Math.floor(x);
}
const lerp = (a, b, k) => a + (b - a) * k;
const mix = (a, b, k) => [lerp(a[0], b[0], k), lerp(a[1], b[1], k), lerp(a[2], b[2], k)];

export class Swarm {
    constructor() {
        this.p = [];
        for (let i = 0; i < N; i++) {
            const a = hash(i * 3.7) * Math.PI * 2;
            this.p.push({
                x: Math.cos(a) * 1.4, y: Math.sin(a) * 1.4, ph: hash(i * 1.9) * Math.PI * 2,
                r: hash(i * 9.1), flare: 0, err: false, a: 0, col: NEUTRAL.slice(),
            });
        }
        this.modeSince = 0;
        this.mode = 'connecting';
    }

    setMode(mode, now) {
        if (mode !== this.mode) {
            this.mode = mode;
            this.modeSince = now;
        }
    }

    flare(err) {
        const p = this.p[Math.floor(Math.random() * N)];
        p.flare = 1;
        p.err = err;
    }

    /** True while anything still moves, so the caller can stop ticking when settled. */
    get busy() {
        return this.mode !== 'stopped' || this.p.some(p => p.flare > 0 || Math.abs(p.a - 0.08) > 0.01);
    }

    step(dt, t, lv, al, voiceOff) {
        const m = this.mode;
        const s = t - this.modeSince;
        for (let i = 0; i < N; i++) {
            const p = this.p[i];
            let tgt, k = 4, jit = 0.04, alpha = 0.85, col = NEUTRAL;
            if (m === 'idle') {
                const a = p.ph + t * 0.12, rr = 0.5 + 0.18 * Math.sin(p.ph * 3 + t * 0.4);
                tgt = [Math.cos(a) * rr, Math.sin(a) * rr * 0.9];
                k = 1.5; jit = 0.06; alpha = 0.7;
            } else if (m === 'connecting') {
                const on = s > i * 0.035;
                const a = p.ph + t * 0.12;
                tgt = on ? [Math.cos(a) * 0.55, Math.sin(a) * 0.5] : [Math.cos(p.ph) * 1.4, Math.sin(p.ph) * 1.4];
                k = 3; alpha = on ? 0.8 : 0;
            } else if (m === 'listening') {
                tgt = cluster(p, t, lv);
                k = 9; jit = 0.02 + lv * 0.12; col = YOU; alpha = 0.95;
            } else if (m === 'thinking') {
                const a = i / N * Math.PI * 2 + t * 2.2, rr = 0.36 + 0.04 * Math.sin(t * 3 + i);
                tgt = [Math.cos(a) * rr, Math.sin(a) * rr];
                k = 10; jit = 0.005;
            } else if (m === 'speaking') {
                tgt = line(i, t, al, N);
                k = 14; jit = 0.005; col = AGENT; alpha = voiceOff ? 0.4 : 0.95;
            } else if (m === 'both') {
                if (i % 2) {
                    tgt = cluster(p, t, lv);
                    col = YOU;
                } else {
                    tgt = line(i / 2, t, al, N / 2);
                    col = AGENT;
                }
                k = 10; jit = 0.03;
            } else if (m === 'muted') {
                tgt = [-0.7 + 1.4 * i / (N - 1), 0.62 + 0.06 * p.r];
                k = 3; jit = 0.003; alpha = 0.3;
            } else {
                tgt = [-0.7 + 1.4 * i / (N - 1), 0.7];
                k = 2; jit = 0; alpha = 0.08;
            }
            const kk = Math.min(1, dt * k);
            p.x = lerp(p.x, tgt[0], kk) + Math.sin(t * 3.1 + p.ph * 7) * jit * dt * 30;
            p.y = lerp(p.y, tgt[1], kk) + Math.cos(t * 2.7 + p.ph * 5) * jit * dt * 30;
            p.a = lerp(p.a, alpha, Math.min(1, dt * 4));
            p.col = mix(p.col, col, Math.min(1, dt * 5));
            p.flare = Math.max(0, p.flare - dt * 1.3);
        }
    }

    draw(cr, w, h, muted) {
        const c = Math.min(w, h) / 2, u = Math.min(w, h) * 0.42, px = Math.min(w, h) / 104;
        cr.setOperator(Cairo.Operator.CLEAR);
        cr.paint();
        cr.setOperator(Cairo.Operator.OVER);
        // a pocket of night behind the fireflies so they read on light windows too
        const night = new Cairo.RadialGradient(c, c, 0, c, c, c);
        const depth = this.mode === 'stopped' ? 0.18 : 0.42;
        night.addColorStopRGBA(0, 0.05, 0.06, 0.09, depth);
        night.addColorStopRGBA(0.7, 0.05, 0.06, 0.09, depth * 0.6);
        night.addColorStopRGBA(1, 0.05, 0.06, 0.09, 0);
        cr.setSource(night);
        cr.arc(c, c, c, 0, Math.PI * 2);
        cr.fill();
        cr.setOperator(Cairo.Operator.ADD);
        for (const p of this.p) {
            if (p.a < 0.005)
                continue;
            const x = c + p.x * u, y = c + p.y * u;
            const fc = p.flare > 0 ? mix(p.col, p.err ? ERR : WHITE, p.flare) : p.col;
            const halo = (4.5 + p.flare * 12) * px;
            const g = new Cairo.RadialGradient(x, y, 0, x, y, halo);
            g.addColorStopRGBA(0, fc[0] / 255, fc[1] / 255, fc[2] / 255, p.a);
            g.addColorStopRGBA(0.25, fc[0] / 255, fc[1] / 255, fc[2] / 255, p.a * 0.45);
            g.addColorStopRGBA(1, fc[0] / 255, fc[1] / 255, fc[2] / 255, 0);
            cr.setSource(g);
            cr.arc(x, y, halo, 0, Math.PI * 2);
            cr.fill();
        }
        cr.setOperator(Cairo.Operator.OVER);
        if (muted) {
            cr.setSourceRGBA(ERR[0] / 255, ERR[1] / 255, ERR[2] / 255, 0.75);
            cr.setLineWidth(1.6 * px);
            cr.setLineCap(Cairo.LineCap.ROUND);
            cr.moveTo(c - u * 0.25, c + u * 0.86);
            cr.lineTo(c + u * 0.25, c + u * 0.86);
            cr.stroke();
        }
    }
}

function cluster(p, t, lv) {
    const a = p.ph + t * 0.6;
    const rr = 0.2 + 0.12 * p.r + lv * 0.5 * (0.5 + 0.5 * Math.sin(p.ph * 5 + t * 9));
    return [Math.cos(a) * rr, Math.sin(a) * rr];
}

function line(i, t, al, n) {
    const x = -0.85 + 1.7 * i / (n - 1);
    return [x, Math.sin(x * 6 - t * 7) * al * 0.5 * (1 - Math.abs(x) * 0.5)];
}
