//! UF2 container for a raw RP2040 flash image (loaded by the BOOTSEL bootloader).

const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;
const FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;
const RP2040_FAMILY_ID: u32 = 0xE48B_FF56;
pub const FLASH_BASE: u32 = 0x1000_0000;
const PAGE: usize = 256;
pub const BLOCK_LEN: usize = 512;

#[cfg(test)]
/// Size of the UF2 file for a flash image of `flash_len` bytes.
pub fn file_len(flash_len: usize) -> usize {
    flash_len.div_ceil(PAGE) * BLOCK_LEN
}

/// Wraps a raw flash image into UF2 blocks starting at `FLASH_BASE`. Every page is included
/// (also all-0xFF ones), so flashing the file restores the exact image.
pub fn from_flash_image(image: &[u8]) -> Vec<u8> {
    from_flash_range(image, 0, image.len())
}

/// Like `from_flash_image`, but only for the bytes `start..end` of the image (both must be
/// multiples of the 256-byte page). The bootloader leaves everything outside the range alone.
pub fn from_flash_range(image: &[u8], start: usize, end: usize) -> Vec<u8> {
    assert!(start.is_multiple_of(PAGE) && end.is_multiple_of(PAGE) && start <= end && end <= image.len());
    let total = ((end - start) / PAGE) as u32;
    let mut out = Vec::with_capacity(total as usize * BLOCK_LEN);
    for (n, chunk) in image[start..end].chunks(PAGE).enumerate() {
        for word in [
            MAGIC_START0,
            MAGIC_START1,
            FLAG_FAMILY_ID_PRESENT,
            FLASH_BASE + (start + n * PAGE) as u32,
            PAGE as u32,
            n as u32,
            total,
            RP2040_FAMILY_ID,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(chunk);
        out.resize(out.len() + (476 - chunk.len()), 0);
        out.extend_from_slice(&MAGIC_END.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_well_formed_and_carry_the_image() {
        let image: Vec<u8> = (0..PAGE * 3).map(|i| (i % 251) as u8).collect();
        let uf2 = from_flash_image(&image);
        assert_eq!(uf2.len(), file_len(image.len()));
        let word = |b: &[u8], i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let mut restored = Vec::new();
        for (n, b) in uf2.chunks(BLOCK_LEN).enumerate() {
            assert_eq!([word(b, 0), word(b, 1), word(b, 127)], [MAGIC_START0, MAGIC_START1, MAGIC_END]);
            assert_eq!(word(b, 3), FLASH_BASE + (n * PAGE) as u32);
            assert_eq!((word(b, 5), word(b, 6), word(b, 7)), (n as u32, 3, RP2040_FAMILY_ID));
            restored.extend_from_slice(&b[32..32 + PAGE]);
        }
        assert_eq!(restored, image);
    }

    #[test]
    fn range_covers_only_the_requested_pages() {
        let image: Vec<u8> = (0..PAGE * 6).map(|i| (i / PAGE) as u8).collect();
        let uf2 = from_flash_range(&image, PAGE * 2, PAGE * 5);
        assert_eq!(uf2.len(), 3 * BLOCK_LEN);
        for (n, b) in uf2.chunks(BLOCK_LEN).enumerate() {
            let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
            assert_eq!((word(3), word(5), word(6)), (FLASH_BASE + ((2 + n) * PAGE) as u32, n as u32, 3));
            assert!(b[32..32 + PAGE].iter().all(|&x| x == (2 + n) as u8));
        }
    }

    #[test]
    fn full_flash_size() {
        assert_eq!(file_len(2 * 1024 * 1024), 4 * 1024 * 1024);
    }
}
