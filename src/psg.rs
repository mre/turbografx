//! PSG — the 6-channel programmable sound generator built into the HuC6280
//! (hardware page `$FF`, offset `$0800..=$0BFF`).
//!
//! Not emulated yet. This stub accepts register writes so games that program
//! the PSG during boot don't wedge on an unmapped write, and it keeps a copy of
//! the register file for when you add audio. Audio output, channel mixing and
//! the LFO/noise generators are all TODO.

/// The PSG exposes 16 registers, selected by a channel-select latch.
#[derive(Clone, Copy, Debug)]
pub struct Psg {
    /// Currently selected channel (`$0800`).
    channel_select: u8,
    /// Main amplitude (`$0801`).
    main_volume: u8,
    /// Per-channel register file. Indexed `[channel][register]`; exact layout
    /// to be defined when audio lands.
    registers: [[u8; 8]; 6],
}

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

impl Psg {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            channel_select: 0,
            main_volume: 0,
            registers: [[0; 8]; 6],
        }
    }

    pub const fn write(&mut self, offset: u16, value: u8) {
        match offset & 0x0F {
            0x00 => self.channel_select = value & 0x07,
            0x01 => self.main_volume = value,
            reg => {
                let ch = self.channel_select as usize;
                if ch < self.registers.len() {
                    self.registers[ch][(reg as usize - 2) & 0x07] = value;
                }
            }
        }
    }

    /// The PSG registers are write-only on hardware; reads return open bus.
    #[must_use]
    pub const fn read(&self, _offset: u16) -> u8 {
        0xFF
    }
}
