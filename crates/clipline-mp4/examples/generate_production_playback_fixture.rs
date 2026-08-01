use std::path::PathBuf;

#[path = "../tests/support/production_fixture.rs"]
mod production_fixture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = production_fixture::generate()?;
    let destination = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    std::fs::write(&destination, &output)?;
    println!("wrote {} bytes to {}", output.len(), destination.display());
    Ok(())
}
