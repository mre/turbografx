//! HuC6280 programmable timer (hardware page `$FF`, offset `$0C00..=$0FFF`).
//!
//! The timer counts down from a reload value at a fixed ~6.99 kHz tick (the
//! 7.16 MHz CPU clock divided by 1024). When it underflows it reloads and
//! raises the TIQ interrupt line.
//!
//! Registers (mirrored across the window, decoded on the low address bit):
//!
//! - offset `$0C00` — reload value (7 bits) on write; counter value on read.
//! - offset `$0C01` — control: bit 0 enables/starts the timer.
//!
//! This is a minimal model: it ticks per scanline rather than per CPU cycle.
//! Good enough to exercise the interrupt path; refine when you need accurate
//! timer-driven effects.

/// Number of CPU cycles between timer ticks (≈ 7.16 MHz / 1024).
pub const CYCLES_PER_TICK: u64 = 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct Timer {
    reload: u8,
    counter: u8,
    running: bool,
    /// TIQ line: set on underflow, cleared by acknowledging via the interrupt
    /// controller (`$1403` write).
    irq: bool,
    /// Accumulated CPU cycles, used to derive ticks.
    cycle_accumulator: u64,
}

impl Timer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reload: 0,
            counter: 0,
            running: false,
            irq: false,
            cycle_accumulator: 0,
        }
    }

    #[must_use]
    pub const fn read(&self, offset: u16) -> u8 {
        match offset & 0x01 {
            0 => self.counter & 0x7F,
            _ => self.running as u8,
        }
    }

    pub const fn write(&mut self, offset: u16, value: u8) {
        match offset & 0x01 {
            0 => self.reload = value & 0x7F,
            _ => {
                let enable = value & 0x01 != 0;
                // Starting the timer reloads the counter.
                if enable && !self.running {
                    self.counter = self.reload;
                    self.cycle_accumulator = 0;
                }
                self.running = enable;
            }
        }
    }

    /// Advance the timer by `cycles` CPU cycles, raising TIQ on underflow.
    pub const fn step(&mut self, cycles: u64) {
        if !self.running {
            return;
        }
        self.cycle_accumulator += cycles;
        while self.cycle_accumulator >= CYCLES_PER_TICK {
            self.cycle_accumulator -= CYCLES_PER_TICK;
            if self.counter == 0 {
                self.counter = self.reload;
                self.irq = true;
            } else {
                self.counter -= 1;
            }
        }
    }

    /// Whether the TIQ line is currently asserted.
    #[must_use]
    pub const fn irq(&self) -> bool {
        self.irq
    }

    /// Acknowledge (clear) the TIQ line.
    pub const fn acknowledge(&mut self) {
        self.irq = false;
    }
}
