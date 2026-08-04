//! Bounded, frontend-neutral game-icon identities and PNG decoding.

use std::error::Error;
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use clipline_settings::ProbeSessionOwner;
use serde::{Deserialize, Serialize};

use crate::identity::GameItemIdentity;

pub const MAX_GAME_ICON_ENCODED_PNG_BYTES: usize = 256 * 1024;
pub const MAX_GAME_ICON_BASE64_BYTES: usize = MAX_GAME_ICON_ENCODED_PNG_BYTES.div_ceil(3) * 4;
pub const MAX_GAME_ICON_DATA_URL_BYTES: usize =
    PNG_DATA_URL_PREFIX.len() + MAX_GAME_ICON_BASE64_BYTES;
pub const MAX_GAME_ICON_ASSET_PATH_BYTES: usize = 4 * 1024;
pub const MAX_SOURCE_ICON_DIMENSION: u32 = 1024;
pub const MAX_SOURCE_ICON_PIXELS: u64 = 1_048_576;
pub const MAX_GAME_ICON_DIMENSION: u32 = 256;
pub const MAX_GAME_ICON_RGBA_BYTES: usize = 256 * 1024;

const PNG_DATA_URL_PREFIX: &str = "data:image/png;base64,";
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const PNG_IHDR_LENGTH: u32 = 13;
const PNG_HEADER_PREFLIGHT_BYTES: usize = 24;
const MAX_SOURCE_RGBA_BYTES: usize =
    MAX_SOURCE_ICON_DIMENSION as usize * MAX_SOURCE_ICON_DIMENSION as usize * 4;
const MAX_BASE64_DECODE_BYTES: usize = MAX_GAME_ICON_ENCODED_PNG_BYTES + 2;
const PNG_DECODER_ALLOCATION_BYTES: usize = MAX_SOURCE_RGBA_BYTES + (256 * 1024);

const _: () = {
    assert!(MAX_SOURCE_ICON_PIXELS == 1_048_576);
    assert!(MAX_GAME_ICON_RGBA_BYTES == 256 * 256 * 4);
    assert!(MAX_GAME_ICON_DATA_URL_BYTES == 349_550);
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameIconError {
    CandidateOwnerMismatch,
    UnsupportedPngDataUrl,
    EncodedSourceTooLarge,
    AssetPathTooLarge,
    InvalidAssetPath,
    MissingSource,
    AssetReadDeferred,
    InvalidBase64,
    EncodedPngTooLarge,
    InvalidPngHeader,
    InvalidSourceDimensions,
    SourceDimensionsTooLarge,
    PngDecodeFailed,
    PngOutputMismatch,
    RgbaOutputTooLarge,
    AllocationFailed(&'static str),
}

impl fmt::Display for GameIconError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateOwnerMismatch => {
                formatter.write_str("game icon candidate belongs to another Settings owner")
            }
            Self::UnsupportedPngDataUrl => {
                formatter.write_str("game icon must use a PNG base64 data URL")
            }
            Self::EncodedSourceTooLarge => formatter.write_str("game icon data URL is too large"),
            Self::AssetPathTooLarge => formatter.write_str("game icon asset path is too large"),
            Self::InvalidAssetPath => formatter.write_str("game icon asset path is invalid"),
            Self::MissingSource => formatter.write_str("game icon source is missing"),
            Self::AssetReadDeferred => {
                formatter.write_str("game icon asset reading belongs to a platform adapter")
            }
            Self::InvalidBase64 => formatter.write_str("game icon base64 is invalid"),
            Self::EncodedPngTooLarge => formatter.write_str("encoded game icon PNG is too large"),
            Self::InvalidPngHeader => formatter.write_str("game icon PNG header is invalid"),
            Self::InvalidSourceDimensions => {
                formatter.write_str("game icon PNG dimensions are invalid")
            }
            Self::SourceDimensionsTooLarge => {
                formatter.write_str("game icon PNG dimensions exceed the decode bound")
            }
            Self::PngDecodeFailed => formatter.write_str("game icon PNG decode failed"),
            Self::PngOutputMismatch => {
                formatter.write_str("game icon PNG output does not match its preflight header")
            }
            Self::RgbaOutputTooLarge => formatter.write_str("decoded game icon is too large"),
            Self::AllocationFailed(context) => {
                write!(formatter, "allocate bounded game icon {context}")
            }
        }
    }
}

impl Error for GameIconError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct GameIconId {
    owner: ProbeSessionOwner,
    item: GameItemIdentity,
}

impl GameIconId {
    pub fn new(owner: ProbeSessionOwner, item: GameItemIdentity) -> Result<Self, GameIconError> {
        if let GameItemIdentity::Candidate(candidate) = &item {
            if candidate.token().owner != owner {
                return Err(GameIconError::CandidateOwnerMismatch);
            }
        }
        Ok(Self { owner, item })
    }

    pub const fn owner(&self) -> ProbeSessionOwner {
        self.owner
    }

    pub const fn item(&self) -> &GameItemIdentity {
        &self.item
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameIconLoadState {
    Missing,
    Loading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameIconSource(GameIconSourceKind);

#[derive(Debug, Clone, PartialEq, Eq)]
enum GameIconSourceKind {
    PngDataUrl(Arc<str>),
    FirstPartyAssetPath(Arc<str>),
    Missing,
}

impl GameIconSource {
    pub fn png_data_url(value: String) -> Result<Self, GameIconError> {
        let Some(payload) = value.strip_prefix(PNG_DATA_URL_PREFIX) else {
            return Err(GameIconError::UnsupportedPngDataUrl);
        };
        if value.len() > MAX_GAME_ICON_DATA_URL_BYTES || payload.len() > MAX_GAME_ICON_BASE64_BYTES
        {
            return Err(GameIconError::EncodedSourceTooLarge);
        }
        Ok(Self(GameIconSourceKind::PngDataUrl(value.into())))
    }

    pub fn first_party_asset_path(value: String) -> Result<Self, GameIconError> {
        if value.len() > MAX_GAME_ICON_ASSET_PATH_BYTES {
            return Err(GameIconError::AssetPathTooLarge);
        }
        let value = value.trim();
        if !matches!(
            value,
            "assets/games/league-of-legends.png" | "assets/games/osu.png"
        ) {
            return Err(GameIconError::InvalidAssetPath);
        }
        Ok(Self(GameIconSourceKind::FirstPartyAssetPath(value.into())))
    }

    pub const fn missing() -> Self {
        Self(GameIconSourceKind::Missing)
    }

    pub fn as_png_data_url(&self) -> Option<&str> {
        match &self.0 {
            GameIconSourceKind::PngDataUrl(value) => Some(value),
            GameIconSourceKind::FirstPartyAssetPath(_) | GameIconSourceKind::Missing => None,
        }
    }

    pub fn as_first_party_asset_path(&self) -> Option<&str> {
        match &self.0 {
            GameIconSourceKind::FirstPartyAssetPath(value) => Some(value),
            GameIconSourceKind::PngDataUrl(_) | GameIconSourceKind::Missing => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodedGameIcon {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl DecodedGameIcon {
    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

pub fn decode_game_icon_source(source: &GameIconSource) -> Result<DecodedGameIcon, GameIconError> {
    decode_game_icon_source_with(source, &mut SystemIconAllocator)
}

trait IconAllocator {
    fn empty_with_capacity(
        &mut self,
        capacity: usize,
        context: &'static str,
    ) -> Result<Vec<u8>, GameIconError>;

    fn zeroed(&mut self, length: usize, context: &'static str) -> Result<Vec<u8>, GameIconError>;
}

struct SystemIconAllocator;

impl IconAllocator for SystemIconAllocator {
    fn empty_with_capacity(
        &mut self,
        capacity: usize,
        context: &'static str,
    ) -> Result<Vec<u8>, GameIconError> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(|_| GameIconError::AllocationFailed(context))?;
        Ok(buffer)
    }

    fn zeroed(&mut self, length: usize, context: &'static str) -> Result<Vec<u8>, GameIconError> {
        let mut buffer = self.empty_with_capacity(length, context)?;
        buffer.resize(length, 0);
        Ok(buffer)
    }
}

fn decode_game_icon_source_with(
    source: &GameIconSource,
    allocator: &mut impl IconAllocator,
) -> Result<DecodedGameIcon, GameIconError> {
    let GameIconSourceKind::PngDataUrl(data_url) = &source.0 else {
        return Err(match &source.0 {
            GameIconSourceKind::FirstPartyAssetPath(_) => GameIconError::AssetReadDeferred,
            GameIconSourceKind::Missing => GameIconError::MissingSource,
            GameIconSourceKind::PngDataUrl(_) => unreachable!(),
        });
    };
    let payload = data_url
        .strip_prefix(PNG_DATA_URL_PREFIX)
        .ok_or(GameIconError::UnsupportedPngDataUrl)?;
    if payload.len() > MAX_GAME_ICON_BASE64_BYTES {
        return Err(GameIconError::EncodedSourceTooLarge);
    }

    let mut encoded_png = allocator.empty_with_capacity(
        MAX_BASE64_DECODE_BYTES.min(payload.len().div_ceil(4).saturating_mul(3)),
        "encoded PNG",
    )?;
    STANDARD
        .decode_vec(payload, &mut encoded_png)
        .map_err(|_| GameIconError::InvalidBase64)?;
    if encoded_png.len() > MAX_GAME_ICON_ENCODED_PNG_BYTES {
        return Err(GameIconError::EncodedPngTooLarge);
    }

    let (source_width, source_height) = inspect_png_header(&encoded_png)?;
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(encoded_png.as_slice()),
        png::Limits {
            bytes: PNG_DECODER_ALLOCATION_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|_| GameIconError::PngDecodeFailed)?;
    let decoded_length = reader.output_buffer_size();
    if decoded_length > MAX_SOURCE_RGBA_BYTES {
        return Err(GameIconError::SourceDimensionsTooLarge);
    }
    let mut decoded = allocator.zeroed(decoded_length, "decoder output")?;
    let output = reader
        .next_frame(&mut decoded)
        .map_err(|_| GameIconError::PngDecodeFailed)?;
    if output.width != source_width
        || output.height != source_height
        || output.buffer_size() > decoded.len()
    {
        return Err(GameIconError::PngOutputMismatch);
    }
    decoded.truncate(output.buffer_size());

    let (width, height) = resized_dimensions(source_width, source_height)?;
    let rgba_length = rgba_length(width, height)?;
    let mut rgba = allocator.zeroed(rgba_length, "RGBA output")?;
    copy_resized_rgba(
        &decoded,
        output.color_type,
        source_width,
        source_height,
        width,
        height,
        &mut rgba,
    )?;
    Ok(DecodedGameIcon {
        width,
        height,
        rgba,
    })
}

fn inspect_png_header(encoded_png: &[u8]) -> Result<(u32, u32), GameIconError> {
    if encoded_png.len() < PNG_HEADER_PREFLIGHT_BYTES
        || encoded_png.get(..8) != Some(PNG_SIGNATURE.as_slice())
        || encoded_png.get(12..16) != Some(b"IHDR".as_slice())
    {
        return Err(GameIconError::InvalidPngHeader);
    }
    let chunk_length = u32::from_be_bytes(
        encoded_png[8..12]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    if chunk_length != PNG_IHDR_LENGTH {
        return Err(GameIconError::InvalidPngHeader);
    }
    let width = u32::from_be_bytes(
        encoded_png[16..20]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    let height = u32::from_be_bytes(
        encoded_png[20..24]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    if width == 0 || height == 0 {
        return Err(GameIconError::InvalidSourceDimensions);
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(GameIconError::SourceDimensionsTooLarge)?;
    if width > MAX_SOURCE_ICON_DIMENSION
        || height > MAX_SOURCE_ICON_DIMENSION
        || pixels > MAX_SOURCE_ICON_PIXELS
    {
        return Err(GameIconError::SourceDimensionsTooLarge);
    }
    Ok((width, height))
}

fn resized_dimensions(source_width: u32, source_height: u32) -> Result<(u32, u32), GameIconError> {
    if source_width <= MAX_GAME_ICON_DIMENSION && source_height <= MAX_GAME_ICON_DIMENSION {
        return Ok((source_width, source_height));
    }
    let (width, height) = if source_width >= source_height {
        let height = (u64::from(source_height) * u64::from(MAX_GAME_ICON_DIMENSION)
            / u64::from(source_width))
        .max(1);
        (
            MAX_GAME_ICON_DIMENSION,
            u32::try_from(height).map_err(|_| GameIconError::RgbaOutputTooLarge)?,
        )
    } else {
        let width = (u64::from(source_width) * u64::from(MAX_GAME_ICON_DIMENSION)
            / u64::from(source_height))
        .max(1);
        (
            u32::try_from(width).map_err(|_| GameIconError::RgbaOutputTooLarge)?,
            MAX_GAME_ICON_DIMENSION,
        )
    };
    Ok((width, height))
}

fn rgba_length(width: u32, height: u32) -> Result<usize, GameIconError> {
    let length = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(GameIconError::RgbaOutputTooLarge)?;
    if length > MAX_GAME_ICON_RGBA_BYTES {
        return Err(GameIconError::RgbaOutputTooLarge);
    }
    Ok(length)
}

#[allow(clippy::too_many_arguments)]
fn copy_resized_rgba(
    decoded: &[u8],
    color_type: png::ColorType,
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    target: &mut [u8],
) -> Result<(), GameIconError> {
    let channels = match color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(GameIconError::PngOutputMismatch),
    };
    let expected_source =
        usize::try_from(u64::from(source_width) * u64::from(source_height) * channels as u64)
            .map_err(|_| GameIconError::PngOutputMismatch)?;
    if decoded.len() != expected_source || target.len() != rgba_length(target_width, target_height)?
    {
        return Err(GameIconError::PngOutputMismatch);
    }

    for target_y in 0..target_height {
        let source_y = u64::from(target_y) * u64::from(source_height) / u64::from(target_height);
        for target_x in 0..target_width {
            let source_x = u64::from(target_x) * u64::from(source_width) / u64::from(target_width);
            let source_pixel = usize::try_from(source_y * u64::from(source_width) + source_x)
                .map_err(|_| GameIconError::PngOutputMismatch)?;
            let source_offset = source_pixel
                .checked_mul(channels)
                .ok_or(GameIconError::PngOutputMismatch)?;
            let target_pixel = usize::try_from(
                u64::from(target_y) * u64::from(target_width) + u64::from(target_x),
            )
            .map_err(|_| GameIconError::PngOutputMismatch)?;
            let target_offset = target_pixel
                .checked_mul(4)
                .ok_or(GameIconError::PngOutputMismatch)?;
            let (red, green, blue, alpha) = match color_type {
                png::ColorType::Grayscale => {
                    let gray = decoded[source_offset];
                    (gray, gray, gray, 255)
                }
                png::ColorType::Rgb => (
                    decoded[source_offset],
                    decoded[source_offset + 1],
                    decoded[source_offset + 2],
                    255,
                ),
                png::ColorType::GrayscaleAlpha => {
                    let gray = decoded[source_offset];
                    (gray, gray, gray, decoded[source_offset + 1])
                }
                png::ColorType::Rgba => (
                    decoded[source_offset],
                    decoded[source_offset + 1],
                    decoded[source_offset + 2],
                    decoded[source_offset + 3],
                ),
                png::ColorType::Indexed => return Err(GameIconError::PngOutputMismatch),
            };
            target[target_offset..target_offset + 4].copy_from_slice(&[red, green, blue, alpha]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingAllocator;

    impl IconAllocator for FailingAllocator {
        fn empty_with_capacity(
            &mut self,
            _capacity: usize,
            context: &'static str,
        ) -> Result<Vec<u8>, GameIconError> {
            Err(GameIconError::AllocationFailed(context))
        }

        fn zeroed(
            &mut self,
            _length: usize,
            context: &'static str,
        ) -> Result<Vec<u8>, GameIconError> {
            Err(GameIconError::AllocationFailed(context))
        }
    }

    #[test]
    fn allocation_failure_is_reported_without_partial_output() {
        let source = GameIconSource::png_data_url(
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADElEQVR4nGNg+M8AAAICAQB7CYxOAAAAAElFTkSuQmCC".into(),
        )
        .unwrap();
        assert_eq!(
            decode_game_icon_source_with(&source, &mut FailingAllocator).unwrap_err(),
            GameIconError::AllocationFailed("encoded PNG")
        );
    }
}
