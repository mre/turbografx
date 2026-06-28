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
//! This models one connected pad. Multitap and mouse protocols are still left
//! for later.

/// Which physical PC Engine/TurboGrafx pad is connected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PadMode {
    /// Standard 2-button pad: D-pad, I, II, Select and Run.
    #[default]
    TwoButton,
    /// Avenue Pad 3 with its third action button wired to Select.
    AvenuePad3Select,
    /// Avenue Pad 3 with its third action button wired to Run.
    AvenuePad3Run,
    /// Avenue Pad 6/Arcade Pad 6 in 6-button mode.
    AvenuePad6,
}

/// Standard, Avenue Pad 3 and Avenue Pad 6 buttons. The first eight buttons are
/// returned by a standard 2-button pad; III can be mapped to Select/Run for the
/// Avenue Pad 3; III-VI are returned by the Avenue Pad 6 extra-button phase.
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

#[derive(Clone, Copy, Debug)]
pub struct IoPort {
    select: bool,
    clear: bool,
    pad: PadState,
    pad_mode: PadMode,
    /// Avenue Pad 6/turbo counter, clocked by CLR rising edges (74HC163 QA/QB).
    counter: u8,
    /// D6 (country): `true` = Japanese PC Engine, `false` = US TurboGrafx-16.
    japanese: bool,
    /// D7 (CD sense): `true` = a CD-ROM base unit is attached (reads as 0).
    cd_attached: bool,
}

impl Default for IoPort {
    fn default() -> Self {
        Self::new()
    }
}

impl IoPort {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            // Match hardware reset: both output lines idle high. The Avenue
            // Pad 6 counter advances on CLR rising edges, so starting CLR low
            // would invert the standard/extra scan phases.
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
            pad_mode: PadMode::TwoButton,
            counter: 0,
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

    /// Select the connected pad type.
    pub const fn set_pad_mode(&mut self, mode: PadMode) {
        self.pad_mode = mode;
        self.counter = 0;
    }

    /// Return the connected pad type.
    #[must_use]
    pub const fn pad_mode(&self) -> PadMode {
        self.pad_mode
    }

    /// Enable Avenue Pad 6 protocol for games that support/require it.
    pub const fn set_six_button(&mut self, enabled: bool) {
        self.pad_mode = if enabled {
            PadMode::AvenuePad6
        } else {
            PadMode::TwoButton
        };
        self.counter = 0;
    }

    /// Replace the current pad state (call once per frame from your input code).
    pub const fn set_pad(&mut self, pad: PadState) {
        self.pad = pad;
    }

    pub const fn write(&mut self, value: u8) {
        let prev_clear = self.clear;
        self.select = value & 0x01 != 0;
        self.clear = value & 0x02 != 0;
        if !prev_clear && self.clear {
            // CLR clocks the controller counter on its rising edge. Avenue Pad 6
            // uses QA to alternate standard and extra-button scans; QB drives
            // turbo/slow-motion hardware on real pads.
            self.counter = self.counter.wrapping_add(1) & 0x03;
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
        // Avenue Pad 3 is electrically a normal 2-button pad with the third
        // action button switchable to either Select or Run.
        let run = self.pad.run || (self.pad_mode == PadMode::AvenuePad3Run && self.pad.button_iii);
        let select =
            self.pad.select || (self.pad_mode == PadMode::AvenuePad3Select && self.pad.button_iii);
        (u8::from(run) << 3)
            | (u8::from(select) << 2)
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

        let extra_phase = self.pad_mode == PadMode::AvenuePad6 && self.counter & 1 != 0;
        let nibble = if self.clear {
            // While CLR is high the pad outputs all lows (active-low 0000).
            return high;
        } else if extra_phase {
            if self.select {
                // Avenue Pad 6 ID/extra phase: the direction nibble is forced to
                // 0000, an impossible D-pad state that compatible games detect.
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
    use super::{IoPort, PadMode, PadState};

    #[test]
    fn default_matches_hardware_reset_state() {
        let mut io = IoPort::default();
        io.set_six_button(true);

        // Starting CLR high means the first normal scan pulse advances to the
        // extra-button phase. If Default drifted from new(), this reads 0x0F.
        io.write(0x01);
        io.write(0x03);
        io.write(0x01);
        assert_eq!(io.read() & 0x0F, 0x00);
    }

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
    fn avenue_pad_3_maps_third_button_to_selected_system_button() {
        let mut io = IoPort::new();
        io.set_pad(PadState {
            button_iii: true,
            ..PadState::default()
        });

        io.write(0x00); // SEL=0, CLR=0: button nibble.
        assert_eq!(io.read() & 0x0F, 0x0F); // 2-button mode ignores III.

        io.set_pad_mode(PadMode::AvenuePad3Select);
        io.write(0x00);
        assert_eq!(io.read() & 0x0F, 0x0B); // Select is active-low bit 2.

        io.set_pad_mode(PadMode::AvenuePad3Run);
        io.write(0x00);
        assert_eq!(io.read() & 0x0F, 0x07); // Run is active-low bit 3.
    }

    #[test]
    fn avenue_pad_6_returns_extra_buttons_after_clear_rising_edge() {
        let mut io = IoPort::new();
        io.set_six_button(true);
        io.set_pad(PadState {
            button_iii: true,
            ..PadState::default()
        });

        // A normal scan sequence briefly raises CLR, then drops it before the
        // reads. The rising edge clocks the Avenue Pad 6 phase counter.
        io.write(0x01); // SEL=1, CLR=0.
        io.write(0x03); // SEL=1, CLR=1: counter -> extra phase.
        io.write(0x01); // SEL=1, CLR=0: read the 6-button ID/header.
        assert_eq!(io.read() & 0x0F, 0x00);

        io.write(0x00); // SEL=0, CLR=0: extra buttons.
        assert_eq!(io.read() & 0x0F, 0x0E); // button III is active-low bit 0.

        io.write(0x02); // SEL=0, CLR=1: counter -> standard phase.
        io.write(0x00); // SEL=0, CLR=0: standard buttons; III is hidden.
        assert_eq!(io.read() & 0x0F, 0x0F);
    }
}
