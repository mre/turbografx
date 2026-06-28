//! The HuC6280 system bus: MMU translation + the PC Engine memory map.
//!
//! The `mos6502` core drives memory through the [`Bus`] trait, always emitting
//! 16-bit *logical* addresses. This type performs the HuC6280 MMU translation
//! (logical → 21-bit physical) using a shadow copy of the mapping registers,
//! then routes the physical access to ROM, work RAM, or the bank-`$FF` hardware
//! page.
//!
//! # Keeping the MMU shadow in sync
//!
//! The mapping registers (MPR0-MPR7) physically live in the CPU
//! (`cpu.registers.mpr`) and are changed by the `TAM`/`TMA` instructions, which
//! the core executes internally. The core mirrors every `TAM` write to the bus
//! through [`Bus::set_mapping_register`], so our shadow stays current
//! automatically with no per-step copy. See [`crate::console`].
//!
//! # Interrupt vectors
//!
//! The `mos6502` core emits the HuC6280 vector addresses directly: RESET from
//! `$FFFE` (`Variant::reset_vector()`), `BRK` from `$FFF6`, and the maskable IRQ
//! from whatever [`SystemBus::irq_vector`] returns. We point the IRQ vector at
//! the highest-priority pending source (`TIQ` > `IRQ1` > `IRQ2`). Every vector
//! fetch still passes through the MMU, matching the hardware rule that vectors
//! come from whichever bank is mapped to `$E000-$FFFF` (MPR7).

use crate::cartridge::Cartridge;
use crate::interrupts::{IRQ1, IRQ2, InterruptController, TIQ};
use crate::io::IoPort;
use crate::psg::Psg;
use crate::timer::Timer;
use crate::vce::Vce;
use crate::vdc::Vdc;
use mos6502::memory::Bus;

/// Physical bank that holds the 8 KiB of work RAM.
const RAM_BANK: u8 = 0xF8;
/// The hardware-register page.
const HARDWARE_BANK: u8 = 0xFF;
/// Highest HuCard ROM bank.
const ROM_BANK_MAX: u8 = 0x7F;

/// 8 KiB of work RAM.
const RAM_SIZE: usize = 0x2000;

/// VDC data-port register selectors that touch VRAM (matching the address-latch
/// values in [`crate::vdc`]). Used to decide which data-port writes arm a queued
/// VRAM read/write for the per-dot slot-contention model.
const VDC_REG_MARR: u8 = 0x01;
const VDC_REG_VRR_VWR: u8 = 0x02;

pub struct SystemBus {
    /// Shadow of the CPU's MPR0-MPR7 mapping registers, mirrored from the CPU
    /// via [`Bus::set_mapping_register`] as `TAM` writes happen.
    mpr: [u8; 8],
    ram: [u8; RAM_SIZE],
    cartridge: Cartridge,

    pub vdc: Vdc,
    pub vce: Vce,
    pub psg: Psg,
    pub timer: Timer,
    pub io: IoPort,
    pub interrupts: InterruptController,

    /// Accumulated HuC6280 stall cycles from accessing the video chips: every
    /// VDC ($0000-$03FF) or VCE ($0400-$07FF) access costs the CPU one extra
    /// cycle while it waits for the dot clock. The console drains this after
    /// each instruction to keep the timer/VDC pacing hardware-accurate.
    pub video_stall_cycles: u64,

    /// Throughput back-pressure for the shared CPU/VDC VRAM bus. The CPU and the
    /// VDC contend for VRAM, so once the CPU latches a data-port access the VDC
    /// holds the bus for a transfer delay and then until a free dot *slot*
    /// arrives (see [`crate::vdc::Vdc::vram_reserve`]); a CPU access issued
    /// before the bus frees waits. [`SystemBus::cpu_cycle`] is a monotonic
    /// **master-clock** timestamp (21.48 MHz; the HuC6280 runs at master / 3)
    /// the console advances after each instruction; the VDC owns the
    /// bus-free timestamp and the per-line slot geometry. Modelling this keeps
    /// timer/VDC pacing correct through heavy VRAM-DMA loops (e.g. Bravoman's
    /// title-screen background draw), where the stall swings with the VDC's
    /// sub-line fetch windows.
    pub cpu_cycle: u64,

    /// Debug counters (not hardware state). Handy while bringing the system up.
    pub debug: BusDebug,
}

/// Lightweight instrumentation for diagnosing boot/interrupt issues.
#[derive(Clone, Copy, Debug, Default)]
pub struct BusDebug {
    /// Number of times the IRQ vector was fetched (≈ interrupts serviced).
    pub irq_vectors_fetched: u64,
    /// Number of reads of the VDC status register (clears vblank/IRQ).
    pub vdc_status_reads: u64,
    /// The vector address of the most recent IRQ dispatch (`$FFFA` = TIQ,
    /// `$FFF8` = IRQ1/VDC, `$FFF6` = IRQ2/BRK). Lets a tracer attribute each
    /// serviced interrupt to a source.
    pub last_irq_vector: u16,
    /// Number of writes to the VCE (palette / color ports).
    pub vce_writes: u64,
    /// VCE writes broken down by port offset (0..=7).
    pub vce_offset_writes: [u64; 8],
    /// VCE data-low writes ($0404) with a non-zero value.
    pub vce_data_lo_nonzero: u64,
    /// Inclusive-exclusive work-RAM (bank $F8) offset window to count writes to,
    /// regardless of the logical address used. Set by a debugger.
    pub ram_watch_lo: usize,
    pub ram_watch_hi: usize,
    /// Number of writes seen inside `[ram_watch_lo, ram_watch_hi)`.
    pub ram_watch_writes: u64,
}

impl SystemBus {
    #[must_use]
    pub fn new(cartridge: Cartridge) -> Self {
        Self {
            mpr: [0; 8],
            ram: [0; RAM_SIZE],
            cartridge,
            vdc: Vdc::new(),
            vce: Vce::new(),
            psg: Psg::new(),
            timer: Timer::new(),
            io: IoPort::new(),
            interrupts: InterruptController::new(),
            video_stall_cycles: 0,
            cpu_cycle: 0,
            debug: BusDebug::default(),
        }
    }

    /// Read a byte of work RAM directly, bypassing the MMU. Intended for
    /// debugging and tests.
    #[must_use]
    pub const fn ram_byte(&self, offset: u16) -> u8 {
        self.ram[(offset as usize) & (RAM_SIZE - 1)]
    }

    /// Peek a logical address without side effects, for debugging/disassembly.
    ///
    /// Translates through the MMU and reads ROM/RAM, but returns `0xFF` for the
    /// hardware page rather than touching device registers (so it never clears
    /// status flags or advances ports).
    #[must_use]
    pub fn peek(&self, logical: u16) -> u8 {
        let phys = self.physical_address(logical);
        let bank = (phys >> 13) as u8;
        match bank {
            HARDWARE_BANK => 0xFF,
            RAM_BANK..=0xFB => self.ram[(phys as usize) & (RAM_SIZE - 1)],
            0x00..=ROM_BANK_MAX => self.cartridge.read(phys),
            _ => 0xFF,
        }
    }

    /// Whether a logical address currently maps to nothing real (open bus).
    /// Executing here means the program counter has run off into the weeds.
    #[must_use]
    pub fn is_unmapped(&self, logical: u16) -> bool {
        let bank = (self.physical_address(logical) >> 13) as u8;
        !matches!(bank, HARDWARE_BANK | RAM_BANK..=0xFB | 0x00..=ROM_BANK_MAX)
    }

    /// Translate a 16-bit logical address to a 21-bit physical address via the
    /// MMU, exactly as the HuC6280 hardware does.
    #[must_use]
    pub fn physical_address(&self, logical: u16) -> u32 {
        let bank = self.mpr[(logical >> 13) as usize];
        (u32::from(bank) << 13) | (u32::from(logical) & 0x1FFF)
    }

    /// The set of currently-raised interrupt lines (before masking), as the
    /// [`crate::interrupts`] bit flags.
    fn irq_sources(&self) -> u8 {
        let mut sources = 0;
        if self.vdc.irq() {
            sources |= IRQ1;
        }
        if self.timer.irq() {
            sources |= TIQ;
        }
        // IRQ2 is external (CD-ROM / expansion); none in the base console.
        sources
    }

    /// HuC6280 IRQ vector for the highest-priority pending source.
    /// Priority follows the hardware: TIQ > IRQ1 > IRQ2.
    fn active_irq_vector(&self) -> u16 {
        let active = self.interrupts.active(self.irq_sources());
        if active & TIQ != 0 {
            0xFFFA
        } else if active & IRQ1 != 0 {
            0xFFF8
        } else {
            // IRQ2 (and the fallback) share the $FFF6 vector with BRK.
            0xFFF6
        }
    }

    /// Stall (in high-speed CPU cycles) the CPU pays draining the previously
    /// latched VRAM access before a new data-port touch, delegating the per-dot
    /// slot decision to the VDC (Geargrafx's `WaitForVramAccess`). The VDC
    /// returns the wait in master clocks; the CPU runs at master / 3.
    fn vram_wait(&mut self) -> u64 {
        let stall_master = self.vdc.vram_wait(self.cpu_cycle);
        stall_master.div_ceil(crate::timing::MASTER_PER_CPU_CYCLE)
    }

    /// Read a physical address after MMU translation and vector remapping.
    fn read_physical(&mut self, phys: u32) -> u8 {
        let bank = (phys >> 13) as u8;
        match bank {
            HARDWARE_BANK => self.read_hardware((phys & 0x1FFF) as u16),
            RAM_BANK..=0xFB => self.ram[(phys as usize) & (RAM_SIZE - 1)],
            0x00..=ROM_BANK_MAX => self.cartridge.read(phys),
            // Unmapped physical space reads back as open bus.
            _ => 0xFF,
        }
    }

    fn write_physical(&mut self, phys: u32, value: u8) {
        let bank = (phys >> 13) as u8;
        match bank {
            HARDWARE_BANK => self.write_hardware((phys & 0x1FFF) as u16, value),
            RAM_BANK..=0xFB => {
                // Debug: count writes to the watched physical work-RAM window,
                // regardless of which logical address / MPR mapping was used.
                if bank == RAM_BANK {
                    let off = (phys as usize) & (RAM_SIZE - 1);
                    if (self.debug.ram_watch_lo..self.debug.ram_watch_hi).contains(&off) {
                        self.debug.ram_watch_writes += 1;
                    }
                }
                self.ram[(phys as usize) & (RAM_SIZE - 1)] = value;
            }
            // ROM ignores writes, except cards with a mapper (e.g. SF2) that
            // latch a bank-select from the write address.
            0x00..=ROM_BANK_MAX => self.cartridge.write(phys, value),
            // Unmapped space ignores writes.
            _ => {}
        }
    }

    /// Dispatch a read within the bank-`$FF` hardware page. `offset` is
    /// `phys & 0x1FFF` (0..=0x1FFF).
    fn read_hardware(&mut self, offset: u16) -> u8 {
        match offset {
            0x0000..=0x03FF => {
                self.video_stall_cycles += 1;
                let reg = offset & 0x03;
                // A data-port access to a VRAM register (MAWR/MARR/VRR) shares
                // the VRAM bus with the VDC's fetches. The CPU first drains any
                // access still pending from a previous touch (it stalls here if
                // the VDC has not yet found a free slot), then — on reading the
                // VRR high byte — arms the next read.
                let is_data = (reg == 0x02 || reg == 0x03) && self.vdc.selected_is_vram();
                if is_data {
                    self.video_stall_cycles += self.vram_wait();
                }
                if reg == 0 {
                    self.debug.vdc_status_reads += 1;
                }
                let value = if reg == 0 {
                    self.vdc.read_status(self.cpu_cycle)
                } else {
                    self.vdc.read(reg)
                };
                if reg == 0x03 && self.vdc.selected_reg() == VDC_REG_VRR_VWR {
                    self.vdc.vram_queue(self.cpu_cycle, false);
                }
                value
            }
            0x0400..=0x07FF => {
                self.video_stall_cycles += 1;
                self.vce.read(offset & 0x07)
            }
            0x0800..=0x0BFF => self.psg.read(offset & 0x0F),
            0x0C00..=0x0FFF => self.timer.read(offset & 0x01),
            0x1000..=0x13FF => self.io.read(),
            0x1400..=0x17FF => self.interrupts.read(offset & 0x03, self.irq_sources()),
            _ => 0xFF,
        }
    }

    /// Dispatch a write within the bank-`$FF` hardware page.
    fn write_hardware(&mut self, offset: u16, value: u8) {
        match offset {
            0x0000..=0x03FF => {
                self.video_stall_cycles += 1;
                let reg = offset & 0x03;
                // Writing the data port to a VRAM register (MAWR/MARR/VWR) shares
                // the VRAM bus with the VDC's fetches. The CPU first drains any
                // access still pending from a previous touch (it stalls here if
                // the VDC has not yet found a free slot). A VWR high-byte write
                // commits the word and arms the next write; an MARR high-byte
                // write arms a read.
                let is_data = (reg == 0x02 || reg == 0x03) && self.vdc.selected_is_vram();
                if is_data {
                    self.video_stall_cycles += self.vram_wait();
                }
                let selected = self.vdc.selected_reg();
                self.vdc.write(reg, value);
                if reg == 0x03 {
                    if selected == VDC_REG_VRR_VWR {
                        self.vdc.vram_queue(self.cpu_cycle, true);
                    } else if selected == VDC_REG_MARR {
                        self.vdc.vram_queue(self.cpu_cycle, false);
                    }
                }
            }
            0x0400..=0x07FF => {
                self.video_stall_cycles += 1;
                self.debug.vce_writes += 1;
                self.debug.vce_offset_writes[(offset & 0x07) as usize] += 1;
                if offset & 0x07 == 0x04 && value != 0 {
                    self.debug.vce_data_lo_nonzero += 1;
                }
                self.vce.write(offset & 0x07, value);
            }
            0x0800..=0x0BFF => self.psg.write(offset & 0x0F, value),
            0x0C00..=0x0FFF => self.timer.write(offset & 0x01, value),
            0x1000..=0x13FF => self.io.write(value),
            0x1400..=0x17FF
                // A write to $1403 acknowledges (clears) the timer interrupt.
                if self.interrupts.write(offset & 0x03, value) => {
                    self.timer.acknowledge();
                }
            _ => {}
        }
    }
}

impl Bus for SystemBus {
    fn get_byte(&mut self, address: u16) -> u8 {
        let phys = self.physical_address(address);
        self.read_physical(phys)
    }

    fn set_byte(&mut self, address: u16, value: u8) {
        let phys = self.physical_address(address);
        self.write_physical(phys, value);
    }

    fn irq_pending(&mut self) -> bool {
        self.interrupts.pending(self.irq_sources())
    }

    fn irq_vector(&mut self) -> u16 {
        // The core asks which vector to use for the pending maskable IRQ; steer
        // it to the highest-priority HuC6280 source (TIQ > IRQ1 > IRQ2).
        self.debug.irq_vectors_fetched += 1;
        let vector = self.active_irq_vector();
        self.debug.last_irq_vector = vector;
        vector
    }

    fn set_mapping_register(&mut self, index: usize, value: u8) {
        // The core pushes every TAM write here, so the MMU shadow stays current.
        self.mpr[index] = value;
    }
}

// `IRQ2` is part of the public interrupt model but the base console never
// raises it; reference it so the import doesn't warn until CD support lands.
const _: u8 = IRQ2;
