//! HuCard (cartridge) ROM handling.
//!
//! A HuCard maps into physical banks `$00..=$7F` (up to 1 MiB). Most cards have
//! no mapper and simply present their ROM; the console's MMU does all the
//! banking. A handful of sizes need special treatment, which we approximate
//! here and flag with TODOs.

use std::io::Read;
use std::path::Path;

/// Size of one physical bank: 8 KiB.
pub const BANK_SIZE: usize = 0x2000;

/// A loaded HuCard.
#[derive(Clone, Debug)]
pub struct Cartridge {
    rom: Vec<u8>,
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
        Self { rom: data }
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
        let index = (phys as usize) % self.rom.len();
        self.rom[index]
    }
}
