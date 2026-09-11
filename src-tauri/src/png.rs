//! Minimal RGBA→PNG writer.
//!
//! Evidence for the tray icons has to be an image, and the icons themselves are
//! raw RGBA byte buffers. Pulling in a full encoder (or an image stack) just to
//! dump 32×32 tiles would grow the crate graph for no runtime benefit, so this
//! module writes the smallest valid PNG there is: a zlib stream made of
//! *stored* deflate blocks plus the four required chunks. Any decoder reads it
//! (PNG spec §3.9 — stored blocks are legal deflate).

/// CRC-32 (IEEE 802.3) as required by every PNG chunk.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32 over the *uncompressed* data, the zlib trailer.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b): (u32, u32) = (1, 0);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut crc_input = Vec::with_capacity(4 + payload.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib (RFC 1950) wrapping a deflate stream of stored blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    out.push(0x78); // CM=8 (deflate), CINFO=7 (32K window)
    out.push(0x01); // FCHECK so that (0x78 << 8 | 0x01) % 31 == 0
    let mut offset = 0usize;
    while offset < raw.len() {
        let len = (raw.len() - offset).min(65_535);
        let last = offset + len >= raw.len();
        out.push(if last { 1 } else { 0 }); // BFINAL + BTYPE=00
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&(!(len as u16)).to_le_bytes());
        out.extend_from_slice(&raw[offset..offset + len]);
        offset += len;
    }
    if raw.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Encode tightly packed row-major RGBA8 pixels as a PNG.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(
        rgba.len(),
        (width as usize) * (height as usize) * 4,
        "rgba buffer does not match {width}x{height}"
    );

    // Raw scanlines, each prefixed with filter type 0 (None).
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity(height as usize * (stride + 1));
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace

    let mut out = Vec::with_capacity(raw.len() + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_parseable_header() {
        let png = encode_rgba(2, 2, &[0u8; 16]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &2u32.to_be_bytes());
        // Last chunk is IEND: zero length, "IEND", then its CRC.
        assert_eq!(
            &png[png.len() - 12..png.len() - 4],
            &[0, 0, 0, 0, b'I', b'E', b'N', b'D']
        );
    }

    #[test]
    fn crc_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn adler_matches_known_vector() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
