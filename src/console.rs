//! The assembled console: CPU + system bus, with a frame-stepping loop.

use crate::bus::SystemBus;
use crate::cartridge::Cartridge;
use crate::io::PadState;
use crate::vdc::{FB_HEIGHT, FB_WIDTH};
use crate::{CPU_CYCLES_PER_SCANLINE, SCANLINES_PER_FRAME};
use mos6502::cpu::CPU;
use mos6502::instruction::Huc6280;

/// A fully wired TurboGrafx-16.
pub struct Console {
    cpu: CPU<SystemBus, Huc6280>,
    /// Cycle accumulator used by [`Console::debug_step`] to pace the VDC.
    scanline_accumulator: u64,
}

impl Console {
    /// Build a console around a loaded HuCard and run the reset sequence.
    #[must_use]
    pub fn new(cartridge: Cartridge) -> Self {
        let bus = SystemBus::new(cartridge);
        let mut console = Self {
            cpu: CPU::new(bus, Huc6280),
            scanline_accumulator: 0,
        };
        console.reset();
        console
    }

    /// Perform the CPU reset sequence (loads the reset vector).
    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    /// Execute one instruction and keep the bus's MMU shadow in sync.
    ///
    /// Returns the number of CPU cycles the instruction consumed.
    fn step_instruction(&mut self) -> u64 {
        let before = self.cpu.cycles;
        let progressed = self.cpu.single_step();
        // TAM writes are mirrored to the bus by the core via
        // `Bus::set_mapping_register`, so the MMU shadow is already current.

        let delta = self.cpu.cycles.wrapping_sub(before);
        if !progressed && delta == 0 {
            // CPU halted (STP/JAM) or stalled (WAI); report a nominal cost so the
            // scanline loop still makes progress instead of spinning forever.
            1
        } else {
            delta
        }
    }

    /// Run the CPU for roughly one scanline, then advance the VDC by one line.
    ///
    /// Returns `true` when this was the final scanline of the frame (the picture
    /// is ready to present).
    pub fn step_scanline(&mut self) -> bool {
        let mut spent: u64 = 0;
        while spent < CPU_CYCLES_PER_SCANLINE {
            let cycles = self.step_instruction();
            self.cpu.memory.timer.step(cycles);
            spent += cycles;
        }
        self.cpu.memory.vdc.step_scanline()
    }

    /// Run a whole frame (until the VDC wraps to scanline 0).
    pub fn run_frame(&mut self) {
        for _ in 0..SCANLINES_PER_FRAME {
            if self.step_scanline() {
                break;
            }
        }
    }

    /// Single-step the CPU (advancing the timer, but not the VDC), returning the
    /// `(program_counter, opcode)` that was about to execute. Intended for
    /// instruction tracing while debugging.
    pub fn debug_step(&mut self) -> (u16, u8) {
        let pc = self.cpu.registers.program_counter;
        let opcode = self.cpu.memory.peek(pc);
        let cycles = self.step_instruction();
        self.cpu.memory.timer.step(cycles);
        // Keep the VDC roughly in step so vblank/raster IRQs still occur.
        self.scanline_accumulator += cycles;
        while self.scanline_accumulator >= CPU_CYCLES_PER_SCANLINE {
            self.scanline_accumulator -= CPU_CYCLES_PER_SCANLINE;
            self.cpu.memory.vdc.step_scanline();
        }
        (pc, opcode)
    }

    /// Update the controller state (call once per frame from your input layer).
    pub fn set_pad(&mut self, pad: PadState) {
        self.cpu.memory.io.set_pad(pad);
    }

    /// Convert the VDC's palette-index framebuffer into a packed `0xAARRGGBB`
    /// image through the VCE palette. The returned buffer is [`FB_WIDTH`] ×
    /// [`FB_HEIGHT`].
    #[must_use]
    pub fn render_argb(&self) -> Vec<u32> {
        let vdc = &self.cpu.memory.vdc;
        let vce = &self.cpu.memory.vce;
        vdc.framebuffer
            .iter()
            .map(|&index| vce.color_argb(index as usize))
            .collect()
    }

    /// Borrow the CPU (registers, cycle count, etc.) for debugging.
    #[must_use]
    pub const fn cpu(&self) -> &CPU<SystemBus, Huc6280> {
        &self.cpu
    }

    /// Borrow the system bus (VDC, VCE, RAM, ...) for debugging.
    #[must_use]
    pub const fn bus(&self) -> &SystemBus {
        &self.cpu.memory
    }

    /// Mutably borrow the system bus.
    pub fn bus_mut(&mut self) -> &mut SystemBus {
        &mut self.cpu.memory
    }

    /// Framebuffer dimensions, for convenience.
    #[must_use]
    pub const fn framebuffer_size(&self) -> (usize, usize) {
        (FB_WIDTH, FB_HEIGHT)
    }
}
