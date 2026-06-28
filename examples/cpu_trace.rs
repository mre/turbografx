//! Headless CPU trace in the Geargrafx `gg_trace` format, for differential
//! diffing of the HuC6280 core against a reference emulator.
//!
//! ```text
//! cargo run --release --example cpu_trace -- roms/Game.zip 200000 > ours.txt
//! GG_TRACE=1 ./gg_trace roms/Game.zip 3 > gg.txt   # in the Geargrafx tree
//! diff <(cut -c1-7 gg.txt) <(cut -c1-7 ours.txt)    # find the first divergence
//! ```
//!
//! Each line is `PC A X Y P SP` after the instruction, plus `C` = the
//! VDC/timer pacing cost of that instruction in high-speed-CPU-cycle units (the
//! per-opcode [`turbografx::timing`] cost plus IRQ/branch/video penalties, then
//! scaled by the CPU clock speed — 4x while the CPU is in low speed), `SL` =
//! the physical VDC scanline (`vpos`) the instruction ran on, and `RL` = the
//! VDC content/raster-compare line. `SL` advances every line and wraps once per
//! frame; `RL` is the line the picture is on (it can lag `SL` when a game
//! reprograms the vertical timing), which is what the raster compare matches.
use std::io::Write;
use turbografx::{Cartridge, Console};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: cpu_trace <rom> [steps]");
    let steps: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let cart = Cartridge::from_path(&path).expect("load ROM");
    let mut console = Console::new(cart);

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for _ in 0..steps {
        let sl = console.bus().vdc.scanline();
        let rl = console.bus().vdc.raster_line();
        let (pc, _op) = console.debug_step();
        let cyc = console.last_step_cycles();
        let r = &console.cpu().registers;
        let _ = writeln!(
            out,
            "PC:{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} C:{cyc} SL:{sl} RL:{rl}",
            pc,
            r.accumulator,
            r.index_x,
            r.index_y,
            r.status.bits(),
            r.stack_pointer.0,
        );
    }
    let _ = out.flush();
}
