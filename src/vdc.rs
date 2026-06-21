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

// --- Internal register indices (selected by the address latch) ---------------
const REG_MAWR: u8 = 0x00; // Memory Address Write
const REG_MARR: u8 = 0x01; // Memory Address Read
const REG_VRR_VWR: u8 = 0x02; // VRAM data (read = VRR, write = VWR)
const REG_CR: u8 = 0x05; // Control
const REG_RCR: u8 = 0x06; // Raster Compare
const REG_BXR: u8 = 0x07; // Background X scroll
const REG_BYR: u8 = 0x08; // Background Y scroll
const REG_MWR: u8 = 0x09; // Memory-access Width (virtual screen size)
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

// --- Control register (CR) bits ----------------------------------------------
const CR_COLLISION_IRQ: u16 = 1 << 0;
const CR_OVERFLOW_IRQ: u16 = 1 << 1;
const CR_RASTER_IRQ: u16 = 1 << 2;
const CR_VBLANK_IRQ: u16 = 1 << 3;
const CR_SPRITE_ENABLE: u16 = 1 << 6; // SB
const CR_BG_ENABLE: u16 = 1 << 7; // BB

// --- DMA control (DCR) bits --------------------------------------------------
const DCR_DMA_IRQ: u16 = 1 << 0; // VRAM-VRAM DMA completion IRQ enable
const DCR_SATB_IRQ: u16 = 1 << 1; // SATB DMA completion IRQ enable
const DCR_SOUR_DEC: u16 = 1 << 2; // source decrements
const DCR_DESR_DEC: u16 = 1 << 3; // destination decrements
const DCR_SATB_AUTO: u16 = 1 << 4; // repeat SATB DMA every vblank

/// Maximum sprites that can be displayed on one scanline before overflow.
const SPRITES_PER_LINE: usize = 16;

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
    /// IRQ1 output line. Set when an enabled VDC interrupt fires; cleared when
    /// the CPU reads the status register.
    irq: bool,
    /// Current scanline of the frame.
    scanline: u16,
    /// Internal vertical-scroll counter (latched from BYR, reloaded on write).
    bg_y: u16,
    /// Palette-index framebuffer (values 0..511, as fed to the VCE).
    pub framebuffer: Vec<u16>,
}

impl Default for Vdc {
    fn default() -> Self {
        Self::new()
    }
}

impl Vdc {
    #[must_use]
    pub fn new() -> Self {
        Self {
            vram: vec![0; VRAM_WORDS],
            satb: [0; 256],
            registers: [0; 0x20],
            selected: 0,
            status: 0,
            irq: false,
            scanline: 0,
            bg_y: 0,
            framebuffer: vec![0; FB_WIDTH * FB_HEIGHT],
        }
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
            // releases the IRQ1 line.
            0x00 => {
                let value = self.status;
                self.status &= !(ST_COLLISION
                    | ST_OVERFLOW
                    | ST_RASTER
                    | ST_SATB_DONE
                    | ST_DMA_DONE
                    | ST_VBLANK);
                self.irq = false;
                value
            }
            0x01 => 0xFF,
            // VRAM read data port (VRR). Low byte at offset 2, high at offset 3.
            // Reading the high byte advances MARR by the increment amount.
            0x02 => {
                let addr = self.registers[REG_MARR as usize] as usize % VRAM_WORDS;
                (self.vram[addr] & 0xFF) as u8
            }
            _ => {
                let addr = self.registers[REG_MARR as usize] as usize % VRAM_WORDS;
                let value = (self.vram[addr] >> 8) as u8;
                let inc = self.address_increment();
                self.registers[REG_MARR as usize] =
                    self.registers[REG_MARR as usize].wrapping_add(inc);
                value
            }
        }
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
    }

    fn write_register_low(&mut self, value: u8) {
        let reg = self.selected as usize;
        if reg < self.registers.len() {
            self.registers[reg] = (self.registers[reg] & 0xFF00) | u16::from(value);
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
                let addr = self.registers[REG_MAWR as usize] as usize % VRAM_WORDS;
                self.vram[addr] = self.registers[REG_VRR_VWR as usize];
                let inc = self.address_increment();
                self.registers[REG_MAWR as usize] =
                    self.registers[REG_MAWR as usize].wrapping_add(inc);
            }
            // Writing BYR reloads the internal vertical-scroll counter, which is
            // how games do per-line vertical parallax.
            REG_BYR => self.bg_y = self.registers[REG_BYR as usize] & 0x01FF,
            // Writing LENR kicks off a VRAM-to-VRAM block copy.
            REG_LENR => self.run_vram_dma(),
            // Writing DVSSR kicks off (or arms) the SATB copy.
            REG_DVSSR => self.run_satb_dma(),
            _ => {}
        }
    }

    /// Perform the VRAM↔VRAM block transfer described by SOUR/DESR/LENR/DCR.
    fn run_vram_dma(&mut self) {
        let dcr = self.registers[REG_DCR as usize];
        let src_step: u16 = if dcr & DCR_SOUR_DEC != 0 { u16::MAX } else { 1 };
        let dst_step: u16 = if dcr & DCR_DESR_DEC != 0 { u16::MAX } else { 1 };
        let mut src = self.registers[REG_SOUR as usize];
        let mut dst = self.registers[REG_DESR as usize];
        // LENR holds (count - 1).
        let count = u32::from(self.registers[REG_LENR as usize]) + 1;
        for _ in 0..count {
            self.vram[dst as usize % VRAM_WORDS] = self.vram[src as usize % VRAM_WORDS];
            src = src.wrapping_add(src_step);
            dst = dst.wrapping_add(dst_step);
        }
        self.registers[REG_SOUR as usize] = src;
        self.registers[REG_DESR as usize] = dst;
        self.status |= ST_DMA_DONE;
        if dcr & DCR_DMA_IRQ != 0 {
            self.irq = true;
        }
    }

    /// Copy the 256-word Sprite Attribute Table from VRAM[DVSSR] into the
    /// internal SATB.
    fn run_satb_dma(&mut self) {
        let base = self.registers[REG_DVSSR as usize] as usize;
        for i in 0..256 {
            self.satb[i] = self.vram[(base + i) % VRAM_WORDS];
        }
        self.status |= ST_SATB_DONE;
        if self.registers[REG_DCR as usize] & DCR_SATB_IRQ != 0 {
            self.irq = true;
        }
    }

    /// Advance the VDC by one scanline, updating raster interrupts and drawing.
    ///
    /// Returns `true` when the frame wraps (the last scanline rolled over to 0)
    /// so the caller can present a frame.
    pub fn step_scanline(&mut self) -> bool {
        // Latch the vertical-scroll counter at the top of the active display.
        if self.scanline == 0 {
            self.bg_y = self.registers[REG_BYR as usize] & 0x01FF;
        }

        // Raster compare (RCR). The hardware compares the line counter offset by
        // 64; a match raises RR (and IRQ1 if enabled).
        let rcr = self.registers[REG_RCR as usize] & 0x03FF;
        if rcr >= 64 && self.scanline == rcr - 64 {
            self.status |= ST_RASTER;
            if self.registers[REG_CR as usize] & CR_RASTER_IRQ != 0 {
                self.irq = true;
            }
        }

        // Render the active display lines.
        if self.scanline < VBLANK_LINE {
            self.render_scanline(self.scanline as usize);
            self.bg_y = self.bg_y.wrapping_add(1) & 0x01FF;
        }

        // Entering vertical blank.
        if self.scanline == VBLANK_LINE {
            self.status |= ST_VBLANK;
            if self.registers[REG_CR as usize] & CR_VBLANK_IRQ != 0 {
                self.irq = true;
            }
            // Auto-repeat SATB DMA if the game armed it.
            if self.registers[REG_DCR as usize] & DCR_SATB_AUTO != 0 {
                self.run_satb_dma();
            }
        }

        self.scanline += 1;
        if self.scanline >= crate::SCANLINES_PER_FRAME {
            self.scanline = 0;
            return true;
        }
        false
    }

    /// Virtual background size in tiles, from MWR bits 4-6.
    fn bat_dimensions(&self) -> (usize, usize) {
        let mwr = self.registers[REG_MWR as usize];
        let width = match (mwr >> 4) & 0x03 {
            0 => 32,
            1 => 64,
            _ => 128,
        };
        let height = if mwr & 0x40 != 0 { 64 } else { 32 };
        (width, height)
    }

    /// Render one active scanline of palette indices into the framebuffer.
    fn render_scanline(&mut self, line: usize) {
        let cr = self.registers[REG_CR as usize];
        let mut line_buf = [0u16; ACTIVE_WIDTH];
        let mut bg_opaque = [false; ACTIVE_WIDTH];

        // --- Background layer -------------------------------------------------
        if cr & CR_BG_ENABLE != 0 {
            let (bat_w, bat_h) = self.bat_dimensions();
            let scroll_x = self.registers[REG_BXR as usize] & 0x03FF;
            let bg_y = self.bg_y as usize % (bat_h * 8);
            let tile_row = bg_y / 8;
            let fine_y = bg_y % 8;

            for (x, slot) in line_buf.iter_mut().enumerate() {
                let bg_x = (scroll_x as usize + x) % (bat_w * 8);
                let tile_col = bg_x / 8;
                let fine_x = bg_x % 8;

                let bat = self.vram[(tile_row * bat_w + tile_col) % VRAM_WORDS];
                let char_code = (bat & 0x07FF) as usize;
                let palette = (bat >> 12) & 0x0F;

                let base = char_code * 16;
                let plane01 = self.vram[(base + fine_y) % VRAM_WORDS];
                let plane23 = self.vram[(base + fine_y + 8) % VRAM_WORDS];
                let bit = 7 - fine_x;
                let color = (((plane01 >> bit) & 1)
                    | (((plane01 >> (bit + 8)) & 1) << 1)
                    | (((plane23 >> bit) & 1) << 2)
                    | (((plane23 >> (bit + 8)) & 1) << 3)) as u16;

                if color != 0 {
                    *slot = (palette << 4) | color;
                    bg_opaque[x] = true;
                }
            }
        }

        // --- Sprite layer -----------------------------------------------------
        if cr & CR_SPRITE_ENABLE != 0 {
            let (collision, overflow) = self.render_sprites_line(line, &mut line_buf, &bg_opaque);
            if collision {
                self.status |= ST_COLLISION;
                if cr & CR_COLLISION_IRQ != 0 {
                    self.irq = true;
                }
            }
            if overflow {
                self.status |= ST_OVERFLOW;
                if cr & CR_OVERFLOW_IRQ != 0 {
                    self.irq = true;
                }
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
                3 => 4,
                _ => 2,
            };

            let height = cells_y * 16;
            let width = cells_x * 16;
            let ly = line as i32 - sy;
            if ly < 0 || ly >= height as i32 {
                continue;
            }

            on_line += 1;
            if on_line > SPRITES_PER_LINE {
                overflow = true;
                // Real hardware stops drawing further sprites this line.
                break;
            }

            // Resolve which 16-pixel-tall cell row this scanline hits.
            let mut row_in_sprite = ly as usize;
            if y_flip {
                row_in_sprite = height - 1 - row_in_sprite;
            }
            let cell_y = row_in_sprite / 16;
            let fine_y = row_in_sprite % 16;

            for col in 0..width {
                let screen_x = sx + col as i32;
                if screen_x < 0 || screen_x >= ACTIVE_WIDTH as i32 {
                    continue;
                }
                let sxp = screen_x as usize;

                let mut col_in_sprite = col;
                if x_flip {
                    col_in_sprite = width - 1 - col_in_sprite;
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

    /// The current scanline within the frame.
    #[must_use]
    pub const fn scanline(&self) -> u16 {
        self.scanline
    }

    /// Read the raw status byte without side effects (for debugging/tests).
    #[must_use]
    pub const fn status(&self) -> u8 {
        self.status
    }

    /// Direct VRAM access for debugging and tests.
    #[must_use]
    pub fn vram(&self) -> &[u16] {
        &self.vram
    }
}
