//! Port of packages/agent/src/harness/tools/image.ts (pi v0.84.3) — image
//! signature detection and base64 encoding.

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Upstream `detectSupportedImageMimeType`: sniff the supported image
/// container from magic bytes; `None` for unsupported/animated variants.
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with(buffer, &[0xFF, 0xD8, 0xFF]) {
        return if buffer.get(3) == Some(&0xF7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(buffer, 0, "GIF") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// Upstream `encodeBase64` (hand-rolled; standard alphabet with padding).
pub fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output
            .push(ALPHABET[(((first & 0x03) << 4) | (second.unwrap_or(0) >> 4)) as usize] as char);
        output.push(match second {
            None => '=',
            Some(second) => {
                ALPHABET[(((second & 0x0F) << 2) | (third.unwrap_or(0) >> 6)) as usize] as char
            }
        });
        output.push(match third {
            None => '=',
            Some(third) => ALPHABET[(third & 0x3F) as usize] as char,
        });
    }
    output
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length as usize + 4;
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(buffer, 2);
    let pixel_data_offset = read_u32_le(buffer, 10);
    let dib_header_size = read_u32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        if buffer.len() < 26 {
            return false;
        }
        (read_u16_le(buffer, 22), read_u16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_u16_le(buffer, 26), read_u16_le(buffer, 28))
    } else {
        return false;
    };
    color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(buffer.get(offset).copied().unwrap_or(0))
        + (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 8)
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    (u32::from(buffer.get(offset).copied().unwrap_or(0)) << 24)
        | (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 16)
        | (u32::from(buffer.get(offset + 2).copied().unwrap_or(0)) << 8)
        | u32::from(buffer.get(offset + 3).copied().unwrap_or(0))
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(buffer.get(offset).copied().unwrap_or(0))
        | (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 8)
        | (u32::from(buffer.get(offset + 2).copied().unwrap_or(0)) << 16)
        | (u32::from(buffer.get(offset + 3).copied().unwrap_or(0)) << 24)
}

fn starts_with(buffer: &[u8], prefix: &[u8]) -> bool {
    buffer.len() >= prefix.len() && buffer[..prefix.len()] == *prefix
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    buffer.len() >= offset + text.len() && &buffer[offset..offset + text.len()] == text.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_image_signatures() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(detect_supported_image_mime_type(&jpeg), Some("image/jpeg"));
        let progressive_jpeg = vec![0xFF, 0xD8, 0xFF, 0xF7];
        assert_eq!(detect_supported_image_mime_type(&progressive_jpeg), None);

        // 16-byte PNG header with IHDR (minimal valid signature).
        let png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R',
        ];
        assert_eq!(detect_supported_image_mime_type(&png), Some("image/png"));

        // Animated PNG (acTL before IDAT) is unsupported.
        let mut apng = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R',
        ];
        apng.extend_from_slice(&[0u8; 17]); // IHDR data (13) + CRC (4)
        apng.extend_from_slice(&8u32.to_be_bytes()); // acTL chunk length
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&[0u8; 12]); // data + CRC
        assert_eq!(detect_supported_image_mime_type(&apng), None);
    }

    #[test]
    fn encodes_base64_with_padding() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn detects_bmp_headers() {
        let mut bytes = vec![0u8; 58];
        bytes[0] = 0x42;
        bytes[1] = 0x4D;
        bytes[2..6].copy_from_slice(&58u32.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&1i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1i32.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        bytes[34..38].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(detect_supported_image_mime_type(&bytes), Some("image/bmp"));
    }
}
