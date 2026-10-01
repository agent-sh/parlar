fn main() -> anyhow::Result<()> {
    println!("libmoonshine {}", parley_moonshine::version());
    let arch = parley_moonshine::ARCH_SMALL_STREAMING.to_string();
    println!("{}", parley_moonshine::stt_manifest("en", &[("model_arch", &arch)])?);
    println!("{}", parley_moonshine::tts_manifest("en", &[("voice", "kokoro_af_heart")])?);
    Ok(())
}
