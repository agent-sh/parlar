//! The swarm: forty fireflies whose motion shows the conversation state. Blue is the user, amber
//! is the agent, red is a failed tool call. The same model as the GNOME indicator's swarm.js, so
//! the two look alike.

pub const N: usize = 40;

pub type Rgb = [f32; 3];
pub const YOU: Rgb = [143.0, 216.0, 255.0];
pub const AGENT: Rgb = [255.0, 191.0, 105.0];
pub const NEUTRAL: Rgb = [246.0, 234.0, 208.0];
pub const ERR: Rgb = [255.0, 90.0, 79.0];
pub const WHITE: Rgb = [255.0, 255.0, 255.0];

/// What the indicator shows. `Both` is the user talking over the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Stopped,
    Connecting,
    Idle,
    Listening,
    Thinking,
    Speaking,
    Both,
    Muted,
}

#[derive(Debug, Clone)]
pub struct Firefly {
    pub x: f32,
    pub y: f32,
    pub ph: f32,
    pub r: f32,
    pub flare: f32,
    pub err: bool,
    pub a: f32,
    pub col: Rgb,
}

pub struct Swarm {
    pub p: Vec<Firefly>,
    mode: Mode,
    mode_since: f32,
    /// The firefly the next flare lands on; a counter rather than randomness, so frames are
    /// reproducible.
    next_flare: usize,
}

fn hash(n: f32) -> f32 {
    let x = (n * 127.1 + 311.7).sin() * 43758.547;
    x - x.floor()
}

fn lerp(a: f32, b: f32, k: f32) -> f32 {
    a + (b - a) * k
}

fn mix(a: Rgb, b: Rgb, k: f32) -> Rgb {
    [lerp(a[0], b[0], k), lerp(a[1], b[1], k), lerp(a[2], b[2], k)]
}

fn cluster(p: &Firefly, t: f32, lv: f32) -> (f32, f32) {
    let a = p.ph + t * 0.6;
    let rr = 0.2 + 0.12 * p.r + lv * 0.5 * (0.5 + 0.5 * (p.ph * 5.0 + t * 9.0).sin());
    (a.cos() * rr, a.sin() * rr)
}

fn line(i: usize, t: f32, al: f32, n: usize) -> (f32, f32) {
    let x = -0.85 + 1.7 * i as f32 / (n - 1) as f32;
    (x, (x * 6.0 - t * 7.0).sin() * al * 0.5 * (1.0 - x.abs() * 0.5))
}

impl Default for Swarm {
    fn default() -> Self {
        Self::new()
    }
}

impl Swarm {
    pub fn new() -> Swarm {
        let p = (0..N)
            .map(|i| {
                let a = hash(i as f32 * 3.7) * std::f32::consts::TAU;
                Firefly {
                    x: a.cos() * 1.4,
                    y: a.sin() * 1.4,
                    ph: hash(i as f32 * 1.9) * std::f32::consts::TAU,
                    r: hash(i as f32 * 9.1),
                    flare: 0.0,
                    err: false,
                    a: 0.0,
                    col: NEUTRAL,
                }
            })
            .collect();
        Swarm { p, mode: Mode::Connecting, mode_since: 0.0, next_flare: 5 }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode, now: f32) {
        if mode != self.mode {
            self.mode = mode;
            self.mode_since = now;
        }
    }

    /// A tool call ended: one firefly flashes, red when the call failed.
    pub fn flare(&mut self, err: bool) {
        let p = &mut self.p[self.next_flare % N];
        p.flare = 1.0;
        p.err = err;
        self.next_flare = (self.next_flare + 13) % N;
    }

    /// True while anything still moves, so the caller can stop ticking when settled.
    pub fn busy(&self) -> bool {
        self.mode != Mode::Stopped || self.p.iter().any(|p| p.flare > 0.0 || (p.a - 0.08).abs() > 0.01)
    }

    /// Advance by `dt` seconds at time `t`. `lv` is the user's level, `al` the agent's, 0..1.
    pub fn step(&mut self, dt: f32, t: f32, lv: f32, al: f32, voice_off: bool) {
        let m = self.mode;
        let s = t - self.mode_since;
        for i in 0..N {
            let p = &mut self.p[i];
            let (tgt, k, jit, alpha, col): ((f32, f32), f32, f32, f32, Rgb) = match m {
                Mode::Idle => {
                    let a = p.ph + t * 0.12;
                    let rr = 0.5 + 0.18 * (p.ph * 3.0 + t * 0.4).sin();
                    ((a.cos() * rr, a.sin() * rr * 0.9), 1.5, 0.06, 0.7, NEUTRAL)
                }
                Mode::Connecting => {
                    let on = s > i as f32 * 0.035;
                    let a = p.ph + t * 0.12;
                    let tgt = if on { (a.cos() * 0.55, a.sin() * 0.5) } else { (p.ph.cos() * 1.4, p.ph.sin() * 1.4) };
                    (tgt, 3.0, 0.04, if on { 0.8 } else { 0.0 }, NEUTRAL)
                }
                Mode::Listening => (cluster(p, t, lv), 9.0, 0.02 + lv * 0.12, 0.95, YOU),
                Mode::Thinking => {
                    let a = i as f32 / N as f32 * std::f32::consts::TAU + t * 2.2;
                    let rr = 0.36 + 0.04 * (t * 3.0 + i as f32).sin();
                    ((a.cos() * rr, a.sin() * rr), 10.0, 0.005, 0.85, NEUTRAL)
                }
                Mode::Speaking => (line(i, t, al, N), 14.0, 0.005, if voice_off { 0.4 } else { 0.95 }, AGENT),
                Mode::Both => {
                    if i % 2 == 1 {
                        (cluster(p, t, lv), 10.0, 0.03, 0.85, YOU)
                    } else {
                        (line(i / 2, t, al, N / 2), 10.0, 0.03, 0.85, AGENT)
                    }
                }
                Mode::Muted => ((-0.7 + 1.4 * i as f32 / (N - 1) as f32, 0.62 + 0.06 * p.r), 3.0, 0.003, 0.3, NEUTRAL),
                Mode::Stopped => ((-0.7 + 1.4 * i as f32 / (N - 1) as f32, 0.7), 2.0, 0.0, 0.08, NEUTRAL),
            };
            let kk = (dt * k).min(1.0);
            p.x = lerp(p.x, tgt.0, kk) + (t * 3.1 + p.ph * 7.0).sin() * jit * dt * 30.0;
            p.y = lerp(p.y, tgt.1, kk) + (t * 2.7 + p.ph * 5.0).cos() * jit * dt * 30.0;
            p.a = lerp(p.a, alpha, (dt * 4.0).min(1.0));
            p.col = mix(p.col, col, (dt * 5.0).min(1.0));
            p.flare = (p.flare - dt * 1.3).max(0.0);
        }
    }

    /// Each firefly's color with its flare applied.
    pub fn color(p: &Firefly) -> Rgb {
        if p.flare > 0.0 { mix(p.col, if p.err { ERR } else { WHITE }, p.flare) } else { p.col }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_swarm_settles_and_a_flare_wakes_it() {
        let mut s = Swarm::new();
        s.set_mode(Mode::Stopped, 0.0);
        let mut t = 0.0;
        while t < 10.0 {
            s.step(1.0 / 30.0, t, 0.0, 0.0, false);
            t += 1.0 / 30.0;
        }
        assert!(!s.busy(), "settled after ten seconds stopped");
        s.flare(true);
        assert!(s.busy());
        assert!(s.p.iter().any(|p| p.err && p.flare > 0.0));
    }

    #[test]
    fn listening_turns_blue_and_speaking_amber() {
        let mut s = Swarm::new();
        for (mode, want) in [(Mode::Listening, YOU), (Mode::Speaking, AGENT)] {
            s.set_mode(mode, 0.0);
            for i in 0..90 {
                s.step(1.0 / 30.0, i as f32 / 30.0, 0.5, 0.5, false);
            }
            let c = s.p[0].col;
            assert!((c[0] - want[0]).abs() < 2.0 && (c[2] - want[2]).abs() < 2.0, "{mode:?}: {c:?}");
        }
    }
}
