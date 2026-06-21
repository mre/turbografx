//! A TurboGrafx-16 / PC Engine emulator core.
//!
//! The console is built around the Hudson Soft / NEC **HuC6280** CPU (a 65C02
//! derivative with an integrated MMU, PSG, timer and I/O port) talking to two
//! video chips:
//!
//! - **HuC6270 VDC** — the Video Display Controller. This is the "graphics
//!   processor": it owns 64 KiB of VRAM and produces the tile/sprite picture.
//!   It lives in [`vdc`] and is the part you'll be fleshing out next.
//! - **HuC6260 VCE** — the Video Color Encoder. It holds the 512-entry color
//!   palette and the dot-clock selection. It lives in [`vce`].
//!
//! ## How the pieces fit together
//!
//! ```text
//!            +-----------------------------------------------+
//!            |                    Console                    |
//!            |                                               |
//!   CPU  <-->|  mos6502::CPU<SystemBus, Huc6280>             |
//!            |        |                                      |
//!            |        v   (Bus trait: get_byte/set_byte)     |
//!            |   SystemBus  -- MMU (MPR0-7) + memory map     |
//!            |     |   |   |        |        |       |       |
//!            |    ROM RAM VDC      VCE     Timer    IRQ ctl  |
//!            +-----------------------------------------------+
//! ```
//!
//! The CPU only ever emits 16-bit *logical* addresses. The [`SystemBus`]
//! performs the HuC6280 MMU translation (logical → 21-bit physical) using a
//! shadow copy of the mapping registers, then dispatches to ROM, work RAM or
//! the hardware page (bank `$FF`).
//!
//! See [`crate::bus`] for the important note on interrupt-vector handling.

pub mod bus;
pub mod cartridge;
pub mod console;
pub mod interrupts;
pub mod io;
pub mod psg;
pub mod timer;
pub mod vce;
pub mod vdc;

pub use bus::SystemBus;
pub use cartridge::Cartridge;
pub use console::Console;

/// CPU clock in its high-speed mode: 21.477 MHz master / 3 ≈ 7.16 MHz.
///
/// `CSL`/`CSH` switch between this and the 1.79 MHz low-speed mode. The
/// `mos6502` core counts cycles per instruction rather than wall-clock time, so
/// the speed switch is currently a no-op (see the TODO in [`crate::bus`]).
pub const CPU_CLOCK_HZ: u32 = 7_159_090;

/// NTSC scanlines per frame (262 active + retrace ≈ 262/263).
pub const SCANLINES_PER_FRAME: u16 = 263;

/// Approximate CPU cycles per scanline at 7.16 MHz.
///
/// Master horizontal total is ~1365 master clocks per line; at master/3 that's
/// ~455 CPU cycles. This is a coarse number used to interleave CPU execution
/// with the VDC's per-scanline stepping until proper dot-clock timing lands.
pub const CPU_CYCLES_PER_SCANLINE: u64 = 455;
