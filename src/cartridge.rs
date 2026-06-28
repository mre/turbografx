//! HuCard (cartridge) ROM handling.
//!
//! A HuCard maps into physical banks `$00..=$7F` (up to 1 MiB). Most cards have
//! no mapper and simply present their ROM; the console's MMU does all the
//! banking.
//!
//! The one exception is **Street Fighter II' Champion Edition**, a 2.5 MiB card
//! that can't fit in the 1 MiB window. It carries a custom mapper: banks
//! `$00..=$3F` are fixed, and banks `$40..=$7F` form a 512 KiB window that is
//! switched between four pages by *writing* to a HuCard offset of
//! `$1FF0..=$1FF3` (the low two address bits select the page). We detect this
//! mapper from the ROM size (> 1 MiB).

use crate::io::PadMode;

use std::io::Read;
use std::path::Path;

/// Size of one physical bank: 8 KiB.
pub const BANK_SIZE: usize = 0x2000;

/// Size of the Street Fighter II bank-switch page (512 KiB = 64 banks).
const SF2_PAGE_SIZE: usize = 0x80_000;

/// A loaded HuCard.
#[derive(Clone, Debug)]
pub struct Cartridge {
    rom: Vec<u8>,
    /// CRC-32 of the header-stripped ROM image, used for compatibility quirks.
    crc32: u32,
    /// `true` if this is an oversized (Street Fighter II) card with the custom
    /// `$40..=$7F` bank-switch mapper.
    sf2: bool,
    /// Currently selected SF2 page (0..=3) for the `$40..=$7F` window.
    sf2_page: u8,
}

impl Cartridge {
    /// Load a HuCard image from raw bytes.
    ///
    /// Some dumps carry a 512-byte copier header; if the length is `512 + a
    /// multiple of 8 KiB`, we strip it.
    #[must_use]
    pub fn from_bytes(mut data: Vec<u8>) -> Self {
        if data.len() % BANK_SIZE == 512 {
            // Strip the 512-byte header some old dumps prepend.
            data.drain(0..512);
        }
        // Cards larger than the 1 MiB HuCard window use the SF2 mapper.
        let sf2 = data.len() > 0x10_0000;
        let crc32 = crc32(&data);
        Self {
            rom: data,
            crc32,
            sf2,
            sf2_page: 0,
        }
    }

    /// CRC-32 of the header-stripped ROM image.
    #[must_use]
    pub const fn crc32(&self) -> u32 {
        self.crc32
    }

    /// Preferred pad mode for known controller-sensitive games.
    #[must_use]
    pub fn recommended_pad_mode(&self) -> Option<PadMode> {
        crate::game_db::pad_mode_for_crc(self.crc32)
    }

    /// Number of 8 KiB banks in the image.
    #[must_use]
    pub fn bank_count(&self) -> usize {
        self.rom.len().div_ceil(BANK_SIZE)
    }

    /// Load a HuCard from a path. Accepts either a raw image (`.pce`/`.bin`) or
    /// a `.zip` archive, in which case the first file that looks like a ROM
    /// (`.pce`/`.bin`, or the only entry) is extracted on the fly.
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be read or a zip contains no usable
    /// ROM entry.
    pub fn from_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        let is_zip = bytes.starts_with(b"PK\x03\x04")
            || path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        if is_zip {
            Self::from_zip_bytes(&bytes)
        } else {
            Ok(Self::from_bytes(bytes))
        }
    }

    /// Extract the first ROM entry from an in-memory zip archive.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive is invalid or holds no usable entry.
    pub fn from_zip_bytes(bytes: &[u8]) -> std::io::Result<Self> {
        let reader = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Prefer an entry with a known ROM extension; otherwise take the first
        // regular file in the archive.
        let mut chosen: Option<usize> = None;
        let mut fallback: Option<usize> = None;
        for i in 0..archive.len() {
            let file = archive
                .by_index(i)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if !file.is_file() {
                continue;
            }
            let name = file.name().to_ascii_lowercase();
            if name.ends_with(".pce") || name.ends_with(".bin") {
                chosen = Some(i);
                break;
            }
            fallback.get_or_insert(i);
        }

        let index = chosen.or(fallback).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no ROM entry in zip archive")
        })?;

        let mut file = archive
            .by_index(index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut data = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
        file.read_to_end(&mut data)?;
        Ok(Self::from_bytes(data))
    }

    /// Read a byte at a physical address inside the HuCard region.
    ///
    /// `phys` is the 21-bit physical address; only banks `$00..=$7F` are valid
    /// here. Out-of-range reads return open-bus (`$FF`).
    ///
    /// For SF2 cards, banks `$40..=$7F` are offset by the selected page.
    ///
    /// TODO: implement accurate mirroring for the irregular sizes. Real HuCards
    /// without a mapper just wire the high address bit as a chip-enable, so e.g.
    /// 384 KiB cards mirror the top 256 KiB into the upper half of the address
    /// window. For now we mirror by modulo, which is correct for power-of-two
    /// sizes and "good enough" to boot most images.
    #[must_use]
    pub fn read(&self, phys: u32) -> u8 {
        if self.rom.is_empty() {
            return 0xFF;
        }
        let mut index = phys as usize;
        if self.sf2 {
            let bank = (phys >> 13) & 0xFF;
            // Banks $40..=$7F are the switchable window; add the page offset.
            if (0x40..=0x7F).contains(&bank) {
                index += self.sf2_page as usize * SF2_PAGE_SIZE;
            }
        }
        self.rom[index % self.rom.len()]
    }

    /// Handle a write into the HuCard region.
    ///
    /// HuCard ROM ignores writes, but the SF2 mapper latches its page select
    /// when the CPU writes to **physical bank `$00`** at a card offset of
    /// `$1FF0..=$1FFF` (the low bits choose the page). The bank-`$00`
    /// restriction matters: the `$40..=$7F` window banks that SF2 streams
    /// graphics and palette data from also contain `$1FFx` offsets, and an
    /// incidental write there must *not* be mistaken for a page select.
    pub fn write(&mut self, phys: u32, _value: u8) {
        if self.sf2 && (phys >> 13) == 0 && (phys & 0x1FF0) == 0x1FF0 {
            self.sf2_page = (phys & 0x0F) as u8;
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{Cartridge, crc32};
    use crate::io::PadMode;

    #[test]
    fn crc32_matches_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn strips_copier_header_before_crc() {
        let mut headered = vec![0xAA; 512];
        headered.extend_from_slice(&vec![0u8; 0x2000]);
        let plain = Cartridge::from_bytes(vec![0u8; 0x2000]);
        let with_header = Cartridge::from_bytes(headered);
        assert_eq!(with_header.crc32(), plain.crc32());
    }

    #[test]
    fn recommends_six_button_for_street_fighter_crc() {
        // This test pins the compatibility-db plumbing; the CRC value comes from
        // known Street Fighter II' - Champion Edition dumps.
        let cart = Cartridge {
            rom: Vec::new(),
            crc32: 0xD15C_B6BB,
            sf2: false,
            sf2_page: 0,
        };
        assert_eq!(cart.recommended_pad_mode(), Some(PadMode::AvenuePad6));
    }
}
