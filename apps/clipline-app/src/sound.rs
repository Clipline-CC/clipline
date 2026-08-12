use std::io::Cursor;
use std::thread;

use rodio::{Decoder, OutputStream, Sink};

const SOUND_EFFECT_OGG: &[u8] = include_bytes!("../../../soundeffect.ogg");
/// Deliberately shorter and quieter than the save sound: a bookmark is a light
/// acknowledgement mid-game, and the two must not be mistaken for each other.
/// rodio is built with the `vorbis` feature only, so assets stay Ogg Vorbis.
const BOOKMARK_OGG: &[u8] = include_bytes!("../../../bookmark.ogg");

pub fn play_replay_saved() {
    play_asset(
        "clipline-replay-sound",
        SOUND_EFFECT_OGG,
        "replay_save_sound_failed",
    );
}

pub fn play_bookmark_added() {
    play_asset(
        "clipline-bookmark-sound",
        BOOKMARK_OGG,
        "bookmark_sound_failed",
    );
}

fn play_asset(thread_name: &str, asset: &'static [u8], failure_event: &'static str) {
    if let Err(e) = thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = play_once(asset) {
                tracing::warn!(event = failure_event, error = %e);
            }
        })
    {
        tracing::warn!(event = "sound_thread_spawn_failed", error = %e);
    }
}

fn play_once(asset: &'static [u8]) -> Result<(), String> {
    let (_stream, handle) = OutputStream::try_default().map_err(|e| e.to_string())?;
    let sink = Sink::try_new(&handle).map_err(|e| e.to_string())?;
    let source = Decoder::new(Cursor::new(asset)).map_err(|e| e.to_string())?;
    sink.append(source);
    sink.sleep_until_end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_sound_effect_decodes() {
        let mut decoder = Decoder::new(Cursor::new(SOUND_EFFECT_OGG)).expect("decode replay sound");
        assert!(
            decoder.next().is_some(),
            "sound effect must contain samples"
        );
    }

    #[test]
    fn embedded_bookmark_sound_decodes() {
        let mut decoder = Decoder::new(Cursor::new(BOOKMARK_OGG)).expect("decode bookmark sound");
        assert!(
            decoder.next().is_some(),
            "bookmark sound must contain samples"
        );
    }

    #[test]
    fn bookmark_sound_is_distinct_from_the_save_sound() {
        assert_ne!(BOOKMARK_OGG, SOUND_EFFECT_OGG);
    }
}
