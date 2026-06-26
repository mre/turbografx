//! Joypad / I/O port (hardware page `$FF`, offset `$1000..=$13FF`).
//!
//! The PC Engine multiplexes its controller through a single byte port. Writing
//! controls two lines:
//!
//! - bit 0 (SEL) selects which controller nibble is returned.
//! - bit 1 (CLR) clears/latches the pad shift register.
//!
//! Reading returns the active nibble in the low 4 bits (active-low). The high
//! bits carry region/CD signals we stub out.
//!
//! This is a placeholder: it reports "no buttons pressed" and ignores the
//! multitap. Wire real input in once the video side is up.

/// The eight buttons on a standard 2-button pad, in the order the hardware
/// returns them across the two SEL nibbles.
#[derive(Clone, Copy, Debug, Default)]
pub struct PadState {
    pub up: bool,
    pub right: bool,
    pub down: bool,
    pub left: bool,
    pub select: bool,
    pub run: bool,
    pub button_i: bool,
    pub button_ii: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IoPort {
    select: bool,
    clear: bool,
    pad: PadState,
    /// D6 (country): `true` = Japanese PC Engine, `false` = US TurboGrafx-16.
    japanese: bool,
    /// D7 (CD sense): `true` = a CD-ROM base unit is attached (reads as 0).
    cd_attached: bool,
}

impl IoPort {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            select: false,
            clear: false,
            pad: PadState {
                up: false,
                right: false,
                down: false,
                left: false,
                select: false,
                run: false,
                button_i: false,
                button_ii: false,
            },
            // Default to a US TurboGrafx-16 with no CD attached. Many US games
            // perform a region check on D6 and lock out on a Japanese machine.
            japanese: false,
            cd_attached: false,
        }
    }

    /// Select the console region. `true` = Japanese PC Engine (D6 reads 1),
    /// `false` = US TurboGrafx-16 (D6 reads 0).
    pub const fn set_japanese(&mut self, japanese: bool) {
        self.japanese = japanese;
    }

    /// Set whether a CD-ROM base unit is attached (D7 reads 0 when attached).
    pub const fn set_cd_attached(&mut self, attached: bool) {
        self.cd_attached = attached;
    }

    /// Replace the current pad state (call once per frame from your input code).
    pub const fn set_pad(&mut self, pad: PadState) {
        self.pad = pad;
    }

    pub const fn write(&mut self, value: u8) {
        self.select = value & 0x01 != 0;
        self.clear = value & 0x02 != 0;
    }

    /// Read the port. Buttons are active-low: a pressed button reads as 0.
    ///
    /// High bits: D7 = CD sense (1 = not attached), D6 = country
    /// (1 = Japanese), D5/D4 = always 1.
    #[must_use]
    pub fn read(&self) -> u8 {
        let mut high = 0x30; // D5 and D4 are always 1.
        if !self.cd_attached {
            high |= 0x80; // D7 = 1 when no CD unit is attached.
        }
        if self.japanese {
            high |= 0x40; // D6 = 1 on Japanese models.
        }

        // While CLR is high the pad returns no data (low nibble reads 0).
        if self.clear {
            return high;
        }
        let nibble = if self.select {
            // SEL = 1: directions -> D3..D0 = Left / Down / Right / Up
            (u8::from(self.pad.left) << 3)
                | (u8::from(self.pad.down) << 2)
                | (u8::from(self.pad.right) << 1)
                | u8::from(self.pad.up)
        } else {
            // SEL = 0: buttons -> D3..D0 = Run / Select / II / I
            (u8::from(self.pad.run) << 3)
                | (u8::from(self.pad.select) << 2)
                | (u8::from(self.pad.button_ii) << 1)
                | u8::from(self.pad.button_i)
        };
        // Joypad data is active-low in the low nibble.
        high | (!nibble & 0x0F)
    }
}
