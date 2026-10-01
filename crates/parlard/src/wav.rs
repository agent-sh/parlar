//! 16-bit mono WAV writer, for debugging output.

use std::io::Write;
use std::path::Path;

pub fn write(path: &Path, pcm: &[f32], rate: u32) -> std::io::Result<()> {
    let data = (pcm.len() * 2) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 2).to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data.to_le_bytes())?;
    for s in pcm {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    f.flush()
}

/// Read a 16-bit PCM WAV (mono or the first channel).
pub fn read(path: &Path) -> std::io::Result<(Vec<f32>, u32)> {
    let b = std::fs::read(path)?;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a 16-bit PCM wav");
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad());
    }
    let (mut i, mut rate, mut ch) = (12, 0u32, 1u16);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let n = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + n).min(b.len())];
        if id == b"fmt " {
            ch = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            if u16::from_le_bytes(body[14..16].try_into().unwrap()) != 16 {
                return Err(bad());
            }
        } else if id == b"data" {
            let step = 2 * ch as usize;
            let pcm = body.chunks_exact(step).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0).collect();
            return Ok((pcm, rate));
        }
        i += 8 + n + (n & 1);
    }
    Err(bad())
}
