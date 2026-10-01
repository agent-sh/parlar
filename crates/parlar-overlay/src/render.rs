//! The swarm as pixels: a square, premultiplied BGRA buffer with per-pixel alpha, the same on
//! every platform. Windows hands it to UpdateLayeredWindow; macOS wraps it in a CGImage.

use crate::swarm::{Mode, Swarm};

/// Draw `swarm` into a `w` by `w` BGRA buffer. `muted_bar` adds the red bar of a muted mic.
pub fn render(swarm: &Swarm, w: usize, muted_bar: bool) -> Vec<u8> {
    let mut px = vec![0u8; w * w * 4];
    let c = w as f32 / 2.0;
    let u = w as f32 * 0.42;
    let scale = w as f32 / 104.0;
    // a pocket of night behind the fireflies so they read on light windows too
    let depth = if swarm.mode() == Mode::Stopped { 0.18 } else { 0.42 };
    for yy in 0..w {
        for xx in 0..w {
            let d = (((xx as f32 - c).powi(2) + (yy as f32 - c).powi(2)).sqrt() / c).min(1.0);
            let a = if d < 0.7 { depth * (1.0 - 0.4 * d / 0.7) } else { depth * 0.6 * (1.0 - (d - 0.7) / 0.3) };
            let i = (yy * w + xx) * 4;
            px[i] = (0.09 * 255.0 * a) as u8;
            px[i + 1] = (0.06 * 255.0 * a) as u8;
            px[i + 2] = (0.05 * 255.0 * a) as u8;
            px[i + 3] = (255.0 * a) as u8;
        }
    }
    for p in &swarm.p {
        if p.a < 0.005 {
            continue;
        }
        let (fx, fy) = (c + p.x * u, c + p.y * u);
        let col = Swarm::color(p);
        let halo = (4.5 + p.flare * 12.0) * scale;
        let (x0, x1) = (((fx - halo).floor() as i32).max(0), ((fx + halo).ceil() as i32).min(w as i32 - 1));
        let (y0, y1) = (((fy - halo).floor() as i32).max(0), ((fy + halo).ceil() as i32).min(w as i32 - 1));
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                let r = ((xx as f32 - fx).powi(2) + (yy as f32 - fy).powi(2)).sqrt() / halo;
                if r >= 1.0 {
                    continue;
                }
                // the radial gradient: full at the center, 45% at a quarter, zero at the edge
                let g = if r < 0.25 { 1.0 - 0.55 * r / 0.25 } else { 0.45 * (1.0 - (r - 0.25) / 0.75) };
                let a = p.a * g;
                let i = ((yy as usize) * w + xx as usize) * 4;
                // additive, like the shell's ADD operator; alpha grows more slowly than the
                // color so the glow stays translucent, then each channel is held at the
                // alpha so the buffer stays premultiplied (Core Graphics requires it)
                px[i + 3] = (px[i + 3] as f32 + 255.0 * a * 0.6).min(255.0) as u8;
                let alpha = px[i + 3];
                for (k, ch) in [2usize, 1, 0].into_iter().enumerate() {
                    px[i + ch] = ((px[i + ch] as f32 + col[k] * a).min(255.0) as u8).min(alpha);
                }
            }
        }
    }
    if muted_bar {
        let y = (c + u * 0.86) as usize;
        for yy in y.saturating_sub(1)..=(y + 1).min(w - 1) {
            for xx in (c - u * 0.25) as usize..=(c + u * 0.25) as usize {
                let i = (yy * w + xx) * 4;
                px[i] = 59;
                px[i + 1] = 67;
                px[i + 2] = 191;
                px[i + 3] = 191;
            }
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendered_frame_is_premultiplied_and_transparent_at_the_corners() {
        let mut s = Swarm::new();
        s.set_mode(Mode::Listening, 0.0);
        for i in 0..60 {
            s.step(1.0 / 30.0, i as f32 / 30.0, 0.5, 0.0, false);
        }
        let w = 104;
        let px = render(&s, w, true);
        assert_eq!(px.len(), w * w * 4);
        // the corners lie outside the night circle and every firefly
        assert_eq!(&px[..4], &[0, 0, 0, 0]);
        // premultiplied: no channel exceeds its alpha
        for p in px.chunks(4) {
            assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "{p:?}");
        }
        // the fireflies lit something near the middle
        let mid = ((w / 2) * w + w / 2) * 4;
        assert!(px[mid + 3] > 0);
    }
}
