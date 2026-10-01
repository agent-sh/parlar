fn main() -> anyhow::Result<()> {
    println!("libmoonshine {}", parlar_moonshine::version());
    let arch = parlar_moonshine::ARCH_SMALL_STREAMING.to_string();
    println!("{}", parlar_moonshine::stt_manifest("en", &[("model_arch", &arch)])?);
    println!("{}", parlar_moonshine::tts_manifest("en", &[("voice", "kokoro_af_heart")])?);
    Ok(())
}
