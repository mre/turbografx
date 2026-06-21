//! HuC6280 interrupt controller (hardware page `$FF`, offset `$1400..=$17FF`).
//!
//! The HuC6280 has three maskable interrupt sources, each with its own vector:
//!
//! | Source | Bit | Vector  | Notes                                   |
//! |--------|-----|---------|-----------------------------------------|
//! | IRQ2   | 0   | `$FFF6` | External / CD-ROM (also the BRK vector) |
//! | IRQ1   | 1   | `$FFF8` | Raised by the VDC (vblank / raster)     |
//! | TIQ    | 2   | `$FFFA` | Raised by the programmable timer        |
//!
//! Two registers are exposed (mirrored across the 1 KiB window):
//!
//! - offset `$1402` — **Interrupt Disable**. A set bit *masks* that source.
//! - offset `$1403` — **Interrupt Request**. Reads the pending sources; writing
//!   acknowledges (clears) the timer interrupt.
//!
//! IRQ1 (the VDC line) is *not* cleared here — it is cleared when the CPU reads
//! the VDC status register. IRQ2 likewise depends on external hardware.

/// Disable/request bit for the external IRQ2 line.
pub const IRQ2: u8 = 0b0000_0001;
/// Disable/request bit for the VDC's IRQ1 line.
pub const IRQ1: u8 = 0b0000_0010;
/// Disable/request bit for the timer's TIQ line.
pub const TIQ: u8 = 0b0000_0100;

/// State of the on-chip interrupt controller.
#[derive(Clone, Copy, Debug, Default)]
pub struct InterruptController {
    /// `$1402`: set bit = source masked.
    disable: u8,
}

impl InterruptController {
    #[must_use]
    pub const fn new() -> Self {
        Self { disable: 0 }
    }

    /// Read a controller register. `offset` is the low 2 bits of the address.
    ///
    /// `sources` is the current set of *raised* lines (see [`IRQ1`], [`TIQ`],
    /// [`IRQ2`]), gathered by the bus from the VDC and timer.
    #[must_use]
    pub const fn read(&self, offset: u16, sources: u8) -> u8 {
        match offset & 0x03 {
            0x02 => self.disable,
            0x03 => sources & 0x07,
            // Open bus on the unmapped registers.
            _ => 0xFF,
        }
    }

    /// Write a controller register. Returns `true` if the write acknowledged
    /// the timer interrupt (so the caller can clear the timer's TIQ line).
    pub const fn write(&mut self, offset: u16, value: u8) -> bool {
        match offset & 0x03 {
            0x02 => {
                self.disable = value & 0x07;
                false
            }
            // Any write to $1403 acknowledges the timer interrupt.
            0x03 => true,
            _ => false,
        }
    }

    /// Given the set of raised `sources`, return those that are not masked.
    #[must_use]
    pub const fn active(&self, sources: u8) -> u8 {
        sources & !self.disable & 0x07
    }

    /// Whether any unmasked source is currently requesting an interrupt.
    #[must_use]
    pub const fn pending(&self, sources: u8) -> bool {
        self.active(sources) != 0
    }
}
