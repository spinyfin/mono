//! Format recognition and dimension extraction, from header bytes only.
//!
//! Deliberately hand-rolled over two formats rather than pulling in an image
//! decoding crate. What the ingest path actually needs is "is this really a
//! PNG/JPEG, and how big is it" — both answerable from a few dozen header
//! bytes without decoding a single pixel. A decoder would add a dependency
//! whose entire attack surface is parsing hostile image data, in a process
//! that holds the engine's database.
//!
//! The filename extension is never consulted. A worker that renames
//! `state.db` to `evidence.png` gets a typed rejection, because the magic
//! number is the evidence and the extension is only a claim.

use boss_protocol::AttachmentMediaType;

/// What a header scan learned about some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    pub media_type: AttachmentMediaType,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// How many leading bytes [`sniff`] ever needs. JPEG dimensions live in the
/// first SOF segment, which follows a variable run of metadata segments
/// (EXIF thumbnails, ICC profiles); 64 KiB clears every real camera/render
/// output while bounding the read for a file that is 8 MiB of nothing.
pub const HEADER_SCAN_BYTES: usize = 64 * 1024;

const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Recognise `bytes` as a supported image and read its dimensions, or return
/// `None` when it is neither a PNG nor a JPEG (or is a truncated one).
///
/// `bytes` may be a prefix of the file — [`HEADER_SCAN_BYTES`] is enough.
pub fn sniff(bytes: &[u8]) -> Option<ImageHeader> {
    sniff_png(bytes).or_else(|| sniff_jpeg(bytes))
}

/// PNG: an 8-byte signature, then the IHDR chunk, whose first two fields are
/// width and height as big-endian u32. IHDR is required by the spec to be the
/// first chunk, so its offset is fixed.
fn sniff_png(bytes: &[u8]) -> Option<ImageHeader> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    // 8 signature + 4 length + 4 type + 4 width + 4 height
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    Some(ImageHeader {
        media_type: AttachmentMediaType::Png,
        pixel_width: be_u32(&bytes[16..20]),
        pixel_height: be_u32(&bytes[20..24]),
    })
}

/// JPEG: `FF D8` (SOI), then a chain of marker segments. Dimensions live in
/// the first Start-Of-Frame marker, which is anywhere in the chain because
/// APPn/COM metadata segments precede it. Walk the chain by each segment's
/// declared length rather than scanning for `FF C0`, so a byte pattern inside
/// an EXIF thumbnail cannot be mistaken for a frame header.
fn sniff_jpeg(bytes: &[u8]) -> Option<ImageHeader> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut cursor = 2usize;
    loop {
        // Markers may be preceded by any number of 0xFF fill bytes.
        while bytes.get(cursor) == Some(&0xFF) && bytes.get(cursor + 1) == Some(&0xFF) {
            cursor += 1;
        }
        let (&0xFF, Some(&marker)) = (bytes.get(cursor)?, bytes.get(cursor + 1)) else {
            return None;
        };
        cursor += 2;
        match marker {
            // Standalone markers: no length field, nothing to skip.
            0x01 | 0xD0..=0xD9 => continue,
            // Start of scan — compressed data follows and there is no frame
            // header ahead of it. A JPEG that reaches here has none.
            0xDA => return None,
            _ => {}
        }
        let length = u16::from_be_bytes([*bytes.get(cursor)?, *bytes.get(cursor + 1)?]) as usize;
        // A segment length counts its own two bytes; anything shorter is
        // malformed and would make the walk loop forever.
        if length < 2 {
            return None;
        }
        let is_sof = matches!(marker, 0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC));
        if is_sof {
            // SOF payload: precision(1), height(2), width(2).
            let payload = bytes.get(cursor + 2..cursor + 7)?;
            return Some(ImageHeader {
                media_type: AttachmentMediaType::Jpeg,
                pixel_height: u16::from_be_bytes([payload[1], payload[2]]) as u32,
                pixel_width: u16::from_be_bytes([payload[3], payload[4]]) as u32,
            });
        }
        cursor = cursor.checked_add(length)?;
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// A minimal but structurally real PNG: signature + a well-formed IHDR
/// chunk. Nothing downstream decodes pixel data, so the image data chunks
/// are irrelevant to what `sniff` must get right. Shared with the store and
/// HTTP tests, which need bytes that survive a real ingest.
#[cfg(test)]
pub(crate) fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, colour type, …
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG whose SOF0 sits behind a decoy APP1 segment, so a test failure
    /// distinguishes "walked the segment chain" from "scanned for 0xFFC0".
    fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // APP1 whose payload contains a byte pattern that looks like SOF0
        // with absurd dimensions — a scanner would return those.
        let decoy = [0xFF, 0xC0, 0x00, 0x11, 0x08, 0xFF, 0xFF, 0xFF, 0xFE];
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((decoy.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&decoy);
        // The real SOF0.
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x11, 0]);
        out
    }

    #[test]
    fn reads_png_dimensions() {
        let header = sniff(&png_bytes(2880, 1800)).expect("png must be recognised");
        assert_eq!(header.media_type, AttachmentMediaType::Png);
        assert_eq!((header.pixel_width, header.pixel_height), (2880, 1800));
    }

    #[test]
    fn reads_jpeg_dimensions_past_metadata_segments() {
        let header = sniff(&jpeg_bytes(1440, 900)).expect("jpeg must be recognised");
        assert_eq!(header.media_type, AttachmentMediaType::Jpeg);
        assert_eq!(
            (header.pixel_width, header.pixel_height),
            (1440, 900),
            "dimensions must come from the real SOF0, not the decoy inside APP1"
        );
    }

    #[test]
    fn rejects_non_images() {
        // A SQLite database renamed `evidence.png` is the case that makes
        // extension-trust unsafe: the engine would otherwise copy it into an
        // HTTP-served store.
        assert!(sniff(b"SQLite format 3\0rest of a database").is_none());
        assert!(sniff(b"").is_none());
        assert!(sniff(b"\x89PNG\r\n\x1a\n truncated").is_none(), "truncated PNG");
    }

    #[test]
    fn rejects_jpeg_with_no_frame_header() {
        // SOI straight into SOS: no SOF, so no dimensions to be had. Must
        // terminate rather than walk off the end or spin.
        assert!(sniff(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]).is_none());
    }

    #[test]
    fn zero_length_segment_terminates() {
        // A declared length below its own 2-byte size would leave the cursor
        // unmoved; the walk must refuse rather than loop forever.
        assert!(sniff(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x00, 0x00]).is_none());
    }
}
