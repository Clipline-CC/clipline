//! Standalone experiment; never selected by Clipline's production recorder.
#[cfg(windows)]
#[path = "windows/print_window_record.rs"]
mod platform;

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    platform::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("print_window_record is Windows-only");
}

// Neutral protocol validation is exercised on both CI platforms.
#[cfg(any(windows, test))]
mod protocol {
    use std::io::Read;
    pub fn print_layout(
        flags: u32,
        window: (u32, u32),
        client: (u32, u32),
        offset: (u32, u32),
    ) -> Result<(u32, u32, u32, u32), &'static str> {
        frame_bytes(window.0, window.1)?;
        frame_bytes(client.0, client.1)?;
        if flags > 3 {
            return Err("PrintWindow flags must be 0..3");
        }
        if flags & 1 != 0 {
            return Ok((client.0, client.1, 0, 0));
        }
        if offset
            .0
            .checked_add(client.0)
            .is_none_or(|right| right > window.0)
            || offset
                .1
                .checked_add(client.1)
                .is_none_or(|bottom| bottom > window.1)
        {
            return Err("client crop outside window");
        }
        Ok((window.0, window.1, offset.0, offset.1))
    }
    pub const MAGIC: u32 = 0x31575043; // CPW1
    pub struct Packet {
        pub width: u32,
        pub height: u32,
        pub begin: i64,
        pub end: i64,
        pub pixels: Vec<u8>,
    }
    pub fn read_packet(
        reader: &mut impl Read,
        sequence: u64,
        previous: i64,
    ) -> std::io::Result<Packet> {
        let invalid = |s| std::io::Error::new(std::io::ErrorKind::InvalidData, s);
        let mut header = [0u8; 40];
        reader.read_exact(&mut header)?;
        let u32_at = |i| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
        let u64_at = |i| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
        if u32_at(0) != MAGIC || u64_at(16) != sequence {
            return Err(invalid("bad frame magic or sequence"));
        }
        let (width, height) = (u32_at(4), u32_at(8));
        let length = frame_bytes(width, height).map_err(invalid)?;
        if length != u32_at(12) as usize {
            return Err(invalid("frame payload length mismatch"));
        }
        let (begin, end) = (u64_at(24) as i64, u64_at(32) as i64);
        validate_times(begin, end, previous).map_err(invalid)?;
        let mut pixels = vec![0; length];
        reader.read_exact(&mut pixels)?;
        Ok(Packet {
            width,
            height,
            begin,
            end,
            pixels,
        })
    }
    pub fn frame_bytes(width: u32, height: u32) -> Result<usize, &'static str> {
        let pixels = u64::from(width) * u64::from(height);
        if width == 0 || height == 0 || pixels > 16_777_216 {
            return Err("invalid or oversized frame");
        }
        Ok(pixels as usize * 4)
    }

    pub fn validate_times(begin: i64, end: i64, previous: i64) -> Result<(), &'static str> {
        if begin <= previous || end < begin || end - begin > 20_000_000 {
            return Err("invalid, nonmonotonic or stalled capture timestamps");
        }
        Ok(())
    }

    pub fn mock_counter(bgra: &[u8], width: u32, height: u32) -> Result<u16, &'static str> {
        if bgra.len() != frame_bytes(width, height)? || width < 360 || height < 120 {
            return Err("mock frame dimensions invalid");
        }
        let pixel = |x: usize, y: usize| {
            let offset = (y * width as usize + x) * 4;
            &bgra[offset..offset + 3]
        };
        if pixel(40, 40) != [0, 0, 255] || pixel(120, 40) != [255, 0, 0] {
            return Err("mock red/blue anchors missing");
        }
        let mut counter = 0;
        for bit in 0..16 {
            match pixel(19 + 22 * bit, 110) {
                [255, 255, 255] => counter |= 1 << bit,
                [255, 0, 0] => (),
                _ => return Err("invalid mock counter pixel"),
            }
        }
        Ok(counter)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn client_only_has_no_nonclient_crop() {
            for flags in [1, 3] {
                assert_eq!(
                    print_layout(flags, (816, 489), (800, 450), (8, 31)),
                    Ok((800, 450, 0, 0))
                );
            }
        }
        #[test]
        fn full_window_retains_client_crop() {
            for flags in [0, 2] {
                assert_eq!(
                    print_layout(flags, (816, 489), (800, 450), (8, 31)),
                    Ok((816, 489, 8, 31))
                );
            }
            assert!(print_layout(4, (800, 450), (800, 450), (0, 0)).is_err());
            assert!(print_layout(2, (800, 450), (800, 450), (u32::MAX, 0)).is_err());
            assert!(print_layout(1, (800, 450), (0, 450), (0, 0)).is_err());
        }
        #[test]
        fn bounds_before_allocation() {
            assert_eq!(frame_bytes(1280, 720), Ok(3_686_400));
            assert!(frame_bytes(0, 720).is_err());
            assert!(frame_bytes(u32::MAX, u32::MAX).is_err());
            assert!(frame_bytes(8192, 8192).is_err());
        }
        #[test]
        fn reject_bad_or_stalled_timestamps() {
            assert!(validate_times(100, 200, 99).is_ok());
            assert!(validate_times(100, 200, 100).is_err());
            assert!(validate_times(200, 100, 99).is_err());
            assert!(validate_times(100, 20_000_101, 99).is_err());
        }
        #[test]
        fn white_frame_cannot_pass_as_mock_motion() {
            assert!(mock_counter(&vec![255; 640 * 360 * 4], 640, 360).is_err());
            assert!(mock_counter(&[0; 4], 640, 360).is_err());
        }
        #[test]
        fn valid_counter_and_wrap_values_decode() {
            for value in [0u16, 1, 0x1234, 65535] {
                let mut pixels = vec![0; 640 * 360 * 4];
                let mut set = |x: usize, y: usize, color: [u8; 3]| {
                    let offset = (y * 640 + x) * 4;
                    pixels[offset..offset + 3].copy_from_slice(&color);
                };
                set(40, 40, [0, 0, 255]);
                set(120, 40, [255, 0, 0]);
                for bit in 0..16 {
                    set(
                        19 + 22 * bit,
                        110,
                        if value & (1 << bit) != 0 {
                            [255; 3]
                        } else {
                            [255, 0, 0]
                        },
                    );
                }
                assert_eq!(mock_counter(&pixels, 640, 360), Ok(value));
            }
        }
        fn wire() -> Vec<u8> {
            let mut bytes = Vec::new();
            for v in [MAGIC, 1, 1, 4] {
                bytes.extend(v.to_le_bytes());
            }
            for v in [0u64, 100, 200] {
                bytes.extend(v.to_le_bytes());
            }
            bytes.extend([1, 2, 3, 4]);
            bytes
        }
        #[test]
        fn protocol_roundtrip_and_truncation() {
            let bytes = wire();
            let packet = read_packet(&mut bytes.as_slice(), 0, 0).unwrap();
            assert_eq!(
                (packet.width, packet.height, packet.begin, packet.end),
                (1, 1, 100, 200)
            );
            assert_eq!(packet.pixels, [1, 2, 3, 4]);
            for length in [0, 39, 40, 43] {
                assert!(read_packet(&mut &bytes[..length], 0, 0).is_err());
            }
        }
        #[test]
        fn reject_header_before_reading_payload() {
            for (offset, value) in [(0, 0u32), (4, u32::MAX), (8, 0), (12, 5)] {
                let mut bytes = wire();
                bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                let mut input = bytes.as_slice();
                assert!(read_packet(&mut input, 0, 0).is_err());
                assert_eq!(input.len(), 4);
            }
            assert!(read_packet(&mut wire().as_slice(), 1, 0).is_err());
            assert!(read_packet(&mut wire().as_slice(), 0, 100).is_err());
        }
    }
}
