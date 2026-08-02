use std::path::PathBuf;

#[path = "../tests/support/production_fixture.rs"]
mod production_fixture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let (output, destination) = if first.is_none() {
        (
            production_fixture::generate()?,
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4"),
        )
    } else if first.as_deref() == Some(std::ffi::OsStr::new("--source")) {
        let source = arguments
            .next()
            .map(PathBuf::from)
            .ok_or("--source requires a path")?;
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--output")) {
            return Err("expected --output after the source path".into());
        }
        let destination = arguments
            .next()
            .map(PathBuf::from)
            .ok_or("--output requires a path")?;
        if arguments.next().is_some() {
            return Err("unexpected trailing arguments".into());
        }
        let source = std::fs::read(source)?;
        (
            production_fixture::generate_from_source(&source)?,
            destination,
        )
    } else {
        return Err(
            "usage: generate_production_playback_fixture [--source <source.mp4> --output <target.mp4>]"
                .into(),
        );
    };
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&destination, &output)?;
    println!("wrote {} bytes to {}", output.len(), destination.display());
    Ok(())
}
