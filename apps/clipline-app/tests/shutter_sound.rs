use std::io::Cursor;

use rodio::{Decoder, OutputStream, Sink};

#[test]
fn embedded_shutter_sound_decodes_and_plays() {
    let bytes = include_bytes!("../../../shutter.ogg");
    let source = Decoder::new(Cursor::new(bytes)).expect("decode shutter sound");
    let (_stream, handle) =
        OutputStream::try_default().expect("open default audio output");
    let sink = Sink::try_new(&handle).expect("open audio sink");
    sink.append(source);
    sink.sleep_until_end();
}
