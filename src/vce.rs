//! HuC6260 VCE — Video Color Encoder (hardware page `$FF`, offset
//! `$0400..=$07FF`).
//!
//! The VCE owns the color palette and the dot-clock selection. The palette is
//! 512 entries of 9-bit color (3 bits each of G, R, B). Entries 0..255 are the
//! background/tile palettes (16 sub-palettes of 16 colors); entries 256..511
//! are the sprite palettes.
//!
//! Registers (decoded on the low 3 bits, mirrored across the window):
//!
//! - `$0400` control: bits 0-1 select the dot clock (5.37 / 7.16 / 10.74 MHz).
//! - `$0402/$0403` color-table address (write pointer into the 512 entries).
//! - `$0404/$0405` color-table data (auto-increments the address on access).

/// Number of palette entries.
pub const PALETTE_ENTRIES: usize = 512;

#[derive(Clone, Debug)]
pub struct Vce {
    control: u8,
    /// Address into the color table (9 bits).
    address: u16,
    /// 9-bit color values: `0bGGG_RRR_BBB`.
    palette: [u16; PALETTE_ENTRIES],
}

impl Default for Vce {
    fn default() -> Self {
        Self::new()
    }
}

impl Vce {
    #[must_use]
    pub fn new() -> Self {
        Self {
            control: 0,
            address: 0,
            palette: [0; PALETTE_ENTRIES],
        }
    }

    pub fn write(&mut self, offset: u16, value: u8) {
        match offset & 0x07 {
            0x00 => self.control = value,
            0x02 => self.address = (self.address & 0x0100) | u16::from(value),
            0x03 => self.address = (self.address & 0x00FF) | (u16::from(value & 0x01) << 8),
            0x04 => {
                let entry = self.address as usize % PALETTE_ENTRIES;
                self.palette[entry] = (self.palette[entry] & 0x0100) | u16::from(value);
            }
            0x05 => {
                let entry = self.address as usize % PALETTE_ENTRIES;
                self.palette[entry] =
                    (self.palette[entry] & 0x00FF) | (u16::from(value & 0x01) << 8);
                // The address auto-increments after the high byte is written.
                self.address = (self.address + 1) & 0x01FF;
            }
            _ => {}
        }
    }

    #[must_use]
    pub fn read(&mut self, offset: u16) -> u8 {
        match offset & 0x07 {
            0x04 => {
                let entry = self.address as usize % PALETTE_ENTRIES;
                (self.palette[entry] & 0xFF) as u8
            }
            0x05 => {
                let entry = self.address as usize % PALETTE_ENTRIES;
                let value = ((self.palette[entry] >> 8) & 0x01) as u8 | 0xFE;
                // Like writes, reading the high byte advances the CRAM address.
                self.address = (self.address + 1) & 0x01FF;
                value
            }
            _ => 0xFF,
        }
    }

    /// The selected dot clock as a 0..=2 index (5.37 / 7.16 / 10.74 MHz).
    #[must_use]
    pub const fn dot_clock_select(&self) -> u8 {
        self.control & 0x03
    }

    /// The raw 9-bit color value of a palette entry, for debugging.
    #[must_use]
    pub fn color_raw(&self, entry: usize) -> u16 {
        self.palette[entry % PALETTE_ENTRIES]
    }

    /// Look up a palette entry and expand it to 0xAARRGGBB for a framebuffer.
    ///
    /// The 9-bit `0bGGG_RRR_BBB` value is scaled to 8 bits per channel.
    #[must_use]
    pub fn color_argb(&self, entry: usize) -> u32 {
        let raw = self.palette[entry % PALETTE_ENTRIES];
        let g = ((raw >> 6) & 0x07) as u32;
        let r = ((raw >> 3) & 0x07) as u32;
        let b = (raw & 0x07) as u32;
        // Scale 3-bit (0..7) channels up to 8-bit (0..255).
        let expand = |c: u32| (c * 255 / 7) & 0xFF;
        0xFF00_0000 | (expand(r) << 16) | (expand(g) << 8) | expand(b)
    }
}
