# parlar-moonshine

Bindings to [libmoonshine](https://github.com/moonshine-ai/moonshine) for Kokoro text to speech,
sentence splitting and the voice download manifest. The library is loaded at run time:

```rust
parlar_moonshine::open(std::path::Path::new("/path/to/libmoonshine.so"))?;
let tts = parlar_moonshine::Tts::load(models_dir, "en_us", "kokoro_af_heart", &[])?;
let (pcm, rate) = tts.synthesize("Hello.")?;
```

Used by [parlar](https://github.com/agent-sh/parlar), whose `parlard fetch` downloads the pinned
libmoonshine release. MIT or Apache-2.0.
