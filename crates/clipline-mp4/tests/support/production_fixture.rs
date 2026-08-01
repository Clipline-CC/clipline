use std::error::Error;

use clipline_mp4::remux_with_selected_audio_tracks;
use clipline_mp4::walker::{children, find, walk};

pub const SOURCE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/playback/h264-two-opus-markers-5s.mp4"
));

pub fn generate() -> Result<Vec<u8>, Box<dyn Error>> {
    let top = walk(SOURCE);
    let moov = find(&top, b"moov").ok_or("source oracle has no moov")?;
    let tracks: Vec<_> = children(SOURCE, moov)
        .into_iter()
        .filter(|box_info| &box_info.fourcc == b"trak")
        .collect();
    if tracks.len() != 3 {
        return Err(format!("expected three source tracks, found {}", tracks.len()).into());
    }

    let mut edit_type_offsets = Vec::with_capacity(tracks.len());
    for track in &tracks {
        let edits: Vec<_> = children(SOURCE, track)
            .into_iter()
            .filter(|box_info| &box_info.fourcc == b"edts")
            .collect();
        if edits.len() != 1 {
            return Err(format!(
                "expected one edit-list box per source track, found {}",
                edits.len()
            )
            .into());
        }
        let type_offset = usize::try_from(edits[0].offset)?
            .checked_add(4)
            .ok_or("edts type offset overflow")?;
        edit_type_offsets.push(type_offset);
    }

    let mut source_without_edits = SOURCE.to_vec();
    for offset in edit_type_offsets {
        source_without_edits[offset..offset + 4].copy_from_slice(b"free");
    }

    // FFmpeg's Opus edit begins at the codec pre-skip, within its first packet.
    // Clipline records complete packets and carries pre-skip in dOps, so disabling
    // only the three parsed foreign edit-list boxes makes this a faithful input
    // to the production writer without scanning or mutating media payload bytes.
    Ok(remux_with_selected_audio_tracks(
        &source_without_edits,
        &[0, 1],
    )?)
}
