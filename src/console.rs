//! The assembled console: CPU + system bus, with a frame-stepping loop.

use crate::bus::SystemBus;
use crate::cartridge::Cartridge;
use crate::io::PadState;
use crate::vdc::{ACTIVE_HEIGHT, ACTIVE_WIDTH, FB_HEIGHT, FB_WIDTH};
use mos6502::cpu::CPU;
use mos6502::instruction::Huc6280;
use mos6502::memory::Bus;

/// A fully wired TurboGrafx-16.
pub struct Console {
    cpu: CPU<SystemBus, Huc6280>,
    /// VDC/timer pacing cost of the most recent `debug_step`, in
    /// high-speed-CPU-cycle units (clock-speed scaled), for tracing tools.
    last_step_cycles: u64,
    /// VDC/timer pacing multiplier for the current CPU clock speed (see
    /// [`crate::timing::LOW_SPEED_FACTOR`]). `CSL`/`CSH` switch it; the HuC6280
    /// resets into low speed, so this starts at [`crate::timing::LOW_SPEED_FACTOR`].
    speed_factor: u64,
    /// Cycles already fed to the timer/VDC during the instruction currently being
    /// executed (used by byte-stepped HuC6280 block transfers).
    inline_paced_cycles: u64,
}

#[derive(Clone, Copy)]
enum BlockStep {
    Inc,
    Dec,
    Fixed,
    Alternate,
}

impl Console {
    /// Build a console around a loaded HuCard and run the reset sequence.
    #[must_use]
    pub fn new(cartridge: Cartridge) -> Self {
        let bus = SystemBus::new(cartridge);
        let mut console = Self {
            cpu: CPU::new(bus, Huc6280),
            last_step_cycles: 0,
            speed_factor: crate::timing::LOW_SPEED_FACTOR,
            inline_paced_cycles: 0,
        };
        console.reset();
        console
    }

    /// Perform the CPU reset sequence (loads the reset vector).
    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    fn read_operand_word(&self, addr: u16) -> u16 {
        u16::from_le_bytes([
            self.cpu.memory.peek(addr),
            self.cpu.memory.peek(addr.wrapping_add(1)),
        ])
    }

    fn push_stack_byte(&mut self, value: u8) {
        let sp = self.cpu.registers.stack_pointer.0;
        self.cpu.memory.set_byte(0x2100 | u16::from(sp), value);
        self.cpu.registers.stack_pointer.decrement();
    }

    fn pull_stack_byte(&mut self) -> u8 {
        self.cpu.registers.stack_pointer.increment();
        let sp = self.cpu.registers.stack_pointer.0;
        self.cpu.memory.get_byte(0x2100 | u16::from(sp))
    }

    fn advance_block_pointer(addr: u16, step: BlockStep, toggle: &mut bool) -> u16 {
        match step {
            BlockStep::Inc => addr.wrapping_add(1),
            BlockStep::Dec => addr.wrapping_sub(1),
            BlockStep::Fixed => addr,
            BlockStep::Alternate => {
                let next = if *toggle {
                    addr.wrapping_sub(1)
                } else {
                    addr.wrapping_add(1)
                };
                *toggle = !*toggle;
                next
            }
        }
    }

    fn pace_inline(&mut self, cycles: u64) {
        if cycles == 0 {
            return;
        }
        self.cpu.memory.timer.step(cycles);
        self.cpu.memory.cpu_cycle += cycles * crate::timing::MASTER_PER_CPU_CYCLE;
        let dot_clock = self.cpu.memory.vce.dot_clock_select();
        self.cpu
            .memory
            .vdc
            .clock_to(self.cpu.memory.cpu_cycle, dot_clock);
        self.inline_paced_cycles += cycles;
    }

    fn block_transfer_steps(opcode: u8) -> Option<(BlockStep, BlockStep)> {
        match opcode {
            0x73 => Some((BlockStep::Inc, BlockStep::Inc)), // TII
            0xC3 => Some((BlockStep::Dec, BlockStep::Dec)), // TDD
            0xD3 => Some((BlockStep::Inc, BlockStep::Fixed)), // TIN
            0xE3 => Some((BlockStep::Inc, BlockStep::Alternate)), // TIA
            0xF3 => Some((BlockStep::Alternate, BlockStep::Inc)), // TAI
            _ => None,
        }
    }

    fn execute_block_transfer(&mut self, opcode: u8, factor: u64) -> Option<u64> {
        let (src_step, dst_step) = Self::block_transfer_steps(opcode)?;
        let pc = self.cpu.registers.program_counter;
        let mut source = self.read_operand_word(pc.wrapping_add(1));
        let mut dest = self.read_operand_word(pc.wrapping_add(3));
        let length = self.read_operand_word(pc.wrapping_add(5));
        let count = if length == 0 {
            0x1_0000_u32
        } else {
            u32::from(length)
        };

        let saved_y = self.cpu.registers.index_y;
        let saved_a = self.cpu.registers.accumulator;
        let saved_x = self.cpu.registers.index_x;
        self.push_stack_byte(saved_y);
        self.push_stack_byte(saved_a);
        self.push_stack_byte(saved_x);

        self.cpu.memory.video_stall_cycles = 0;
        let mut total_cycles = 17 * factor;
        self.pace_inline(17 * factor);

        let mut src_toggle = false;
        let mut dst_toggle = false;
        for _ in 0..count {
            let value = self.cpu.memory.get_byte(source);
            self.cpu.memory.set_byte(dest, value);
            let stall = self.cpu.memory.video_stall_cycles;
            self.cpu.memory.video_stall_cycles = 0;
            let byte_cycles = 6 * factor + stall;
            total_cycles += byte_cycles;
            self.pace_inline(byte_cycles);
            source = Self::advance_block_pointer(source, src_step, &mut src_toggle);
            dest = Self::advance_block_pointer(dest, dst_step, &mut dst_toggle);
        }

        self.cpu.registers.index_x = self.pull_stack_byte();
        self.cpu.registers.accumulator = self.pull_stack_byte();
        self.cpu.registers.index_y = self.pull_stack_byte();
        self.cpu.registers.program_counter = pc.wrapping_add(7);
        self.cpu.cycles = self.cpu.cycles.wrapping_add(17 + 6 * u64::from(count));

        Some(total_cycles)
    }

    /// Execute one instruction and keep the bus's MMU shadow in sync.
    ///
    /// Returns the number of **HuC6280** cycles the instruction consumed. The
    /// `mos6502` core tracks base 6502/65C02 timings, which run a cycle or two
    /// short of the HuC6280 on most opcodes; we re-derive the hardware-accurate
    /// count (see [`crate::timing`]) so the programmable timer and VDC are paced
    /// correctly. Timer-driven title screens (e.g. Bravoman) depend on this.
    fn step_instruction(&mut self) -> u64 {
        // Snapshot what we need to score the instruction's true cost before it
        // runs: the opcode, and (for a branch) the target it would jump to if
        // taken, so we can detect the taken case afterwards.
        let pc = self.cpu.registers.program_counter;
        let opcode = self.cpu.memory.peek(pc);
        let branch_target = crate::timing::branch_len(opcode).map(|len| {
            let offset = self.cpu.memory.peek(pc.wrapping_add(u16::from(len) - 1)) as i8;
            pc.wrapping_add(u16::from(len))
                .wrapping_add(offset as i16 as u16)
        });

        let before = self.cpu.cycles;
        self.inline_paced_cycles = 0;
        // Clear any leftover stall before the instruction so we attribute video
        // accesses to the instruction that makes them.
        self.cpu.memory.video_stall_cycles = 0;
        // Snapshot the IRQ-dispatch counter: the core may *service* an interrupt
        // at the end of `single_step` (after the instruction runs), but it
        // charges that dispatch `0` cycles. We detect it here and add the
        // hardware cost below so the timer/VDC don't drift by ~8 cycles per IRQ.
        let irq_vectors_before = self.cpu.memory.debug.irq_vectors_fetched;
        if Self::block_transfer_steps(opcode).is_some() {
            return self
                .execute_block_transfer(opcode, self.speed_factor)
                .unwrap_or(1);
        }
        // TAM writes are mirrored to the bus by the core via
        // `Bus::set_mapping_register`, so the MMU shadow is already current.
        //
        // The core advances the cycle count even while halted or waiting
        // (WAI/STP/JAM), so the delta is always positive and the scanline
        // loop keeps making progress. Use `self.cpu.wait_state()` if we ever
        // want to stop the emulation on a stopped CPU.
        self.cpu.single_step();
        let core_cycles = self.cpu.cycles.wrapping_sub(before);
        // Each VDC/VCE access stalls the HuC6280 by one cycle (the CPU waits on
        // the dot clock); the bus tallied them as the instruction ran.
        let video_stall = self.cpu.memory.video_stall_cycles;
        // If the core serviced a maskable interrupt during this step, charge the
        // dispatch sequence the core accounts as free (see
        // [`crate::timing::IRQ_DISPATCH_CYCLES`]).
        let irq_cost = if self.cpu.memory.debug.irq_vectors_fetched != irq_vectors_before {
            crate::timing::IRQ_DISPATCH_CYCLES
        } else {
            0
        };

        // Pace the VDC/timer at the current CPU clock speed. The per-opcode
        // and IRQ/branch costs scale with the clock (4x the master cycles per
        // CPU cycle in low speed), but the VDC-access stall is a fixed master
        // wait that does not. `CSL`/`CSH` change the clock; like Geargrafx
        // (which adds the base cost then multiplies by the *post-switch* speed)
        // the switch applies to the switching instruction itself, so update the
        // factor before scoring.
        if opcode == crate::timing::CSL_OPCODE {
            self.speed_factor = crate::timing::LOW_SPEED_FACTOR;
        } else if opcode == crate::timing::CSH_OPCODE {
            self.speed_factor = crate::timing::HIGH_SPEED_FACTOR;
        }
        let factor = self.speed_factor;

        let base = crate::timing::BASE_CYCLES[opcode as usize];
        if base == 0 {
            // Block-transfer (TII/TDD/TIN/TIA/TAI) or other data-dependent cost:
            // the core's own per-byte accounting is the best estimate we have.
            return (core_cycles.max(1) + irq_cost) * factor + video_stall;
        }
        // A taken branch costs a flat +2 on the HuC6280.
        let branch_penalty = if let Some(target) = branch_target
            && self.cpu.registers.program_counter == target
        {
            crate::timing::BRANCH_TAKEN_PENALTY
        } else {
            0
        };
        (u64::from(base) + irq_cost + branch_penalty) * factor + video_stall
    }

    /// Run one scanline worth of wall-clock time. The VDC itself now owns the
    /// horizontal state machine; this method simply executes CPU instructions until
    /// the clocked raster advances to the next line (or wraps the frame).
    ///
    /// Returns `true` when this was the final scanline of the frame.
    pub fn step_scanline(&mut self) -> bool {
        let start_line = self.cpu.memory.vdc.scanline();
        loop {
            if self.advance_vdc_to_cpu_time(true) {
                return true;
            }
            if self.cpu.memory.vdc.scanline() != start_line {
                return false;
            }
            if self.line_step(true).3 {
                return true;
            }
            if self.cpu.memory.vdc.scanline() != start_line {
                return false;
            }
        }
    }

    /// Execute one instruction, then let the timer and master-clocked VDC catch up
    /// to the CPU timestamp. Returns the `(program_counter, opcode)` that was
    /// executed, its cycle cost, and whether the VDC crossed a frame boundary.
    fn line_step(&mut self, stop_at_frame: bool) -> (u16, u8, u64, bool) {
        let pc = self.cpu.registers.program_counter;
        let opcode = self.cpu.memory.peek(pc);
        let cycles = self.step_instruction();
        self.last_step_cycles = cycles;
        let remaining = cycles.saturating_sub(self.inline_paced_cycles);
        self.cpu.memory.timer.step(remaining);
        self.cpu.memory.cpu_cycle += remaining * crate::timing::MASTER_PER_CPU_CYCLE;
        let frame_wrapped = self.advance_vdc_to_cpu_time(stop_at_frame);
        (pc, opcode, cycles, frame_wrapped)
    }

    /// Advance the VDC/VCE clock loop until it has caught up with the CPU's
    /// monotonic master-clock timestamp.
    fn advance_vdc_to_cpu_time(&mut self, stop_at_frame: bool) -> bool {
        let target = self.cpu.memory.cpu_cycle;
        let dot_clock = self.cpu.memory.vce.dot_clock_select();
        self.cpu
            .memory
            .vdc
            .clock_to_with_frame_limit(target, dot_clock, stop_at_frame)
    }

    /// Run a whole frame (until the VDC wraps back to scanline 0).
    pub fn run_frame(&mut self) {
        // The VDC ends the frame via [`Vdc::advance_scanline`], whose length now
        // comes from the game-programmed vertical-timing registers (typically
        // 264 lines). Cap the loop well above any sane frame so a misprogrammed
        // VDC can't spin forever, but let the wrap be the real terminator.
        for _ in 0..512 {
            if self.step_scanline() {
                break;
            }
        }
    }

    /// Single-step the CPU and advance the timer/VDC to the resulting CPU
    /// timestamp, returning the `(program_counter, opcode)` that was executed.
    pub fn debug_step(&mut self) -> (u16, u8) {
        self.advance_vdc_to_cpu_time(false);
        let (pc, opcode, _cycles, _frame_wrapped) = self.line_step(false);
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

    /// The active display size in pixels, `(width, height)`.
    #[must_use]
    pub const fn active_size(&self) -> (usize, usize) {
        (ACTIVE_WIDTH, ACTIVE_HEIGHT)
    }

    /// Render the active display area as tightly-packed RGBA8 bytes
    /// (`ACTIVE_WIDTH * ACTIVE_HEIGHT * 4`), ready to upload as a texture.
    #[must_use]
    pub fn active_frame_rgba(&self) -> Vec<u8> {
        let vdc = &self.cpu.memory.vdc;
        let vce = &self.cpu.memory.vce;
        let mut out = vec![0u8; ACTIVE_WIDTH * ACTIVE_HEIGHT * 4];
        for y in 0..ACTIVE_HEIGHT {
            for x in 0..ACTIVE_WIDTH {
                let index = vdc.framebuffer[y * FB_WIDTH + x] as usize;
                let argb = vce.color_argb(index);
                let o = (y * ACTIVE_WIDTH + x) * 4;
                out[o] = (argb >> 16) as u8; // R
                out[o + 1] = (argb >> 8) as u8; // G
                out[o + 2] = argb as u8; // B
                out[o + 3] = 0xFF; // A
            }
        }
        out
    }

    /// Borrow the CPU (registers, cycle count, etc.) for debugging.
    #[must_use]
    pub const fn cpu(&self) -> &CPU<SystemBus, Huc6280> {
        &self.cpu
    }

    /// The VDC/timer pacing cost of the most recent [`Console::debug_step`], in
    /// high-speed-CPU-cycle units (scaled by the CPU clock speed).
    #[must_use]
    pub const fn last_step_cycles(&self) -> u64 {
        self.last_step_cycles
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
