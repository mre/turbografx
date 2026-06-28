//! HuC6270 VDC — Video Display Controller (hardware page `$FF`, offset
//! `$0000..=$03FF`).
//!
//! This is the **graphics processor**. It owns 64 KiB (32K words) of VRAM and
//! generates the background (tilemap) and sprite layers. The CPU talks to it
//! through four byte-wide ports; internally it has twenty 16-bit registers
//! selected by an address latch.
//!
//! ## What's implemented
//!
//! - The port interface: register select, status read, and the VRAM data port
//!   with auto-increment.
//! - The raster state machine ([`Vdc::step_scanline`]): scanline counter, vblank
//!   and raster-compare (RCR) interrupts, driving IRQ1.
//! - Background rendering: BAT fetch, 4-bitplane 8x8 tiles, horizontal/vertical
//!   scroll (BXR/BYR) and the MWR virtual-screen sizes.
//! - Sprite rendering: 64 sprites from the internal SATB, 16x16 cells up to
//!   32x64, X/Y flip, palette, background priority and basic sprite-0 collision.
//! - SATB DMA (`DVSSR`) and VRAM↔VRAM DMA (`SOUR`/`DESR`/`LENR` + `DCR`).
//!
//! ## Output
//!
//! The framebuffer holds VCE palette *indices* (0..511), exactly what the chip
//! feeds the VCE. Index 0 is the backdrop. The console turns these into ARGB
//! through the VCE palette (see [`crate::console::Console::render_argb`]).
//!
//! ## Known simplifications
//!
//! - The active area is fixed at 256x239; the precise `HSR`/`HDR`/`VPR`/`VDW`
//!   timing registers are not decoded for geometry yet.
//! - Per-scanline rendering reads BXR live and uses a reloadable BYR counter, so
//!   most raster split / parallax effects work, but exact mid-line timing does
//!   not.

use alloc::vec;
use alloc::vec::Vec;
use core::mem;

/// VRAM size in 16-bit words (64 KiB).
pub const VRAM_WORDS: usize = 0x8000;

/// Maximum framebuffer dimensions we allocate for.
pub const FB_WIDTH: usize = 512;
pub const FB_HEIGHT: usize = 242;

/// Active display width in pixels (fixed for now; most NTSC games use 256).
pub const ACTIVE_WIDTH: usize = 256;
/// Active display height in pixels (lines `0..ACTIVE_HEIGHT` are drawn).
pub const ACTIVE_HEIGHT: usize = 239;

/// First scanline of vertical blanking.
pub const VBLANK_LINE: u16 = ACTIVE_HEIGHT as u16;

/// Physical scanlines of top blanking before the first displayable screen row.
/// The VDC's vertical-display-start (VDS) region is programmed so its active
/// (VDW) window opens at this physical line; screen row 0 = physical line
/// `TOP_BLANKING`. Matches Geargrafx's `HUC6270_LINES_TOP_BLANKING`.
const TOP_BLANKING: u16 = 14;

/// Last physical scanline (exclusive) that maps onto a screen row. Physical
/// lines `TOP_BLANKING..BOTTOM_BLANKING` are the visible window.
const BOTTOM_BLANKING: u16 = 256;

/// Physical scanline at which the VCE forces the VDC into vertical sync,
/// anchoring the vertical state machine to the raster. Matches Geargrafx's
/// `total_lines - 4`.
const VSYNC_LINE: u16 = crate::SCANLINES_PER_FRAME - 4;

// --- Internal register indices (selected by the address latch) ---------------
const REG_MAWR: u8 = 0x00; // Memory Address Write
const REG_MARR: u8 = 0x01; // Memory Address Read
const REG_VRR_VWR: u8 = 0x02; // VRAM data (read = VRR, write = VWR)
const REG_CR: u8 = 0x05; // Control
const REG_RCR: u8 = 0x06; // Raster Compare
const REG_BXR: u8 = 0x07; // Background X scroll
const REG_BYR: u8 = 0x08; // Background Y scroll
const REG_MWR: u8 = 0x09; // Memory-access Width (virtual screen size)
const REG_HSR: u8 = 0x0A; // Horizontal sync: HSW (bits 0-4) + HDS (bits 8-14)
const REG_HDR: u8 = 0x0B; // Horizontal display: HDW (bits 0-6) + HDE (bits 8-14)
const REG_VPR: u8 = 0x0C; // Vertical sync: VSW (bits 0-4) + VDS (bits 8-15)
const REG_VDW: u8 = 0x0D; // Vertical display width (active lines - 1, 9 bits)
const REG_VCR: u8 = 0x0E; // Vertical display end (bottom blanking, 8 bits)
const REG_DCR: u8 = 0x0F; // DMA Control
const REG_SOUR: u8 = 0x10; // VRAM-VRAM DMA source
const REG_DESR: u8 = 0x11; // VRAM-VRAM DMA destination
const REG_LENR: u8 = 0x12; // VRAM-VRAM DMA length (write triggers)
const REG_DVSSR: u8 = 0x13; // SATB DMA source (write triggers)

// --- Status register bits (read at port offset 0) ----------------------------
const ST_COLLISION: u8 = 1 << 0; // CR: sprite #0 collision
const ST_OVERFLOW: u8 = 1 << 1; // OR: too many sprites on a line
const ST_RASTER: u8 = 1 << 2; // RR: RCR scanline match
const ST_SATB_DONE: u8 = 1 << 3; // DS: SATB DMA complete
const ST_DMA_DONE: u8 = 1 << 4; // DV: VRAM-VRAM DMA complete
const ST_VBLANK: u8 = 1 << 5; // VD: vertical blank
const ST_BUSY: u8 = 1 << 6; // BSY: pending CPU VRAM access not yet serviced

// --- Control register (CR) bits ----------------------------------------------
const CR_COLLISION_IRQ: u16 = 1 << 0;
const CR_OVERFLOW_IRQ: u16 = 1 << 1;
const CR_RASTER_IRQ: u16 = 1 << 2;
const CR_VBLANK_IRQ: u16 = 1 << 3;
const CR_SPRITE_ENABLE: u16 = 1 << 6; // SB
const CR_BG_ENABLE: u16 = 1 << 7; // BB

// --- DMA control (DCR) bits --------------------------------------------------
const DCR_SATB_IRQ: u16 = 1 << 0; // SATB DMA completion IRQ enable
const DCR_DMA_IRQ: u16 = 1 << 1; // VRAM-VRAM DMA completion IRQ enable
const DCR_SOUR_DEC: u16 = 1 << 2; // source decrements
const DCR_DESR_DEC: u16 = 1 << 3; // destination decrements
const DCR_SATB_AUTO: u16 = 1 << 4; // repeat SATB DMA every vblank

/// Maximum sprites that can be displayed on one scanline before overflow.
const SPRITES_PER_LINE: usize = 16;

/// Sprite cell geometry indexed by the CGY (height) / CGX (width) attribute
/// fields, matching Geargrafx's `k_huc6270_sprite_height` / `_width`. Used to
/// count how many sprite cells fetch on a line, which sizes the per-sprite VRAM
/// slot window in the contention model.
const SPRITE_CELL_HEIGHT: [i32; 4] = [16, 32, 64, 64];
const SPRITE_CELL_WIDTH: [i32; 2] = [16, 32];

/// Per-dot CPU↔VRAM **slot-contention** timing, ported from Geargrafx's
/// HuC6270 (`IsCpuVramSlotAvailable`, `ProcessCpuVramAccesses`).
///
/// The CPU and the VDC share the single VRAM bus. When the CPU latches a
/// data-port access the VDC needs a fixed transfer delay
/// ([`crate::timing::VRAM_READ_DELAY_MASTER`] / `_WRITE_`) to service it, after
/// which it must wait for a free *slot* — a dot the VDC is not itself using for
/// a background or sprite fetch. Whether a slot is free swings with the VDC's
/// sub-line dot position (`hpos`, in master clocks), so two issues of the same
/// instruction can cost wildly different amounts depending on where in the line
/// they land. This is the effect a per-scanline VDC cannot reproduce.
///
/// All times are **master clocks** (21.48 MHz; the HuC6280 runs at master / 3).
/// The line is [`crate::timing::LINE_LENGTH_MASTER`] (1365) master wide. The
/// per-line geometry (`load_bg_*`, sprite count, HSW guard) is recomputed once
/// per display line by [`Vdc::begin_slot_line`]; within a line the slot test is
/// a pure function of `hpos`.
#[derive(Clone)]
struct VramSlotTiming {
    /// Master clocks per dot (VCE divider: 4 / 3 / 2 for 5.37 / 7.16 / 10.74 MHz).
    divider: i32,
    /// Master timestamp at which the current display line began. `hpos` for a
    /// given master time `t` is `(t - line_start) % 1365`.
    line_start: u64,
    /// A CPU VRAM access latched at the data port but not yet serviced by the
    /// VDC. The CPU only stalls on it when it next touches the data port (or an
    /// address register), mirroring Geargrafx's `WaitForVramAccess`.
    pending: bool,
    /// Master timestamp the pending access completes (transfer delay + the wait
    /// for a free dot slot).
    pending_ready_at: u64,
    /// Transfer delays (master clocks) for the current dot clock.
    read_delay: u64,
    write_delay: u64,
    // --- line context, recomputed by `begin_line` ---------------------------
    /// Whether the line is in the active (`Vdw`) vertical phase.
    in_vdw: bool,
    /// Burst mode (both BG and sprites disabled): the VDC fetches nothing, so
    /// the CPU never contends for fetch slots (only the dot-parity rule holds).
    burst: bool,
    /// Physical raster position of the line (for the `[14,256)` active window).
    vpos: i32,
    /// Latched CR (sprite-enable bit) and MWR (screen size / sprite mode).
    cr: u16,
    mwr: u16,
    /// Sprite cells fetched on this line; sizes the per-sprite slot window.
    sprite_count: i32,
    /// Background-fetch window for the line, in master clocks within the line.
    load_bg_start: i32,
    load_bg_end: i32,
    /// Master clock the horizontal sync window ends at (the forced HSW region at
    /// the start of the line); within the first 8 dots of it no slot is free.
    hsw_end: i32,
}

impl VramSlotTiming {
    const fn new() -> Self {
        Self {
            divider: 4,
            line_start: 0,
            pending: false,
            pending_ready_at: 0,
            read_delay: crate::timing::VRAM_READ_DELAY_MASTER[0],
            write_delay: crate::timing::VRAM_WRITE_DELAY_MASTER[0],
            in_vdw: false,
            burst: false,
            vpos: 0,
            cr: 0,
            mwr: 0,
            sprite_count: 0,
            load_bg_start: i32::MAX,
            load_bg_end: 0,
            hsw_end: 0,
        }
    }

    /// Whether `hclock` (master, within the line) falls in the background fetch
    /// window. Mirrors Geargrafx's `IsInBgFetchWindow`.
    fn is_in_bg_fetch(&self, hclock: i32) -> bool {
        !self.burst
            && hclock >= self.load_bg_start
            && hclock < self.load_bg_end
            && self.vpos >= 14
            && self.vpos < 256
    }

    /// Whether the background fetch leaves this dot free for a CPU access.
    /// Mirrors Geargrafx's `IsCpuVramBgSlotAllowed`.
    fn bg_slot_allowed(&self, hclock: i32) -> bool {
        let bg_clock = hclock - self.load_bg_start;
        if bg_clock < 0 {
            return false;
        }
        let bg_dot = (bg_clock / self.divider) - 1;
        if bg_dot < 0 {
            return false;
        }
        match self.mwr & 0x03 {
            0 => (bg_dot & 0x01) == 0,
            1 | 2 => ((bg_dot & 0x07) == 2) || ((bg_dot & 0x07) == 3),
            _ => false,
        }
    }

    /// Port of Geargrafx's `IsCpuVramSlotAvailable` for an access landing at
    /// `hclock` (master clocks within the line). The SAT/VRAM-DMA-pending guard
    /// and the transfer-delay countdown are handled by the caller; this is the
    /// per-dot slot decision.
    fn slot_available(&self, hclock: i32) -> bool {
        let d = self.divider;
        let in_bg = self.is_in_bg_fetch(hclock);
        let sprites_enabled = (self.cr & 0x0040) != 0;

        let access_blocked = if !self.in_vdw
            || self.burst
            || ((!sprites_enabled || self.sprite_count == 0) && !in_bg && self.in_vdw)
        {
            ((hclock / d) & 0x01) != 0
        } else {
            let allow = in_bg && self.bg_slot_allowed(hclock);
            let mut blocked = in_bg && !allow;
            if !blocked && !in_bg && sprites_enabled {
                let clock_count = if hclock > self.load_bg_end {
                    hclock - self.load_bg_end
                } else {
                    crate::timing::LINE_LENGTH_MASTER as i32 - self.load_bg_end + hclock
                };
                let dot_count = clock_count / d;
                let dots_per_sprite = match (self.mwr >> 2) & 0x03 {
                    2 => 8,
                    3 => 16,
                    _ => 4,
                };
                if dot_count < self.sprite_count * dots_per_sprite {
                    blocked = true;
                } else {
                    blocked = ((hclock / d) & 0x01) != 0;
                }
            }
            blocked
        };

        if access_blocked {
            return false;
        }

        // HSW guard: within the first 8 dots of horizontal sync no slot is free.
        // (`hsync_start` is 0 within the line, so clocks-since-hsync is hclock+3.)
        if hclock < self.hsw_end && (hclock + 3) < 8 * d {
            return false;
        }
        true
    }

    /// Master time at which an access issued at `begin` completes: the transfer
    /// delay, then forward to the next free slot (3-master-clock granularity, as
    /// in `ProcessCpuVramAccesses`).
    fn completion(&self, begin: u64, is_write: bool) -> u64 {
        let delay = if is_write {
            self.write_delay
        } else {
            self.read_delay
        };
        let line_len = crate::timing::LINE_LENGTH_MASTER;
        let mut t = begin + delay;
        // Cap the search at ~one line of dots so a pathological window can't spin.
        for _ in 0..512 {
            let hclock = ((t - self.line_start) % line_len) as i32;
            if self.slot_available(hclock) {
                break;
            }
            t += 3;
        }
        t + 3
    }

    /// Whether a previously-latched CPU VRAM access is still waiting for its slot
    /// at master time `now`; this drives status bit 6 (`BSY`).
    fn busy(&self, now: u64) -> bool {
        self.pending && now < self.pending_ready_at
    }

    /// Drain any access still pending from a previous data-port touch, returning
    /// the stall (master clocks) the CPU pays waiting for it to complete. This is
    /// Geargrafx's `WaitForVramAccess`: the CPU blocks here, on the *next* port
    /// access, until the previous transfer has found its VRAM slot.
    fn wait(&mut self, now: u64) -> u64 {
        if !self.pending {
            return 0;
        }
        self.pending = false;
        self.pending_ready_at.saturating_sub(now)
    }

    /// Latch a new CPU VRAM access at master time `now` (the data-port commit).
    /// Its completion time is computed once, here, from the per-dot slot model;
    /// a later [`VramSlotTiming::wait`] pays whatever of it has not yet elapsed.
    fn queue(&mut self, now: u64, is_write: bool) {
        self.pending = true;
        self.pending_ready_at = self.completion(now, is_write);
    }
}

/// A transient VDC event captured while tracing is enabled (see
/// [`Vdc::set_trace`]). These are the events a boot/IRQ tracer cares about:
/// the program arming a raster split, and the VDC actually raising the
/// raster-compare and vertical-blank interrupts.
#[derive(Clone, Copy, Debug)]
pub struct VdcEvent {
    /// Scanline the event happened on.
    pub scanline: u16,
    pub kind: VdcEventKind,
}

/// The kind of a captured [`VdcEvent`].
#[derive(Clone, Copy, Debug)]
pub enum VdcEventKind {
    /// The raster-compare register (RCR) was set to a new value.
    RcrWrite(u16),
    /// The control register (CR) was set to a new value.
    CrWrite(u16),
    /// The line counter matched RCR, so the RR status bit was set. `irq` is
    /// whether the raster-compare IRQ was enabled (and thus IRQ1 was raised).
    RasterMatch { rcr: u16, irq: bool },
    /// Vertical blank began, so the VD status bit was set. `irq` is whether the
    /// vblank IRQ was enabled (and thus IRQ1 was raised).
    Vblank { irq: bool },
}

/// The VDC's vertical state machine phase. Each frame cycles
/// `Vds -> Vdw -> Vcr -> Vsw`, with each phase lasting a
/// register-derived number of lines (re-latched at the `Vsw` boundary). The
/// `Vdw` (vertical display width) phase is the active region that produces
/// picture; the others are blanking/sync. Modelling this explicitly — rather
/// than collapsing the frame so scanline 0 is the first active line — keeps the
/// content line counter (`raster_line`) in phase with the CPU through games
/// that reprogram the vertical timing mid-frame (e.g. Bravoman's title).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VState {
    /// Vertical display start (top blanking).
    Vds,
    /// Vertical display width (active picture).
    Vdw,
    /// Vertical display end (bottom blanking).
    Vcr,
    /// Vertical sync.
    Vsw,
}

/// Horizontal display phase for the current VDC line. The VCE owns the fixed
/// 1365-master-clock line cadence; the VDC decodes that into the familiar HSW,
/// HDS, HDW and HDE regions from HSR/HDR.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HState {
    /// Horizontal sync/forced blanking at the start of the line.
    Hsw,
    /// Horizontal display start / left border.
    Hds,
    /// Horizontal display width / active fetch.
    Hdw,
    /// Horizontal display end / right border.
    Hde,
}

/// Sub-line event scheduled by `HSyncStart`/`HDWStart`, ported from Geargrafx's
/// `HuC6270_Line_Event` chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineEvent {
    Byr,
    Bxr,
    Hds,
    Rcr,
}

#[derive(Clone)]
pub struct Vdc {
    vram: Vec<u16>,
    /// Internal Sprite Attribute Table (64 sprites x 4 words), filled by DMA.
    satb: [u16; 256],
    /// The twenty internal 16-bit registers, indexed by selector.
    registers: [u16; 0x20],
    /// Currently selected register (set by writing port offset 0).
    selected: u8,
    /// Status flags (the readable status byte, minus the busy bit).
    status: u8,
    /// VRAM read buffer (`VRR`). Writing `MARR` prefetches this; reading the high
    /// data-port byte returns it, increments `MARR`, then prefetches the next word.
    vram_read_buffer: u16,
    /// IRQ1 output line. Set when an enabled VDC interrupt fires; cleared when
    /// the CPU reads the status register.
    irq: bool,
    /// Physical scanline within the frame (`0..SCANLINES_PER_FRAME`). This is
    /// the raster position that drives screen-row placement and the frame wrap;
    /// it advances every line regardless of the vertical state machine, exactly
    /// like Geargrafx's `m_vpos`.
    vpos: u16,
    /// Current phase of the vertical state machine (see [`VState`]).
    v_state: VState,
    /// Current horizontal phase of the master-clocked VDC line.
    h_state: HState,
    /// Absolute master-clock position of the VDC. This is advanced by
    /// [`Vdc::clock_to`] in small event/dot-sized slices instead of by the old
    /// scanline scheduler.
    master_clock: u64,
    /// Absolute master-clock timestamp at which the current line began.
    line_start_master: u64,
    /// Master-clock offset within the current line (`0..LINE_LENGTH_MASTER`).
    hpos_master: u64,
    /// Master-clock offset at which the current horizontal state ends.
    h_state_end_master: u64,
    /// Next scheduled sub-line event and its master-clock offset within the line.
    next_line_event: Option<(LineEvent, u64)>,
    /// Whether the current line has had its forced HSync/start-of-line setup.
    line_started: bool,
    /// Geargrafx increments the raster counter at the RCR event, but still catches
    /// it at end-of-line if malformed horizontal timing skipped that event.
    need_to_increment_raster_line: bool,
    /// Vblank is raised once after leaving VDW; this mirrors Geargrafx's
    /// `m_vblank_triggered` guard.
    vblank_triggered: bool,
    /// Lines remaining in the current vertical phase before it transitions.
    /// Decremented once per line; on reaching zero the next phase is entered
    /// (with its own line count latched from the registers).
    lines_to_next_v_state: i32,
    /// Content line counter: reset to 0 when the `Vdw` (active) phase begins and
    /// incremented every line thereafter. The raster-compare (RCR) interrupt is
    /// evaluated against this, and it indexes the background fetch — so it is
    /// the line the *picture* is on, which can differ from `vpos` when a game
    /// reprograms the vertical timing mid-frame.
    raster_line: u16,
    /// Whether the vertical-blank interrupt is due to fire this line, i.e. the
    /// line is the falling edge of the active (`Vdw`) phase. Computed once per
    /// line in [`Vdc::render_current_line`] from the line-start phase versus the
    /// previous line's, so it triggers exactly once at the end of the active
    /// region.
    vblank_due: bool,
    /// `v_state` captured at the start of the current line, before any mid-line
    /// phase transition. Rendering and the vblank edge use this so they see the
    /// phase the line *began* in (matching where hardware samples them).
    v_state_line_start: VState,
    /// Vertical-timing values latched at the `Vsw` boundary (start of frame),
    /// so mid-frame register writes only take effect next frame — as on
    /// hardware. `(VDS, VDW, VCR, VSW)` raw register fields.
    latched_vds: u16,
    latched_vdw: u16,
    latched_vcr: u16,
    latched_vsw: u16,
    /// Internal vertical-scroll counter (latched from BYR, reloaded on write).
    bg_y: u16,
    /// Whether a BYR write should reload the scroll counter at the next line's
    /// scroll-latch point.
    bg_y_update_pending: bool,
    /// Per-line latched rendering state sampled at the horizontal display setup
    /// point. Mid-line register writes should not retroactively affect pixels that
    /// have already been fetched.
    line_cr: u16,
    line_mwr: u16,
    line_bxr: u16,
    line_bg_y: u16,
    /// A write to DVSSR arms a SATB DMA; hardware performs the copy at vblank
    /// (or every vblank when DCR's auto bit is set), not immediately on the write.
    satb_dma_pending: bool,
    /// Dot countdown for an active SATB DMA. Geargrafx transfers one SAT word
    /// every 4 VDC dots, for 256 words / 1024 dot clocks total.
    satb_dma_countdown: u16,
    /// Dot countdown for an active VRAM→VRAM DMA. One word transfers every 4 VDC
    /// dots; LENR stores remaining words - 1 as the copy progresses.
    vram_dma_countdown: u32,
    vram_dma_src: u16,
    vram_dma_dst: u16,

    /// Palette-index framebuffer (values 0..511, as fed to the VCE).
    pub framebuffer: Vec<u16>,
    /// When set, transient events are appended to `events` for a tracer to
    /// drain. Off by default and zero-cost when disabled.
    trace_enabled: bool,
    /// Captured events, drained via [`Vdc::drain_events`].
    events: Vec<VdcEvent>,
    /// Last RCR/CR values emitted as events, so we only log real changes.
    traced_rcr: u16,
    traced_cr: u16,
    /// Per-dot CPU↔VRAM slot-contention timing (see [`VramSlotTiming`]).
    slot: VramSlotTiming,
}

impl Default for Vdc {
    fn default() -> Self {
        Self::new()
    }
}

impl Vdc {
    #[must_use]
    pub fn new() -> Self {
        let mut vdc = Self {
            vram: vec![0; VRAM_WORDS],
            satb: [0; 256],
            registers: [0; 0x20],
            selected: 0,
            status: 0,
            vram_read_buffer: 0,
            irq: false,
            vpos: 0,
            v_state: VState::Vds,
            h_state: HState::Hsw,
            master_clock: 0,
            line_start_master: 0,
            hpos_master: 0,
            h_state_end_master: 0,
            next_line_event: None,
            line_started: false,
            need_to_increment_raster_line: false,
            vblank_triggered: false,
            lines_to_next_v_state: 0,
            raster_line: 0,
            vblank_due: false,
            v_state_line_start: VState::Vds,
            latched_vds: 0,
            latched_vdw: 0,
            latched_vcr: 0,
            latched_vsw: 0,
            bg_y: 0,
            bg_y_update_pending: false,
            line_cr: 0,
            line_mwr: 0,
            line_bxr: 0,
            line_bg_y: 0,
            satb_dma_pending: false,
            satb_dma_countdown: 0,
            vram_dma_countdown: 0,
            vram_dma_src: 0,
            vram_dma_dst: 0,
            framebuffer: vec![0; FB_WIDTH * FB_HEIGHT],
            trace_enabled: false,
            events: Vec::new(),
            traced_rcr: 0,
            traced_cr: 0,
            slot: VramSlotTiming::new(),
        };
        // Power-on defaults that keep an unprogrammed VDC advancing at a sane
        // NTSC rate. These match the conventional HuC6270 reset state used by
        // established emulators: real horizontal timing, 240 active lines, and a
        // VSW-starting vertical state. Starting in VDS with zeroed VPR produces
        // spurious early vblank edges during boot, which can perturb games that
        // poll VDC status while initializing VRAM.
        vdc.registers[REG_HSR as usize] = 0x0202;
        vdc.registers[REG_HDR as usize] = 0x041F;
        vdc.registers[REG_VPR as usize] = 0x0F02;
        vdc.registers[REG_VDW as usize] = 0x00EF;
        vdc.registers[REG_VCR as usize] = 0x0004;
        vdc.latch_vertical_timing();
        vdc.v_state = VState::Vsw;
        vdc.v_state_line_start = VState::Vsw;
        vdc.lines_to_next_v_state = i32::from(vdc.latched_vsw) + 1;
        // Do not raise a boot-time vblank just because CR is enabled before the
        // first active VDW window. `NextVerticalState(Vdw)` re-arms this for the
        // first real falling edge out of active display.
        vdc.vblank_triggered = true;
        vdc
    }

    /// VRAM address auto-increment, decoded from CR bits 11-12.
    fn address_increment(&self) -> u16 {
        match (self.registers[REG_CR as usize] >> 11) & 0x03 {
            0b00 => 1,
            0b01 => 32,
            0b10 => 64,
            _ => 128,
        }
    }

    /// Read a VDC port. `offset` is the low 2 bits of the bus address.
    pub fn read(&mut self, offset: u16) -> u8 {
        match offset & 0x03 {
            // Status register. Reading it clears the pending interrupt flags and
            // releases the IRQ1 line. Use [`Vdc::read_status`] when a caller can
            // provide the current master timestamp so BSY reflects the CPU↔VRAM
            // slot engine; this legacy path omits transient BSY.
            0x00 => self.read_status(0),
            0x01 => 0xFF,
            // VRAM read data port (VRR). Low byte at offset 2, high at offset 3.
            // Reading the high byte advances MARR by the increment amount.
            0x02 => (self.vram_read_buffer & 0xFF) as u8,
            _ => {
                let value = (self.vram_read_buffer >> 8) as u8;
                let inc = self.address_increment();
                self.registers[REG_MARR as usize] =
                    self.registers[REG_MARR as usize].wrapping_add(inc);
                self.prefetch_vram_read();
                value
            }
        }
    }

    /// Read the VDC status register at master time `now`. Bit 6 (`BSY`) is not a
    /// latched interrupt flag; it reflects whether a CPU VRAM access is still
    /// pending in the per-dot slot engine. Bits 5..0 are cleared by the read.
    pub fn read_status(&mut self, now: u64) -> u8 {
        let busy = if self.slot.busy(now) { ST_BUSY } else { 0 };
        let value = self.status | busy;
        self.status &=
            !(ST_COLLISION | ST_OVERFLOW | ST_RASTER | ST_SATB_DONE | ST_DMA_DONE | ST_VBLANK);
        self.irq = false;
        value
    }

    /// Write a VDC port. `offset` is the low 2 bits of the bus address.
    pub fn write(&mut self, offset: u16, value: u8) {
        match offset & 0x03 {
            // Address/register select (low 5 bits).
            0x00 => self.selected = value & 0x1F,
            0x01 => {}
            // Data port low byte.
            0x02 => self.write_register_low(value),
            // Data port high byte (commits VRAM writes / triggers DMA).
            _ => self.write_register_high(value),
        }
        if self.trace_enabled {
            self.trace_register_write();
        }
    }

    /// Emit an event when a traced register (RCR/CR) changes value, so a tracer
    /// can see exactly when and to what the program arms a raster split or
    /// flips the interrupt-enable / display-enable bits.
    fn trace_register_write(&mut self) {
        match self.selected {
            REG_RCR => {
                let value = self.registers[REG_RCR as usize] & 0x03FF;
                if value != self.traced_rcr {
                    self.traced_rcr = value;
                    self.events.push(VdcEvent {
                        scanline: self.vpos,
                        kind: VdcEventKind::RcrWrite(value),
                    });
                }
            }
            REG_CR => {
                let value = self.registers[REG_CR as usize];
                if value != self.traced_cr {
                    self.traced_cr = value;
                    self.events.push(VdcEvent {
                        scanline: self.vpos,
                        kind: VdcEventKind::CrWrite(value),
                    });
                }
            }
            _ => {}
        }
    }

    fn write_register_low(&mut self, value: u8) {
        let reg = self.selected as usize;
        if reg < self.registers.len() {
            self.registers[reg] = (self.registers[reg] & 0xFF00) | u16::from(value);
        }
        if self.selected == REG_MARR {
            self.prefetch_vram_read();
        }
    }

    fn write_register_high(&mut self, value: u8) {
        let reg = self.selected as usize;
        if reg < self.registers.len() {
            self.registers[reg] = (self.registers[reg] & 0x00FF) | (u16::from(value) << 8);
        }

        match self.selected {
            // Writing the VWR high byte commits the word to VRAM at MAWR and
            // advances MAWR by the increment amount.
            REG_VRR_VWR => {
                let addr = self.registers[REG_MAWR as usize] as usize;
                // The HuC6270 has a 16-bit VRAM address counter but the PC Engine
                // only populates the lower 32K words ($0000..$7FFF). CPU writes to
                // $8000..$FFFF are ignored; they must not mirror into low VRAM.
                if addr < VRAM_WORDS {
                    self.vram[addr] = self.registers[REG_VRR_VWR as usize];
                }
                let inc = self.address_increment();
                self.registers[REG_MAWR as usize] =
                    self.registers[REG_MAWR as usize].wrapping_add(inc);
            }
            // Writing MARR prefetches the VRAM read buffer. Subsequent VRR reads
            // return this latched word until the high byte read advances MARR.
            REG_MARR => self.prefetch_vram_read(),
            // Writing BYR reloads the internal vertical-scroll counter, which is
            // how games do per-line vertical parallax.
            REG_BYR => self.bg_y_update_pending = true,
            // Writing LENR kicks off a VRAM-to-VRAM block copy. The copy runs on
            // VDC dots (one word every four dots), not immediately on the CPU
            // write; games can observe the in-flight DMA through BSY/timing.
            REG_LENR => self.start_vram_dma(),
            // Writing DVSSR arms a SATB copy for the next vblank. Copying
            // immediately sets DS/IRQ too early and can perturb vblank-driven
            // setup code.
            REG_DVSSR => self.satb_dma_pending = true,
            _ => {}
        }
    }

    /// Refresh the VRAM read buffer from the current `MARR` value.
    fn prefetch_vram_read(&mut self) {
        self.vram_read_buffer = self.cpu_vram_read(self.registers[REG_MARR as usize] as usize);
    }

    /// CPU-visible reads from the unpopulated half of the VDC address space return
    /// open-bus-like zero in established emulators. More importantly, writes there
    /// are ignored (see [`Vdc::write_register_high`]), rather than wrapping into
    /// the lower 64 KiB and corrupting real VRAM.
    fn cpu_vram_read(&self, addr: usize) -> u16 {
        if addr < VRAM_WORDS {
            self.vram[addr]
        } else {
            0
        }
    }

    /// Arm the VRAM↔VRAM block transfer described by SOUR/DESR/LENR/DCR. The
    /// actual copy is advanced by [`Vdc::clock_vdc_dot`], matching Geargrafx's
    /// `VRAMTransfer` countdown.
    fn start_vram_dma(&mut self) {
        self.vram_dma_countdown = 4 * (u32::from(self.registers[REG_LENR as usize]) + 1);
        self.vram_dma_src = self.registers[REG_SOUR as usize];
        self.vram_dma_dst = self.registers[REG_DESR as usize];
    }

    /// Advance one word of an active VRAM↔VRAM DMA. Called every fourth VDC dot.
    fn step_vram_dma_word(&mut self) {
        let data = self.cpu_vram_read(self.vram_dma_src as usize);
        if (self.vram_dma_dst as usize) < VRAM_WORDS {
            self.vram[self.vram_dma_dst as usize] = data;
        }

        let dcr = self.registers[REG_DCR as usize];
        let src_step: u16 = if dcr & DCR_SOUR_DEC != 0 { u16::MAX } else { 1 };
        let dst_step: u16 = if dcr & DCR_DESR_DEC != 0 { u16::MAX } else { 1 };
        self.vram_dma_src = self.vram_dma_src.wrapping_add(src_step);
        self.vram_dma_dst = self.vram_dma_dst.wrapping_add(dst_step);
        self.registers[REG_SOUR as usize] = self.vram_dma_src;
        self.registers[REG_DESR as usize] = self.vram_dma_dst;
        self.registers[REG_LENR as usize] = ((self.vram_dma_countdown >> 2) as u16).wrapping_sub(1);

        if self.vram_dma_countdown == 0 && dcr & DCR_DMA_IRQ != 0 {
            self.status |= ST_DMA_DONE;
            self.irq = true;
        }
    }

    /// Advance one word of an active SATB DMA. Called every fourth VDC dot.
    fn step_satb_dma_word(&mut self) {
        let base = self.registers[REG_DVSSR as usize] as usize;
        let index = 255usize.saturating_sub((self.satb_dma_countdown >> 2) as usize);
        self.satb[index] = self.vram[(base + index) % VRAM_WORDS];

        if self.satb_dma_countdown == 0 && self.registers[REG_DCR as usize] & DCR_SATB_IRQ != 0 {
            self.status |= ST_SATB_DONE;
            self.irq = true;
        }
    }

    /// VCE dot-clock divider (master clocks per VDC dot) for a dot-clock select
    /// value: 5.37 MHz -> 4, 7.16 MHz -> 3, 10.74 MHz -> 2.
    const fn dot_clock_divider(dot_clock_select: u8) -> u64 {
        match dot_clock_select {
            0 => 4,
            1 => 3,
            _ => 2,
        }
    }

    /// The forced HSync duration at the start of a VCE line, in VDC dots.
    const fn forced_hsw_dots(divider: u64) -> u64 {
        if divider == 3 { 32 } else { 24 }
    }

    fn event_at_or_after_current(&self, event_dot: u64, divider: u64) -> u64 {
        let event = event_dot.saturating_mul(divider);
        if event <= self.hpos_master {
            (self.hpos_master + 1).min(crate::timing::LINE_LENGTH_MASTER - 1)
        } else {
            event.min(crate::timing::LINE_LENGTH_MASTER - 1)
        }
    }

    /// Start the current line in the forced HSync region and schedule the same
    /// sub-line events Geargrafx derives in `HSyncStart`.
    fn start_line_if_needed(&mut self, dot_clock_select: u8) {
        if self.line_started {
            return;
        }

        self.begin_current_line();
        self.line_started = true;
        self.hpos_master = self.master_clock.saturating_sub(self.line_start_master);
        self.h_state = HState::Hsw;

        let divider = Self::dot_clock_divider(dot_clock_select);
        let hsw_dots = Self::forced_hsw_dots(divider);
        self.h_state_end_master = (hsw_dots * divider).min(crate::timing::LINE_LENGTH_MASTER);
        self.next_line_event = None;

        // Refresh the CPU↔VRAM slot geometry from the line's true HSync anchor.
        self.begin_slot_line(dot_clock_select, self.line_start_master);

        let hds = u64::from((self.registers[REG_HSR as usize] >> 8) & 0x7F);
        let display_start = hsw_dots + (hds + 1) * 8;
        if display_start.saturating_sub(24) * divider >= crate::timing::LINE_LENGTH_MASTER {
            return;
        }

        if self.v_state == VState::Vdw {
            self.next_line_event = Some((
                LineEvent::Byr,
                self.event_at_or_after_current(display_start.saturating_sub(36), divider),
            ));
        } else {
            self.next_line_event = Some((
                LineEvent::Hds,
                self.event_at_or_after_current(display_start.saturating_sub(26), divider),
            ));
        }
    }

    /// Advance VDC-owned DMA countdowns by one VDC dot. SATB DMA has priority over
    /// VRAM↔VRAM DMA, matching Geargrafx's `Clock`.
    fn clock_vdc_dot(&mut self) {
        if self.satb_dma_countdown > 0 {
            self.satb_dma_countdown -= 1;
            if (self.satb_dma_countdown & 0x03) == 0 {
                self.step_satb_dma_word();
            }
        } else if self.vram_dma_countdown > 0 {
            self.vram_dma_countdown -= 1;
            if (self.vram_dma_countdown & 0x03) == 0 {
                self.step_vram_dma_word();
            }
        }
    }

    fn clock_vdc_dots_between(&mut self, old_hpos: u64, new_hpos: u64, divider: u64) {
        let dots = (new_hpos / divider).saturating_sub(old_hpos / divider);
        for _ in 0..dots {
            self.clock_vdc_dot();
        }
    }

    fn latch_scroll_y(&mut self) {
        if self.raster_line == 0 {
            self.bg_y = self.registers[REG_BYR as usize] & 0x01FF;
        } else {
            if self.bg_y_update_pending {
                self.bg_y = self.registers[REG_BYR as usize] & 0x01FF;
                self.bg_y_update_pending = false;
            }
            self.bg_y = self.bg_y.wrapping_add(1) & 0x01FF;
        }
        self.line_bg_y = self.bg_y;
    }

    fn burst_mode(&self) -> bool {
        self.registers[REG_CR as usize] & (CR_BG_ENABLE | CR_SPRITE_ENABLE) == 0
    }

    fn handle_line_event(&mut self, event: LineEvent, divider: u64) {
        match event {
            LineEvent::Byr => {
                self.latch_scroll_y();
                self.next_line_event = Some((
                    LineEvent::Bxr,
                    (self.hpos_master + 2 * divider).min(crate::timing::LINE_LENGTH_MASTER - 1),
                ));
            }
            LineEvent::Bxr => {
                self.line_bxr = self.registers[REG_BXR as usize] & 0x03FF;
                self.next_line_event = Some((
                    LineEvent::Hds,
                    (self.hpos_master + 6 * divider).min(crate::timing::LINE_LENGTH_MASTER - 1),
                ));
            }
            LineEvent::Hds => {
                self.next_line_event = None;
                if self.v_state != VState::Vdw && !self.vblank_triggered {
                    self.vblank_triggered = true;
                    self.raise_vblank_irq();
                }
                if self.status & ST_OVERFLOW != 0
                    && self.registers[REG_CR as usize] & CR_OVERFLOW_IRQ != 0
                {
                    self.irq = true;
                }
            }
            LineEvent::Rcr => {
                self.next_line_event = None;
                self.increment_raster_line();
                if self.v_state == VState::Vdw && !self.burst_mode() {
                    self.slot.sprite_count = self.count_sprites_on_line(self.raster_line);
                }
            }
        }
    }

    fn render_at_hdw_start(&mut self) {
        self.line_cr = self.registers[REG_CR as usize];
        self.line_mwr = self.registers[REG_MWR as usize];
        if self.line_bg_y == 0 && self.raster_line == 0 {
            // If malformed timing skipped the BYR latch, keep the first active line
            // deterministic rather than rendering from stale scroll state.
            self.line_bg_y = self.registers[REG_BYR as usize] & 0x01FF;
        }

        if self.v_state_line_start == VState::Vdw
            && (TOP_BLANKING..BOTTOM_BLANKING).contains(&self.vpos)
        {
            let row = (self.vpos - TOP_BLANKING) as usize;
            if row < FB_HEIGHT {
                self.render_scanline(row);
            }
        }
    }

    fn next_horizontal_state(&mut self, divider: u64) {
        match self.h_state {
            HState::Hsw => {
                self.h_state = HState::Hds;
                let hds = u64::from((self.registers[REG_HSR as usize] >> 8) & 0x7F);
                self.h_state_end_master = (self.h_state_end_master + (hds + 1) * 8 * divider)
                    .min(crate::timing::LINE_LENGTH_MASTER);
            }
            HState::Hds => {
                self.h_state = HState::Hdw;
                let hdw = u64::from(self.registers[REG_HDR as usize] & 0x7F);
                self.h_state_end_master = (self.h_state_end_master + (hdw + 1) * 8 * divider)
                    .min(crate::timing::LINE_LENGTH_MASTER);
                self.need_to_increment_raster_line = true;
                let rcr_offset = hdw.saturating_sub(1) * 8 + 2;
                self.next_line_event = Some((
                    LineEvent::Rcr,
                    (self.hpos_master + rcr_offset * divider)
                        .min(crate::timing::LINE_LENGTH_MASTER - 1),
                ));
                self.render_at_hdw_start();
            }
            HState::Hdw => {
                self.h_state = HState::Hde;
                let hde = u64::from((self.registers[REG_HDR as usize] >> 8) & 0x7F);
                self.h_state_end_master = (self.h_state_end_master + (hde + 1) * 8 * divider)
                    .min(crate::timing::LINE_LENGTH_MASTER);
            }
            HState::Hde => {
                self.h_state = HState::Hsw;
                self.h_state_end_master = crate::timing::LINE_LENGTH_MASTER;
            }
        }
    }

    fn finish_clocked_line(&mut self) -> bool {
        if self.need_to_increment_raster_line {
            self.increment_raster_line();
        }
        self.line_started = false;
        self.next_line_event = None;
        self.line_start_master += crate::timing::LINE_LENGTH_MASTER;
        self.hpos_master = 0;
        self.advance_scanline()
    }

    /// Advance the VDC/VCE raster to an absolute master-clock timestamp. This is
    /// the replacement for the old scanline scheduler: horizontal states, line
    /// events, DMA countdowns and slot geometry all move on the same fixed
    /// 1365-master-clock line timeline.
    pub fn clock_to(&mut self, target_master: u64, dot_clock_select: u8) -> bool {
        self.clock_to_with_frame_limit(target_master, dot_clock_select, false)
    }

    /// Like [`Vdc::clock_to`], but optionally stops exactly after the first frame
    /// wrap so presentation can happen before any CPU overshoot draws into the next
    /// frame. The unconsumed CPU/VDC skew is preserved for the following call.
    pub fn clock_to_with_frame_limit(
        &mut self,
        target_master: u64,
        dot_clock_select: u8,
        stop_at_frame: bool,
    ) -> bool {
        let mut frame_wrapped = false;
        while self.master_clock < target_master {
            self.start_line_if_needed(dot_clock_select);
            let divider = Self::dot_clock_divider(dot_clock_select);
            let line_end_abs = self.line_start_master + crate::timing::LINE_LENGTH_MASTER;
            let mut next = target_master.min(line_end_abs);
            let h_state_abs = self.line_start_master + self.h_state_end_master;
            if h_state_abs > self.master_clock {
                next = next.min(h_state_abs);
            }
            if let Some((_, event_hpos)) = self.next_line_event {
                let event_abs = self.line_start_master + event_hpos;
                if event_abs > self.master_clock {
                    next = next.min(event_abs);
                } else if event_abs == self.master_clock {
                    next = self.master_clock;
                }
            }

            let old_hpos = self.hpos_master;
            if next > self.master_clock {
                self.master_clock = next;
                self.hpos_master = self.master_clock - self.line_start_master;
                self.clock_vdc_dots_between(old_hpos, self.hpos_master, divider);
            }

            if let Some((event, event_hpos)) = self.next_line_event
                && self.hpos_master >= event_hpos
            {
                self.handle_line_event(event, divider);
            }

            while self.hpos_master >= self.h_state_end_master
                && self.h_state_end_master < crate::timing::LINE_LENGTH_MASTER
            {
                self.next_horizontal_state(divider);
            }

            if self.master_clock >= line_end_abs {
                let wrapped = self.finish_clocked_line();
                frame_wrapped |= wrapped;
                if wrapped && stop_at_frame {
                    break;
                }
            }
        }
        frame_wrapped
    }

    /// Advance the VDC by one whole scanline using the master-clocked horizontal
    /// loop. Returns `true` when the frame wraps.
    pub fn step_scanline(&mut self) -> bool {
        let target = self.master_clock + crate::timing::LINE_LENGTH_MASTER;
        self.clock_to(target, 0)
    }

    /// Latch state for the current scanline before rendering it.
    pub fn begin_current_line(&mut self) {
        // Capture the phase the line begins in, before the raster event can step
        // the state machine partway through the line. Rendering and the vblank
        // edge both key off the line-start phase.
        let prev = self.v_state_line_start;
        self.v_state_line_start = self.v_state;
        // Vblank fires on the falling edge of the active phase (an active line
        // followed by a non-active one), which happens exactly once per frame.
        self.vblank_due = prev == VState::Vdw && self.v_state != VState::Vdw;
    }

    /// Render the scanline latched by [`Vdc::begin_current_line`] and advance the
    /// internal vertical-scroll counter for the next line.
    pub fn finish_current_line(&mut self) {
        let active = self.v_state_line_start == VState::Vdw;
        // Latch scroll/rendering registers at the display setup point, not at the
        // forced HSync line start. Games can and do write scroll during HBlank.
        if active {
            if self.raster_line == 0 {
                self.bg_y = self.registers[REG_BYR as usize] & 0x01FF;
                self.bg_y_update_pending = false;
            } else if self.bg_y_update_pending {
                self.bg_y = (self.registers[REG_BYR as usize].wrapping_add(1)) & 0x01FF;
                self.bg_y_update_pending = false;
            }
        }
        self.line_cr = self.registers[REG_CR as usize];
        self.line_mwr = self.registers[REG_MWR as usize];
        self.line_bxr = self.registers[REG_BXR as usize] & 0x03FF;
        self.line_bg_y = self.bg_y;
        // Place the active line at its physical screen row (vpos - top blanking),
        // which is what makes the picture follow `vpos` while its content follows
        // the (separately tracked) scroll/raster counters.
        if active && (TOP_BLANKING..BOTTOM_BLANKING).contains(&self.vpos) {
            let row = (self.vpos - TOP_BLANKING) as usize;
            if row < FB_HEIGHT {
                self.render_scanline(row);
            }
        }
        if active {
            self.bg_y = self.bg_y.wrapping_add(1) & 0x01FF;
        }
    }

    /// Render the current scanline and reload the vertical-scroll counter at the
    /// top of the active region. Picture is produced only while the vertical
    /// state machine is in its active (`Vdw`) phase and the physical raster
    /// position (`vpos`) falls on a visible screen row.
    pub fn render_current_line(&mut self) {
        self.begin_current_line();
        self.finish_current_line();
    }

    /// The raw `(VDS, VDW, VCR, VSW)` vertical-timing register fields.
    fn vertical_fields(&self) -> (u16, u16, u16, u16) {
        let vpr = self.registers[REG_VPR as usize];
        let vsw = vpr & 0x1F;
        let vds = (vpr >> 8) & 0xFF;
        let vdw = self.registers[REG_VDW as usize] & 0x01FF;
        let vcr = self.registers[REG_VCR as usize] & 0xFF;
        (vds, vdw, vcr, vsw)
    }

    /// Latch the vertical-timing fields that govern the phase lengths. Hardware
    /// only re-reads these at the vertical-sync (`Vsw`) boundary, so mid-frame
    /// writes take effect on the next frame.
    fn latch_vertical_timing(&mut self) {
        let (vds, vdw, vcr, vsw) = self.vertical_fields();
        self.latched_vds = vds;
        self.latched_vdw = vdw;
        self.latched_vcr = vcr;
        self.latched_vsw = vsw;
    }

    /// Advance the vertical state machine to its next phase and load that
    /// phase's line count. Mirrors Geargrafx's `NextVerticalState`: the active
    /// (`Vdw`) phase resets the content line counter and re-arms vblank; the
    /// sync (`Vsw`) phase re-latches the vertical-timing registers.
    fn next_vertical_state(&mut self) {
        self.v_state = match self.v_state {
            VState::Vds => VState::Vdw,
            VState::Vdw => VState::Vcr,
            VState::Vcr => VState::Vsw,
            VState::Vsw => VState::Vds,
        };
        match self.v_state {
            VState::Vds => self.lines_to_next_v_state = i32::from(self.latched_vds) + 2,
            VState::Vdw => {
                self.lines_to_next_v_state = i32::from(self.latched_vdw) + 1;
                self.raster_line = 0;
                self.vblank_triggered = false;
            }
            VState::Vcr => self.lines_to_next_v_state = i32::from(self.latched_vcr),
            VState::Vsw => {
                self.lines_to_next_v_state = i32::from(self.latched_vsw) + 1;
                self.latch_vertical_timing();
            }
        }
    }

    /// Enter horizontal blank for the current scanline: raise the raster-compare
    /// interrupt (if this line matches RCR) and, on the first blank line, the
    /// vertical-blank interrupt and any armed SATB DMA.
    ///
    /// This is the combined helper used by the convenience [`Vdc::step_scanline`]
    /// and instruction-stepping tools that don't model sub-scanline timing. The
    /// console instead calls [`Vdc::raster_event`] and [`Vdc::vblank_event`]
    /// separately, at their register-derived horizontal positions within the
    /// line (see [`Vdc::line_irq_timing`]).
    pub fn enter_hblank(&mut self) {
        self.raster_event();
        self.vblank_event();
    }

    /// Evaluate the raster-compare (RCR) interrupt for the current scanline.
    ///
    /// On hardware this fires near the end of the active-display window (the end
    /// of the HDW horizontal state), a little before HBlank proper. The console
    /// calls it at that register-derived offset so the handler's register writes
    /// land on the next line, exactly as on hardware.
    pub fn raster_event(&mut self) {
        self.increment_raster_line();
    }

    fn increment_raster_line(&mut self) {
        // Advance the content line and step the vertical state machine (this is
        // Geargrafx's `IncrementRasterLine`): the content counter ticks every
        // line, a phase boundary may be crossed, and the `Vdw` phase resets the
        // counter to zero. The raster compare is then evaluated against the new
        // content line.
        self.need_to_increment_raster_line = false;
        self.raster_line = self.raster_line.wrapping_add(1);
        self.lines_to_next_v_state -= 1;
        while self.lines_to_next_v_state <= 0 {
            self.next_vertical_state();
        }

        // Raster compare (RCR). The hardware compares the content line counter
        // offset by 64; a match raises RR (and IRQ1 if enabled).
        let rcr = self.registers[REG_RCR as usize] & 0x03FF;
        if rcr >= 64 && self.raster_line == rcr - 64 {
            let irq_enabled = self.registers[REG_CR as usize] & CR_RASTER_IRQ != 0;
            if irq_enabled {
                self.status |= ST_RASTER;
                self.irq = true;
            }
            if self.trace_enabled {
                self.events.push(VdcEvent {
                    scanline: self.vpos,
                    kind: VdcEventKind::RasterMatch {
                        rcr,
                        irq: irq_enabled,
                    },
                });
            }
        }
    }

    /// Evaluate the vertical-blank interrupt for the current scanline.
    ///
    /// Vblank is raised once per frame, on the first line after the active
    /// (`Vdw`) region ends. The console calls this at the line's register-derived
    /// vblank offset; the decision of *whether* to fire was latched at the start
    /// of the line (see [`Vdc::render_current_line`]) so a mid-line phase step
    /// can't disturb it.
    pub fn vblank_event(&mut self) {
        self.vblank_event_at(0);
    }

    /// Timestamped variant of [`Vdc::vblank_event`] used by legacy callers. The
    /// master-clocked horizontal loop raises vblank through [`Vdc::raise_vblank_irq`]
    /// at the HDS event of the first non-VDW line.
    pub fn vblank_event_at(&mut self, _now: u64) {
        // Legacy edge detector used by `enter_hblank`/old tests.
        if self.vblank_due {
            self.vblank_due = false;
            self.raise_vblank_irq();
        }
    }

    fn raise_vblank_irq(&mut self) {
        let irq_enabled = self.registers[REG_CR as usize] & CR_VBLANK_IRQ != 0;
        if irq_enabled {
            self.status |= ST_VBLANK;
            self.irq = true;
        }
        if self.trace_enabled {
            self.events.push(VdcEvent {
                scanline: self.vpos,
                kind: VdcEventKind::Vblank { irq: irq_enabled },
            });
        }
        // Start a pending one-shot SATB DMA, or auto-repeat it every vblank if
        // the game enabled DCR's SATB auto bit. The copy itself is dot-clocked in
        // `clock_vdc_dot`.
        if self.satb_dma_pending || self.registers[REG_DCR as usize] & DCR_SATB_AUTO != 0 {
            self.satb_dma_pending = false;
            self.satb_dma_countdown = 1024;
        }
    }

    /// Master-clock offset at which active display begins on this line.
    #[must_use]
    pub fn line_display_start_master(&self, divider: u64) -> u64 {
        let hsr = self.registers[REG_HSR as usize];
        let hds = u64::from((hsr >> 8) & 0x7F);
        let hsw_dots = if divider == 3 { 32 } else { 24 };
        (hsw_dots + (hds + 1) * 8) * divider
    }

    /// Resolve the within-line master-clock offsets at which the raster-compare
    /// and vertical-blank interrupts are evaluated, from the horizontal-timing
    /// registers (HSR/HDR) and the VCE dot-clock `divider` (master clocks per
    /// dot: 4 / 3 / 2 for the 5.37 / 7.16 / 10.74 MHz dot clocks).
    ///
    /// Returns `(raster_master, vblank_master)` measured from the start of the
    /// line. The horizontal layout per line is, in dots (8 dots per character
    /// cell): `(HDS+1)*8` left blank, `(HDW+1)*8` active, `(HDE+1)*8` right blank,
    /// `(HSW+1)*8` sync. The raster compare fires `(HDW-1)*8+2` dots into the
    /// active window; vblank fires once the active window, right border and
    /// H-sync have elapsed.
    ///
    /// Keeping these offsets in master clocks lets the console run VDC line starts
    /// on the same 1365-master-clock timeline as the per-dot VRAM contention
    /// model, instead of rounding line boundaries to CPU instruction costs.
    /// Falls back to the legacy fixed split while the registers are still
    /// unprogrammed (e.g. during early boot).
    #[must_use]
    pub fn line_irq_timing_master(&self, divider: u64) -> (u64, u64) {
        let hdr = self.registers[REG_HDR as usize];
        let hdw = u64::from(hdr & 0x7F);

        // Only trust register-derived timing once the active window looks real;
        // otherwise keep the legacy fixed point so boot code still advances.
        if !(8..=0x7F).contains(&hdw) {
            let active = crate::ACTIVE_CYCLES_PER_SCANLINE * crate::timing::MASTER_PER_CPU_CYCLE;
            return (active, active);
        }

        // `line_start_master` is the start of the forced HSync region, matching
        // `begin_slot_line`. Include HSW and HDS before scheduling active-display
        // events; otherwise raster/vblank IRQ handlers run tens of CPU cycles too
        // early relative to the per-dot VRAM slot timeline.
        let display_start = self.line_display_start_master(divider) / divider;
        let dot_to_master = |dots: u64| dots * divider;
        let raster = dot_to_master(display_start + hdw.saturating_sub(1) * 8 + 2);
        // On non-active lines Geargrafx schedules the HDS/vblank event shortly
        // before active display would begin (`display_start - 26` dots), not at
        // the end of the would-be display span. This gives vblank handlers the
        // correct budget before the next active frame.
        let vblank = dot_to_master(display_start.saturating_sub(26));

        let cap = crate::timing::LINE_LENGTH_MASTER - 1;
        (raster.min(cap), vblank.min(cap))
    }

    /// Compatibility wrapper returning high-speed CPU-cycle offsets. New console
    /// scheduling uses [`Vdc::line_irq_timing_master`] to avoid losing sub-line
    /// overshoot at scanline boundaries.
    #[must_use]
    pub fn line_irq_timing(&self, divider: u64) -> (u64, u64) {
        let (raster, vblank) = self.line_irq_timing_master(divider);
        let to_cpu = |master: u64| master / crate::timing::MASTER_PER_CPU_CYCLE;
        (to_cpu(raster), to_cpu(vblank))
    }

    /// Force the start of vertical sync: re-latch the vertical-timing registers
    /// and restart the state machine at the `Vsw` phase. On hardware the VCE
    /// drives this at a fixed physical line each frame (see [`VSYNC_LINE`]),
    /// which anchors the VDC's vertical phases to the raster position so the
    /// picture stays put frame to frame instead of rolling.
    fn vertical_sync(&mut self) {
        self.latch_vertical_timing();
        self.v_state = VState::Vsw;
        self.lines_to_next_v_state = i32::from(self.latched_vsw) + 1;
    }

    /// Advance to the next physical scanline. Returns `true` when the frame
    /// wraps (the last line rolled over to 0) so the caller can present a frame.
    /// The frame length is the fixed NTSC total; the vertical state machine runs
    /// on its own register-derived countdowns within it, re-anchored each frame
    /// by the VCE-driven vertical sync.
    pub fn advance_scanline(&mut self) -> bool {
        // The VCE forces vertical sync a few lines before the frame wraps; this
        // pins the vertical state machine to the physical raster.
        if self.vpos == VSYNC_LINE {
            self.vertical_sync();
        }
        self.vpos += 1;
        if self.vpos >= crate::SCANLINES_PER_FRAME {
            self.vpos = 0;
            return true;
        }
        false
    }

    /// Virtual background size in tiles, from MWR bits 4-6.
    fn bat_dimensions(&self) -> (usize, usize) {
        let mwr = self.line_mwr;
        let width = match (mwr >> 4) & 0x03 {
            0 => 32,
            1 => 64,
            _ => 128,
        };
        let height = if mwr & 0x40 != 0 { 64 } else { 32 };
        (width, height)
    }

    /// Latched horizontal display width in pixels for the current line.
    fn line_width_pixels(&self) -> usize {
        let hdw = (self.registers[REG_HDR as usize] & 0x7F) as usize;
        ((hdw + 1) * 8).min(ACTIVE_WIDTH)
    }

    /// Render one active scanline of palette indices into the framebuffer.
    fn render_scanline(&mut self, line: usize) {
        let cr = self.line_cr;
        let width = self.line_width_pixels();
        let mut line_buf = [0u16; ACTIVE_WIDTH];
        let mut bg_opaque = [false; ACTIVE_WIDTH];

        // --- Background layer -------------------------------------------------
        if cr & CR_BG_ENABLE != 0 {
            let (bat_w, bat_h) = self.bat_dimensions();
            let scroll_x = self.line_bxr;
            let bg_y = self.line_bg_y as usize % (bat_h * 8);
            let tile_row = bg_y / 8;
            let fine_y = bg_y % 8;

            for (x, slot) in line_buf.iter_mut().take(width).enumerate() {
                let bg_x = (scroll_x as usize + x) % (bat_w * 8);
                let tile_col = bg_x / 8;
                let fine_x = bg_x % 8;

                let bat = self.vram[(tile_row * bat_w + tile_col) % VRAM_WORDS];
                let char_code = (bat & 0x07FF) as usize;
                let palette = (bat >> 12) & 0x0F;

                let base = char_code * 16;
                let plane01 = self.vram[(base + fine_y) & (VRAM_WORDS - 1)];
                let plane23 = self.vram[(base + fine_y + 8) & (VRAM_WORDS - 1)];
                let bit = 7 - fine_x;
                let color = ((plane01 >> bit) & 1)
                    | (((plane01 >> (bit + 8)) & 1) << 1)
                    | (((plane23 >> bit) & 1) << 2)
                    | (((plane23 >> (bit + 8)) & 1) << 3);

                if color != 0 {
                    *slot = (palette << 4) | color;
                    bg_opaque[x] = true;
                }
            }
        }

        // --- Sprite layer -----------------------------------------------------
        if cr & CR_SPRITE_ENABLE != 0 {
            let (collision, overflow) = self.render_sprites_line(
                self.raster_line as usize,
                width,
                &mut line_buf,
                &bg_opaque,
            );
            if collision && cr & CR_COLLISION_IRQ != 0 {
                self.status |= ST_COLLISION;
                self.irq = true;
            }
            if overflow && cr & CR_OVERFLOW_IRQ != 0 {
                self.status |= ST_OVERFLOW;
                self.irq = true;
            }
        }

        // Commit the assembled line to the framebuffer.
        let start = line * FB_WIDTH;
        self.framebuffer[start..start + ACTIVE_WIDTH].copy_from_slice(&line_buf);
        for slot in &mut self.framebuffer[start + ACTIVE_WIDTH..start + FB_WIDTH] {
            *slot = 0;
        }
    }

    /// Composite sprites for one scanline over the already-drawn background in
    /// `line_buf`. Returns `(sprite0_collision, line_overflow)` so the caller can
    /// update status flags (this borrows `self` immutably).
    fn render_sprites_line(
        &self,
        line: usize,
        display_width: usize,
        line_buf: &mut [u16; ACTIVE_WIDTH],
        bg_opaque: &[bool; ACTIVE_WIDTH],
    ) -> (bool, bool) {
        // First opaque sprite to touch a pixel wins (sprite 0 = highest prio).
        let mut taken = [false; ACTIVE_WIDTH];
        // Track sprite-0 coverage for collision detection.
        let mut sprite0 = [false; ACTIVE_WIDTH];
        let mut on_line = 0usize;
        let mut collision = false;
        let mut overflow = false;

        for index in 0..64 {
            let attr = &self.satb[index * 4..index * 4 + 4];
            let sy = (attr[0] & 0x03FF) as i32 - 64;
            let sx = (attr[1] & 0x03FF) as i32 - 32;
            let pattern = (attr[2] >> 1) & 0x03FF;
            let flags = attr[3];

            let palette = flags & 0x0F;
            let in_front = flags & 0x0080 != 0;
            let x_flip = flags & 0x0800 != 0;
            let y_flip = flags & 0x8000 != 0;
            let cells_x = if flags & 0x0100 != 0 { 2 } else { 1 };
            let cells_y = match (flags >> 12) & 0x03 {
                0 => 1,
                1 => 2,
                _ => 4,
            };

            let height = cells_y * 16;
            let sprite_width = cells_x * 16;
            let ly = line as i32 - sy;
            if ly < 0 || ly >= height as i32 {
                continue;
            }

            let sprite_cells = cells_x as usize;
            if on_line + sprite_cells > SPRITES_PER_LINE {
                overflow = true;
                // Real hardware limits sprite fetches to 16 16-pixel cells per
                // line; a 32-pixel-wide sprite consumes two cells.
                break;
            }
            on_line += sprite_cells;

            // Resolve which 16-pixel-tall cell row this scanline hits.
            let mut row_in_sprite = ly as usize;
            if y_flip {
                row_in_sprite = height - 1 - row_in_sprite;
            }
            let cell_y = row_in_sprite / 16;
            let fine_y = row_in_sprite % 16;

            for col in 0..sprite_width {
                let screen_x = sx + col;
                if screen_x < 0 || screen_x >= display_width as i32 {
                    continue;
                }
                let sxp = screen_x as usize;

                let mut col_in_sprite = col;
                if x_flip {
                    col_in_sprite = sprite_width - 1 - col_in_sprite;
                }
                let cell_x = col_in_sprite / 16;
                let fine_x = col_in_sprite % 16;

                // Multi-cell pattern numbering: horizontal cell -> bit 0,
                // vertical cell -> bits 1.. .
                let mut pat = pattern;
                if cells_x == 2 {
                    pat = (pat & !0x01) | cell_x as u16;
                }
                if cells_y >= 2 {
                    let mask = (cells_y as u16 - 1) << 1;
                    pat = (pat & !mask) | ((cell_y as u16) << 1);
                }

                let base = pat as usize * 64;
                let bit = 15 - fine_x;
                let p0 = (self.vram[(base + fine_y) % VRAM_WORDS] >> bit) & 1;
                let p1 = (self.vram[(base + 16 + fine_y) % VRAM_WORDS] >> bit) & 1;
                let p2 = (self.vram[(base + 32 + fine_y) % VRAM_WORDS] >> bit) & 1;
                let p3 = (self.vram[(base + 48 + fine_y) % VRAM_WORDS] >> bit) & 1;
                let color = p0 | (p1 << 1) | (p2 << 2) | (p3 << 3);
                if color == 0 {
                    continue; // transparent
                }

                // Sprite-0 collision: an opaque sprite-0 pixel overlapping any
                // other opaque sprite pixel sets the flag.
                if index == 0 {
                    sprite0[sxp] = true;
                } else if sprite0[sxp] {
                    collision = true;
                }

                if taken[sxp] {
                    continue; // a higher-priority sprite already drew here
                }
                taken[sxp] = true;

                // Background priority: a low-priority sprite hides behind opaque
                // background pixels.
                if in_front || !bg_opaque[sxp] {
                    line_buf[sxp] = 256 + ((palette << 4) | color);
                }
            }
        }

        (collision, overflow)
    }

    /// Current IRQ1 line state (polled by the bus / interrupt controller).
    #[must_use]
    pub const fn irq(&self) -> bool {
        self.irq
    }

    /// The current physical scanline within the frame (`vpos`, `0..263`). This
    /// advances every line and wraps once per frame, so it is the value to use
    /// for frame accounting. For the *content* line the picture is on (which can
    /// differ under mid-frame reprogramming), see [`Vdc::raster_line`].
    #[must_use]
    pub const fn scanline(&self) -> u16 {
        self.vpos
    }

    /// The content (raster-compare) line counter: 0 at the start of the active
    /// region, incremented every line. This is what RCR compares against and
    /// what indexes the background fetch.
    #[must_use]
    pub const fn raster_line(&self) -> u16 {
        self.raster_line
    }

    /// Read the raw status byte without side effects (for debugging/tests).
    #[must_use]
    pub const fn status(&self) -> u8 {
        self.status
    }

    /// The current Control register (CR) value, for debugging. Bit 7 = BG
    /// enable, bit 6 = sprite enable, bits 0-3 = interrupt enables.
    #[must_use]
    pub const fn control(&self) -> u16 {
        self.registers[REG_CR as usize]
    }

    /// Direct VRAM access for debugging and tests.
    #[must_use]
    pub fn vram(&self) -> &[u16] {
        &self.vram
    }

    /// The raster-compare register (RCR), masked to its valid 10-bit range.
    #[must_use]
    pub const fn rcr(&self) -> u16 {
        self.registers[REG_RCR as usize] & 0x03FF
    }

    /// Read any internal VDC register by index, for debugging.
    #[must_use]
    pub fn register(&self, index: usize) -> u16 {
        self.registers.get(index).copied().unwrap_or(0)
    }

    /// Enable or disable transient event capture (see [`VdcEvent`]). Off by
    /// default; enabling it lets a tracer observe RCR/CR writes and the
    /// raster/vblank interrupts the VDC raises.
    pub fn set_trace(&mut self, on: bool) {
        self.trace_enabled = on;
    }

    /// Take and clear the events captured since the last drain.
    pub fn drain_events(&mut self) -> Vec<VdcEvent> {
        mem::take(&mut self.events)
    }

    /// Whether the currently-selected data-port register targets VRAM (MAWR,
    /// MARR or VWR). A data-port access to one of these goes through the shared
    /// VRAM bus, so it can contend with the VDC's own display fetches.
    #[must_use]
    pub const fn selected_is_vram(&self) -> bool {
        matches!(self.selected, REG_MAWR | REG_MARR | REG_VRR_VWR)
    }

    /// The currently-selected data-port register index (the address latch).
    #[must_use]
    pub const fn selected_reg(&self) -> u8 {
        self.selected
    }

    /// Whether the VDC is currently in its active-display (`Vdw`) phase, where
    /// it fetches background/sprite data from VRAM (and so contends with CPU
    /// VRAM accesses).
    #[must_use]
    pub fn in_active_display(&self) -> bool {
        self.v_state == VState::Vdw
    }

    /// CPU cycles of the active-display (HDW) window on a line, for the given
    /// dot-clock `divider`. This is where the VDC's background fetch happens, so
    /// a CPU VRAM access before this point (on an active line) contends with it.
    /// Falls back to the legacy fixed active span while HDR is unprogrammed.
    #[must_use]
    pub fn active_display_cycles(&self, divider: u64) -> u64 {
        let hdw = u64::from(self.registers[REG_HDR as usize] & 0x7F);
        if !(8..=0x7F).contains(&hdw) {
            return crate::ACTIVE_CYCLES_PER_SCANLINE;
        }
        (hdw + 1) * 8 * divider / 3
    }

    /// Recompute the per-dot VRAM slot-contention context for a display line
    /// that starts at master time `line_start`, for the VCE `dot_clock_select`
    /// (`0`/`1`/`2` -> divider 4/3/2). The console calls this at every line
    /// boundary so that subsequent CPU VRAM data-port accesses (queued through
    /// [`Vdc::vram_queue`]) are gated by the VDC's true background/sprite
    /// fetch windows for the line. See [`VramSlotTiming`].
    pub fn begin_slot_line(&mut self, dot_clock_select: u8, line_start: u64) {
        let d: i32 = match dot_clock_select {
            0 => 4,
            1 => 3,
            _ => 2,
        };
        let (read_delay, write_delay) = crate::timing::vram_delays_master(dot_clock_select);

        let cr = self.registers[REG_CR as usize];
        let in_vdw = self.v_state == VState::Vdw;
        // Burst mode: neither background nor sprites enabled -> the VDC issues no
        // fetches, so the CPU contends only with the dot-parity rule.
        let burst = (cr & (CR_BG_ENABLE | CR_SPRITE_ENABLE)) == 0;

        // Horizontal-sync (HSW) and background-fetch windows, in master clocks,
        // reconstructed from the latched HSR/HDR exactly as Geargrafx's
        // `SetHSyncHigh` + `HSyncStart` do (the line begins forced into HSW).
        let hsw_dots = if d == 3 { 32 } else { 24 };
        let hds = i32::from((self.registers[REG_HSR as usize] >> 8) & 0x7F);
        let hdw = i32::from(self.registers[REG_HDR as usize] & 0x7F);
        let hsw_end = hsw_dots * d;
        // Dot index at which the active display (HDW) begins on the line.
        let display_start = hsw_dots + (hds + 1) * 8;
        let line_len = crate::timing::LINE_LENGTH_MASTER as i32;
        let (load_bg_start, load_bg_end) =
            if in_vdw && (display_start - 24) * d < line_len && (8..=0x7F).contains(&hdw) {
                let start = (display_start - 16) * d;
                let end = (start + ((hdw + 1) * 8 + 16) * d).min(line_len);
                (start, end)
            } else {
                // No background fetch this line (blanking, or HDR not programmed).
                (i32::MAX, 0)
            };

        let sprite_count = self.count_sprites_on_line(self.raster_line);

        let slot = &mut self.slot;
        slot.divider = d;
        slot.line_start = line_start;
        slot.read_delay = read_delay;
        slot.write_delay = write_delay;
        slot.in_vdw = in_vdw;
        slot.burst = burst;
        slot.vpos = i32::from(self.vpos);
        slot.cr = cr;
        slot.mwr = self.registers[REG_MWR as usize];
        slot.sprite_count = sprite_count;
        slot.load_bg_start = load_bg_start;
        slot.load_bg_end = load_bg_end;
        slot.hsw_end = hsw_end;
    }

    /// Count the sprite *cells* that fetch on `raster_line`, replicating
    /// Geargrafx's `FetchSprites` accounting (a 32-pixel-wide sprite fetches two
    /// cells). This sizes the per-sprite VRAM slot window; sprites past the
    /// 16-cell line limit still count toward the window the hardware reserves.
    fn count_sprites_on_line(&self, raster_line: u16) -> i32 {
        let rl = i32::from(raster_line);
        let mut count: i32 = 0;
        for i in 0..64 {
            let sat0 = self.satb[i * 4];
            let sat3 = self.satb[i * 4 + 3];
            let sprite_y = (i32::from(sat0 & 0x03FF)) - 64;
            let cgy = ((sat3 >> 12) & 0x03) as usize;
            let height = SPRITE_CELL_HEIGHT[cgy];
            if sprite_y <= rl && (sprite_y + height) > rl {
                let cgx = ((sat3 >> 8) & 0x01) as usize;
                let cells = if SPRITE_CELL_WIDTH[cgx] == 16 { 1 } else { 2 };
                count += cells;
                if count >= 16 {
                    break;
                }
            }
        }
        count
    }

    /// Drain the previously-latched CPU VRAM access (if any), returning the
    /// stall in **master clocks** the CPU pays for it to complete. The bus calls
    /// this before every CPU data-port access, mirroring Geargrafx's
    /// `WaitForVramAccess`. The wait depends on the VDC's per-dot fetch windows
    /// for the current line (see [`Vdc::begin_slot_line`]).
    pub fn vram_wait(&mut self, now: u64) -> u64 {
        self.slot.wait(now)
    }

    /// Latch a CPU VRAM data-port access at master time `now` (a VWR write / VRR
    /// read commit, or an MARR address write that arms a read). The CPU does not
    /// stall now; it stalls on the next data-port touch via [`Vdc::vram_wait`].
    pub fn vram_queue(&mut self, now: u64, is_write: bool) {
        self.slot.queue(now, is_write);
    }
}

#[cfg(test)]
mod slot_tests {
    use super::VramSlotTiming;

    /// A slot timer for a blanking line (no VDC fetches): availability follows
    /// the dot-parity rule, so even dots are free and odd dots are not.
    fn blanking_line() -> VramSlotTiming {
        let mut s = VramSlotTiming::new();
        s.divider = 4;
        s.in_vdw = false;
        s.burst = false;
        s.load_bg_start = i32::MAX;
        s.load_bg_end = 0;
        s.hsw_end = 24 * 4;
        s
    }

    #[test]
    fn slot_availability_follows_dot_parity_when_idle() {
        let s = blanking_line();
        // Past the HSW guard, even dots (hclock / divider even) are free.
        let d = s.divider;
        let even_dot = 40 * d; // dot 40 -> even
        let odd_dot = 41 * d; // dot 41 -> odd
        assert!(s.slot_available(even_dot));
        assert!(!s.slot_available(odd_dot));
    }

    #[test]
    fn hsync_guard_blocks_first_dots_of_line() {
        let s = blanking_line();
        // Within the first 8 dots of HSW no slot is free, even on an even dot.
        assert!(!s.slot_available(0));
    }

    #[test]
    fn spaced_accesses_do_not_stall_but_tight_ones_do() {
        let mut s = blanking_line();
        // Queue a read at t=100; a follow-up access far in the future has had the
        // transfer complete, so it pays no stall.
        s.queue(100, false);
        assert_eq!(s.wait(100 + 1000), 0);

        // A follow-up that arrives while the transfer is still in flight pays the
        // remaining wait (the read delay is 24 master at divider 4).
        s.queue(100, false);
        let stall = s.wait(100 + 6);
        assert!(
            stall > 0,
            "a tight follow-up access must stall, got {stall}"
        );
    }

    #[test]
    fn background_fetch_window_gates_alternate_dots() {
        // On an active line with a 32-wide BAT (MWR mode 0) the background fetch
        // claims every other dot: even background dots leave a CPU slot free,
        // odd ones do not. Mirrors Geargrafx's `IsCpuVramBgSlotAllowed`.
        let mut active = VramSlotTiming::new();
        active.divider = 4;
        active.in_vdw = true;
        active.vpos = 100;
        active.cr = 0x0080; // BG enabled
        active.mwr = 0;
        active.load_bg_start = 200;
        active.load_bg_end = 1200;
        active.hsw_end = 96;

        // hclock at background dot N is load_bg_start + (N + 1) * divider.
        let d = active.divider;
        let bg_dot = |n: i32| active.load_bg_start + (n + 1) * d;
        assert!(active.is_in_bg_fetch(bg_dot(4)));
        assert!(active.slot_available(bg_dot(4)), "even bg dot is free");
        assert!(!active.slot_available(bg_dot(5)), "odd bg dot is claimed");
    }
}
