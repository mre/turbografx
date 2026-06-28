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

/// Standard and Avenue Pad 6 buttons. The first eight buttons are returned by a
/// standard 2-button pad; III-VI are returned by the Avenue Pad 6 extra-button
/// phase.
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
    pub button_iii: bool,
    pub button_iv: bool,
    pub button_v: bool,
    pub button_vi: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IoPort {
    select: bool,
    clear: bool,
    pad: PadState,
    six_button: bool,
    selected_extra_buttons: bool,
    /// D6 (country): `true` = Japanese PC Engine, `false` = US TurboGrafx-16.
    japanese: bool,
    /// D7 (CD sense): `true` = a CD-ROM base unit is attached (reads as 0).
    cd_attached: bool,
}

impl IoPort {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            // Match Geargrafx/hardware reset: both output lines idle high. The
            // Avenue Pad 6 protocol toggles extra-button phase on CLR falling
            // edges, so starting CLR low inverts the button phases.
            select: true,
            clear: true,
            pad: PadState {
                up: false,
                right: false,
                down: false,
                left: false,
                select: false,
                run: false,
                button_i: false,
                button_ii: false,
                button_iii: false,
                button_iv: false,
                button_v: false,
                button_vi: false,
            },
            six_button: false,
            selected_extra_buttons: false,
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

    /// Enable Avenue Pad 6 protocol for games that support/require it.
    pub const fn set_six_button(&mut self, enabled: bool) {
        self.six_button = enabled;
        self.selected_extra_buttons = false;
    }

    /// Replace the current pad state (call once per frame from your input code).
    pub const fn set_pad(&mut self, pad: PadState) {
        self.pad = pad;
    }

    pub const fn write(&mut self, value: u8) {
        let prev_clear = self.clear;
        self.select = value & 0x01 != 0;
        self.clear = value & 0x02 != 0;
        if self.six_button && prev_clear && !self.clear {
            self.selected_extra_buttons = !self.selected_extra_buttons;
        }
    }

    fn direction_nibble(&self) -> u8 {
        // SEL = 1: directions -> D3..D0 = Left / Down / Right / Up.
        (u8::from(self.pad.left) << 3)
            | (u8::from(self.pad.down) << 2)
            | (u8::from(self.pad.right) << 1)
            | u8::from(self.pad.up)
    }

    fn button_nibble(&self) -> u8 {
        // SEL = 0: buttons -> D3..D0 = Run / Select / II / I.
        (u8::from(self.pad.run) << 3)
            | (u8::from(self.pad.select) << 2)
            | (u8::from(self.pad.button_ii) << 1)
            | u8::from(self.pad.button_i)
    }

    fn extra_button_nibble(&self) -> u8 {
        // Avenue Pad 6 extra phase, SEL = 0: D3..D0 = VI / V / IV / III.
        (u8::from(self.pad.button_vi) << 3)
            | (u8::from(self.pad.button_v) << 2)
            | (u8::from(self.pad.button_iv) << 1)
            | u8::from(self.pad.button_iii)
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

        let nibble = if self.clear {
            // While CLR is high the pad returns no data (low nibble reads 0).
            return high;
        } else if self.six_button && self.selected_extra_buttons {
            if self.select {
                // Avenue Pad 6 ID/extra phase: Geargrafx leaves the low nibble
                // at 0 when SEL is high. Games use this when detecting the
                // 6-button pad; returning 0xF here makes extra buttons invisible.
                return high;
            } else {
                self.extra_button_nibble()
            }
        } else if self.select {
            self.direction_nibble()
        } else {
            self.button_nibble()
        };
        // Joypad data is active-low in the low nibble.
        high | (!nibble & 0x0F)
    }
}

#[cfg(test)]
mod tests {
    use super::{IoPort, PadState};

    #[test]
    fn direction_nibble_matches_standard_pad_bit_order() {
        let cases = [
            (
                PadState {
                    up: true,
                    ..PadState::default()
                },
                0x0E,
            ),
            (
                PadState {
                    right: true,
                    ..PadState::default()
                },
                0x0D,
            ),
            (
                PadState {
                    down: true,
                    ..PadState::default()
                },
                0x0B,
            ),
            (
                PadState {
                    left: true,
                    ..PadState::default()
                },
                0x07,
            ),
        ];

        for (pad, low_nibble) in cases {
            let mut io = IoPort::new();
            io.set_pad(pad);
            io.write(0x01); // SEL=1, CLR=0: direction nibble.
            assert_eq!(io.read() & 0x0F, low_nibble);
        }
    }

    #[test]
    fn avenue_pad_6_returns_extra_buttons_on_alternate_clear_phase() {
        let mut io = IoPort::new();
        io.set_six_button(true);
        io.set_pad(PadState {
            button_iii: true,
            ..PadState::default()
        });

        // From reset CLR is high; the first falling edge selects the Avenue Pad 6
        // extra-button phase.
        io.write(0x00);
        assert_eq!(io.read() & 0x0F, 0x0E); // button III is active-low bit 0.
        io.write(0x01); // SEL=1 during extra phase returns the pad ID low nibble.
        assert_eq!(io.read() & 0x0F, 0x00);

        // Another falling edge returns to the standard phase, where III is not
        // visible as I/II/Select/Run.
        io.write(0x02);
        io.write(0x00);
        assert_eq!(io.read() & 0x0F, 0x0F);
    }
}
