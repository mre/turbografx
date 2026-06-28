//! HuC6280 instruction timing.
//!
//! The `mos6502` core counts cycles with the base 65C02/6502 timings, but the
//! HuC6280 is slower: most memory accesses cost an extra cycle or two, and a
//! taken branch costs a flat `+2` (rather than the 6502's `+1`, `+1` on page
//! cross). Those differences are small per instruction but they accumulate, and
//! the console's programmable timer ticks every 1024 *CPU cycles* — so getting
//! the per-instruction cost wrong drifts every timer-driven effect.
//!
//! Several PC Engine titles (e.g. Bravoman) drive their title-screen
//! background DMA from the timer interrupt; with 6502 timings the timer fires at
//! the wrong moment and the tilemap is assembled incorrectly. We therefore pace
//! the timer and VDC from these hardware-accurate counts instead of the core's.
//!
//! Values are the documented HuC6280 base cycle counts (one entry per opcode);
//! page-cross and branch-taken penalties are applied on top (see
//! [`branch_len`] and [`BRANCH_TAKEN_PENALTY`]). Block-transfer opcodes
//! (TII/TDD/TIN/TIA/TAI) are encoded as `0` here because their cost is
//! data-dependent; for those we defer to the core's own cycle count.

/// Base cycle cost per opcode on the HuC6280 (`0` = data-dependent, defer to
/// the core — currently only the block-transfer instructions).
pub const BASE_CYCLES: [u8; 256] = [
    // 0x00
    8, 7, 3, 5, 6, 4, 6, 7, 3, 2, 2, 2, 7, 5, 7, 6, // 0x10
    2, 7, 7, 5, 6, 4, 6, 7, 2, 5, 2, 2, 7, 5, 7, 6, // 0x20
    7, 7, 3, 5, 4, 4, 6, 7, 4, 2, 2, 2, 5, 5, 7, 6, // 0x30
    2, 7, 7, 2, 4, 4, 6, 7, 2, 5, 2, 2, 5, 5, 7, 6, // 0x40
    7, 7, 3, 4, 8, 4, 6, 7, 3, 2, 2, 2, 4, 5, 7, 6, // 0x50
    2, 7, 7, 5, 3, 4, 6, 7, 2, 5, 3, 2, 2, 5, 7, 6, // 0x60
    7, 7, 2, 2, 4, 4, 6, 7, 4, 2, 2, 2, 7, 5, 7, 6, // 0x70
    2, 7, 7, 0, 4, 4, 6, 7, 2, 5, 4, 2, 7, 5, 7, 6, // 0x80
    2, 7, 2, 7, 4, 4, 4, 7, 2, 2, 2, 2, 5, 5, 5, 6, // 0x90
    2, 7, 7, 8, 4, 4, 4, 7, 2, 5, 2, 2, 5, 5, 5, 6, // 0xA0
    2, 7, 2, 7, 4, 4, 4, 7, 2, 2, 2, 2, 5, 5, 5, 6, // 0xB0
    2, 7, 7, 8, 4, 4, 4, 7, 2, 5, 2, 2, 5, 5, 5, 6, // 0xC0
    2, 7, 2, 0, 4, 4, 6, 7, 2, 2, 2, 2, 5, 5, 7, 6, // 0xD0
    2, 7, 7, 0, 3, 4, 6, 7, 2, 5, 3, 2, 2, 5, 7, 6, // 0xE0
    2, 7, 2, 0, 4, 4, 6, 7, 2, 2, 2, 2, 5, 5, 7, 6, // 0xF0
    2, 7, 7, 0, 2, 4, 6, 7, 2, 5, 4, 2, 2, 5, 7, 6,
];

/// Extra cycles a *taken* branch costs on the HuC6280 (flat, no page penalty).
pub const BRANCH_TAKEN_PENALTY: u64 = 2;

/// Cycles the HuC6280 spends acknowledging a hardware interrupt (TIQ/IRQ1/IRQ2):
/// it pushes PC and P, sets the I flag, and loads the two-byte vector. The
/// `mos6502` core performs this sequence but charges it `0` cycles (its
/// `service_interrupt` never advances the cycle count), and we pace from the
/// per-opcode [`BASE_CYCLES`] rather than the core's count — so the dispatch
/// cost has to be added back explicitly or every serviced IRQ silently drops
/// ~8 cycles, drifting the timer/VDC. Matches Geargrafx's `HandleIRQ` (`+8`).
pub const IRQ_DISPATCH_CYCLES: u64 = 8;

/// VDC/timer pacing multiplier for the two HuC6280 clock speeds. The CPU runs
/// at 1.79 MHz (low) or 7.16 MHz (high), selected by `CSL`/`CSH`, but the VDC
/// and programmable timer run off the fixed master clock. Geargrafx encodes
/// this as a master-cycle divisor of `{12 (low), 3 (high)}`; expressed in
/// high-speed-CPU-cycle units (master / 3) that is a factor of `4` in low speed
/// and `1` in high speed. So a given instruction advances the picture/timer 4x
/// further per CPU cycle while the CPU is in low speed. The HuC6280 powers up in
/// low speed, so [`LOW_SPEED_FACTOR`] is the reset default.
pub const LOW_SPEED_FACTOR: u64 = 4;
pub const HIGH_SPEED_FACTOR: u64 = 1;

/// `CSL` opcode — select the 1.79 MHz (low) CPU clock.
pub const CSL_OPCODE: u8 = 0x54;
/// `CSH` opcode — select the 7.16 MHz (high) CPU clock.
pub const CSH_OPCODE: u8 = 0xD4;

/// CPU↔VRAM data-port transfer delay, in **master clocks**, indexed by the VCE
/// dot-clock select (`0` = 5.37 MHz, `1` = 7.16 MHz, `2` = 10.74 MHz). When the
/// CPU latches a data-port read or write the VDC needs this long to service it
/// before the result is ready; a *following* CPU access that arrives while the
/// previous one is still in flight stalls. Verbatim from Geargrafx's
/// `k_huc6270_vram_read_delay` / `k_huc6270_vram_write_delay`. The per-dot slot
/// model (see [`crate::vdc`]) consumes these directly in master units rather
/// than the old CPU-cycle approximation, so the residual stall now tracks the
/// VDC's sub-line fetch windows instead of a flat back-pressure cost.
pub const VRAM_READ_DELAY_MASTER: [u64; 3] = [24, 24, 15];
pub const VRAM_WRITE_DELAY_MASTER: [u64; 3] = [21, 18, 12];

/// VRAM read/write transfer delay (master clocks) for a VCE dot-clock select.
#[must_use]
pub const fn vram_delays_master(dot_clock_select: u8) -> (u64, u64) {
    let i = if dot_clock_select > 2 {
        2
    } else {
        dot_clock_select as usize
    };
    (VRAM_READ_DELAY_MASTER[i], VRAM_WRITE_DELAY_MASTER[i])
}

/// Master clocks per high-speed CPU cycle (the HuC6280 runs at master / 3).
pub const MASTER_PER_CPU_CYCLE: u64 = 3;

/// VCE horizontal line length in master clocks (Geargrafx `HUC6260_LINE_LENGTH`).
pub const LINE_LENGTH_MASTER: u64 = 1365;

/// If `opcode` is a branch, return its instruction length and the relative
/// offset's byte position so the caller can compute the taken target. Returns
/// `None` for non-branches.
///
/// Covers the PC-relative conditional branches and `BRA`, plus the
/// zero-page-relative `BBR0..7`/`BBS0..7` bit branches.
#[must_use]
pub const fn branch_len(opcode: u8) -> Option<u8> {
    match opcode {
        // BPL BMI BVC BVS BCC BCS BNE BEQ BRA — PC-relative, 2 bytes.
        0x10 | 0x30 | 0x50 | 0x70 | 0x90 | 0xB0 | 0xD0 | 0xF0 | 0x80 => Some(2),
        // BBR0..7 (0x0F..0x7F) and BBS0..7 (0x8F..0xFF) — zero-page-relative, 3 bytes.
        0x0F | 0x1F | 0x2F | 0x3F | 0x4F | 0x5F | 0x6F | 0x7F | 0x8F | 0x9F | 0xAF | 0xBF
        | 0xCF | 0xDF | 0xEF | 0xFF => Some(3),
        _ => None,
    }
}
