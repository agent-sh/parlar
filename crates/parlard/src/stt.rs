//! Final transcript: a NeMo FastConformer TDT model (Phonon-2, or Parakeet TDT 0.6B v3 that it
//! derives from) on ONNX Runtime, in the onnx-asr layout: a mel preprocessor, an encoder and a
//! joint decoder, decoded greedily the way onnx-asr does it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::value::Tensor;

/// Tokens a TDT decoder may emit on one frame before it must move on.
const MAX_TOKENS_PER_STEP: usize = 10;

pub struct Tdt {
    pre: Session,
    enc: Session,
    dec: Session,
    vocab: Vec<String>,
    blank: usize,
    /// (layers, hidden) of the prediction network's two LSTM states.
    state: (usize, usize),
}

fn session(path: &Path, threads: usize) -> Result<Session> {
    Session::builder()?
        .with_intra_threads(threads)?
        .commit_from_file(path)
        .with_context(|| format!("load {}", path.display()))
}

/// First file that exists among the names, so a model directory may ship int8 or fp32.
fn pick(dir: &Path, names: &[&str]) -> Result<std::path::PathBuf> {
    names
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("none of {names:?} in {}", dir.display()))
}

impl Tdt {
    pub fn load(dir: &Path, ort_lib: &Path, threads: usize) -> Result<Tdt> {
        crate::turn::init_ort(ort_lib)?;
        // a model may ship its own preprocessor (Phonon-2 does); onnx-asr's nemo128 otherwise
        let pre = session(&pick(dir, &["preprocessor-model.onnx", "nemo128.onnx"])?, 1)?;
        let enc_name = std::env::var("PARLAR_ENCODER").ok().filter(|n| !n.is_empty());
        let enc_names: Vec<&str> = match enc_name.as_deref() {
            Some(n) => vec![n],
            None => vec!["encoder-model.int8.onnx", "encoder-model.exact4x2.onnx", "encoder-model.onnx"],
        };
        let enc = session(&pick(dir, &enc_names)?, threads)?;
        let dec = session(&pick(dir, &["decoder_joint-model.int8.onnx", "decoder_joint-model.onnx"])?, 1)?;
        let text = std::fs::read_to_string(dir.join("vocab.txt")).context("read vocab.txt")?;
        let mut vocab = Vec::new();
        for line in text.lines() {
            let Some((tok, id)) = line.rsplit_once(' ') else { continue };
            let id: usize = id.parse().context("vocab id")?;
            if vocab.len() <= id {
                vocab.resize(id + 1, String::new());
            }
            vocab[id] = tok.replace('\u{2581}', " ");
        }
        let blank = vocab.iter().position(|t| t == "<blk>").context("no <blk> in vocab")?;
        let state = state_shape(&dec)?;
        Ok(Tdt { pre, enc, dec, vocab, blank, state })
    }

    /// Transcribe 16 kHz mono audio.
    pub fn transcribe(&mut self, pcm: &[f32]) -> Result<String> {
        if pcm.len() < 1600 {
            return Ok(String::new());
        }
        let wav = Tensor::from_array(([1usize, pcm.len()], pcm.to_vec()))?;
        let lens = Tensor::from_array(([1usize], vec![pcm.len() as i64]))?;
        let (fshape, feats, flen) = {
            let out = self.pre.run(ort::inputs!["waveforms" => wav, "waveforms_lens" => lens])?;
            let (fshape, feats) = out["features"].try_extract_tensor::<f32>()?;
            let (_, flen) = out["features_lens"].try_extract_tensor::<i64>()?;
            (fshape.iter().map(|&d| d as usize).collect::<Vec<_>>(), feats.to_vec(), flen[0])
        };
        let feats = Tensor::from_array((fshape, feats))?;
        let flen = Tensor::from_array(([1usize], vec![flen]))?;
        let (dim, frames, len, enc) = {
            let out = self.enc.run(ort::inputs!["audio_signal" => feats, "length" => flen])?;
            let (eshape, enc) = out["outputs"].try_extract_tensor::<f32>()?;
            let (_, elen) = out["encoded_lengths"].try_extract_tensor::<i64>()?;
            // [1, dim, frames]
            let (dim, frames) = (eshape[1] as usize, eshape[2] as usize);
            (dim, frames, (elen[0] as usize).min(frames), enc.to_vec())
        };

        let (layers, hidden) = self.state;
        let mut state1 = vec![0f32; layers * hidden];
        let mut state2 = vec![0f32; layers * hidden];
        let mut tokens: Vec<usize> = Vec::new();
        let vocab_size = self.vocab.len();
        let (mut t, mut emitted) = (0usize, 0usize);
        let mut frame = vec![0f32; dim];
        while t < len {
            for (d, f) in frame.iter_mut().enumerate() {
                *f = enc[d * frames + t];
            }
            let prev = *tokens.last().unwrap_or(&self.blank) as i32;
            let res = self.dec.run(ort::inputs![
                "encoder_outputs" => Tensor::from_array(([1usize, dim, 1], frame.clone()))?,
                "targets" => Tensor::from_array(([1usize, 1], vec![prev]))?,
                "target_length" => Tensor::from_array(([1usize], vec![1i32]))?,
                "input_states_1" => Tensor::from_array(([layers, 1, hidden], state1.clone()))?,
                "input_states_2" => Tensor::from_array(([layers, 1, hidden], state2.clone()))?,
            ])?;
            let (_, logits) = res["outputs"].try_extract_tensor::<f32>()?;
            if logits.len() <= vocab_size {
                bail!("joint output has {} values for a vocab of {vocab_size}", logits.len());
            }
            let token = argmax(&logits[..vocab_size]);
            let step = argmax(&logits[vocab_size..]);
            if token != self.blank {
                let (_, s1) = res["output_states_1"].try_extract_tensor::<f32>()?;
                let (_, s2) = res["output_states_2"].try_extract_tensor::<f32>()?;
                state1.copy_from_slice(&s1[..layers * hidden]);
                state2.copy_from_slice(&s2[..layers * hidden]);
                tokens.push(token);
                emitted += 1;
            }
            if step > 0 {
                t += step;
                emitted = 0;
            } else if token == self.blank || emitted == MAX_TOKENS_PER_STEP {
                t += 1;
                emitted = 0;
            }
        }
        Ok(self.detokenize(&tokens))
    }

    fn detokenize(&self, tokens: &[usize]) -> String {
        // control pieces (<unk>, <pad>, <|...|>) are not words
        let joined: String = tokens
            .iter()
            .map(|&i| self.vocab[i].as_str())
            .filter(|t| !(t.starts_with('<') && t.ends_with('>')))
            .collect();
        joined.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

/// The prediction network state is [layers, batch, hidden]; batch is dynamic.
fn state_shape(dec: &Session) -> Result<(usize, usize)> {
    let input = dec
        .inputs
        .iter()
        .find(|i| i.name == "input_states_1")
        .context("decoder has no input_states_1")?;
    let ort::value::ValueType::Tensor { shape, .. } = &input.input_type else {
        bail!("input_states_1 is not a tensor");
    };
    if shape.len() != 3 || shape[0] <= 0 || shape[2] <= 0 {
        bail!("unexpected state shape {shape:?}");
    }
    Ok((shape[0] as usize, shape[2] as usize))
}
