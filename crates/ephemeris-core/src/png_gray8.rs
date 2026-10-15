//! Minimal uncompressed gray8 → PNG writer (no external `image` crate).

/// Encode an 8-bit grayscale buffer as a PNG (filter=None, zlib store/deflate).
pub fn gray8_to_png(width: u32, height: u32, pixels: &[u8]) -> crate::Result<Vec<u8>> {
    let expected = (width as usize).saturating_mul(height as usize);
    if pixels.len() < expected {
        return Err(crate::AppError::Storage(format!(
            "gray8_to_png: need {expected} bytes, got {}",
            pixels.len()
        )));
    }
    // Raw image data with filter byte 0 per row.
    let mut raw = Vec::with_capacity(expected + height as usize);
    for row in 0..height as usize {
        raw.push(0); // filter None
        let start = row * width as usize;
        raw.extend_from_slice(&pixels[start..start + width as usize]);
    }
    let compressed = deflate_zlib(&raw);

    let mut out = Vec::with_capacity(compressed.len() + 256);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    write_chunk(&mut out, b"IHDR", &{
        let mut d = Vec::with_capacity(13);
        d.extend_from_slice(&width.to_be_bytes());
        d.extend_from_slice(&height.to_be_bytes());
        d.push(8); // bit depth
        d.push(0); // grayscale
        d.push(0); // compression
        d.push(0); // filter
        d.push(0); // interlace
        d
    })?;
    write_chunk(&mut out, b"IDAT", &compressed)?;
    write_chunk(&mut out, b"IEND", &[])?;
    Ok(out)
}

fn write_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) -> crate::Result<()> {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(ty);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
    Ok(())
}

/// zlib-wrapped DEFLATE stored blocks (no compression — fine for e-ink cards).
fn deflate_zlib(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    // zlib header: CMF/FLG (deflate, 32k, check bits)
    out.push(0x78);
    out.push(0x01);
    // Store blocks of ≤65535 bytes.
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let take = remaining.min(65535);
        let last = offset + take >= data.len();
        out.push(if last { 0x01 } else { 0x00 });
        let len = take as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + take]);
        offset += take;
    }
    let adler = adler32(data);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_signature() {
        let px = vec![255u8; 4];
        let png = gray8_to_png(2, 2, &px).unwrap();
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.len() > 40);
    }
}
