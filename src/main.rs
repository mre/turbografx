//! Minimal headless front-end for the TurboGrafx-16 core.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -- path/to/game.zip
//! cargo run --release -- path/to/game.pce
//! ```
//!
//! ROM paths may be raw images (`.pce`/`.bin`) or `.zip` archives, which are
//! unpacked on the fly. With no ROM argument it runs a tiny built-in program so
//! you can sanity-check the CPU/bus/VDC wiring without a real image. There is no
//! window yet — drive [`turbografx::Console::render_argb`] into your windowing
//! library of choice (e.g. `minifb`, `pixels`, or `sdl2`) when you're ready.

use std::process::ExitCode;

use turbografx::Cartridge;
use turbografx::Console;

fn main() -> ExitCode {
    let rom_path = std::env::args().nth(1);

    let cartridge = match &rom_path {
        Some(path) => match Cartridge::from_path(path) {
            Ok(cart) => {
                println!("Loaded HuCard: {path} ({} banks)", cart.bank_count());
                cart
            }
            Err(err) => {
                eprintln!("Failed to load {path}: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            println!("No ROM provided; running the built-in smoke-test program.");
            Cartridge::from_bytes(smoke_test_rom())
        }
    };

    let mut console = Console::new(cartridge);
    println!(
        "reset: PC={:#06x}  (running 60 frames)",
        console.cpu().registers.program_counter
    );

    // TRACE=1 single-steps and stops the moment the CPU is about to execute from
    // unmapped memory (i.e. it ran off the rails), dumping the preceding
    // instructions so the offending opcode can be identified.
    if std::env::var("TRACE").is_ok() {
        return trace_until_derail(&mut console);
    }

    // Run ~1 second of frames and report progress so we can see the CPU and the
    // VDC's vblank cycle ticking over.
    for frame in 0..60 {
        console.run_frame();
        if frame % 10 == 0 || frame == 59 {
            let cpu = console.cpu();
            let dbg = console.bus().debug;
            println!(
                "frame {frame:>2}: PC={:#06x} A={:#04x} X={:#04x} Y={:#04x} cycles={} vdc_status={:#04x} irqs={} vdc_status_reads={}",
                cpu.registers.program_counter,
                cpu.registers.accumulator,
                cpu.registers.index_x,
                cpu.registers.index_y,
                cpu.cycles,
                console.bus().vdc.status(),
                dbg.irq_vectors_fetched,
                dbg.vdc_status_reads,
            );
        }
    }

    // Dump the neighbourhood of the final PC plus the MMU state, so a hang in a
    // wait loop can be inspected.
    let pc = console.cpu().registers.program_counter;
    print!("bytes @ {:#06x}:", pc.wrapping_sub(4));
    for i in 0..20u16 {
        let addr = pc.wrapping_sub(4).wrapping_add(i);
        print!(" {:02x}", console.bus().peek(addr));
    }
    println!();
    println!("MPR = {:02x?}", console.cpu().registers.mpr);
    println!("status flags = {:?}", console.cpu().registers.status);

    ExitCode::SUCCESS
}

/// Single-step until the CPU is about to execute from unmapped memory, then dump
/// the last instructions leading up to the derailment.
fn trace_until_derail(console: &mut Console) -> ExitCode {
    use std::collections::VecDeque;

    let mut history: VecDeque<(u16, u8)> = VecDeque::with_capacity(32);
    for _ in 0..20_000_000u64 {
        let pc = console.cpu().registers.program_counter;
        if console.bus().is_unmapped(pc) {
            println!("\nCPU about to execute from UNMAPPED address {pc:#06x}.");
            println!("MPR = {:02x?}", console.cpu().registers.mpr);
            println!("last {} instructions:", history.len());
            for (tpc, op) in &history {
                let o1 = console.bus().peek(tpc.wrapping_add(1));
                let o2 = console.bus().peek(tpc.wrapping_add(2));
                println!("  {tpc:#06x}: {op:02x} {o1:02x} {o2:02x}");
            }
            return ExitCode::SUCCESS;
        }
        let (tpc, op) = console.debug_step();
        if history.len() == 32 {
            history.pop_front();
        }
        history.push_back((tpc, op));
    }
    println!(
        "\nNo derailment within the instruction budget; PC={:#06x}",
        console.cpu().registers.program_counter
    );
    ExitCode::SUCCESS
}

/// A tiny HuCard that boots, sets up the MMU, and spins.
///
/// The reset vector (logical `$FFFE`, physical bank 0 `$1FFE`) points at a
/// routine that maps RAM/hardware into the logical space and then loops. This
/// exercises the reset-vector remap, the MMU shadow sync, and basic execution.
fn smoke_test_rom() -> Vec<u8> {
    // One 8 KiB bank is enough.
    let mut rom = vec![0u8; 0x2000];

    // Program at physical $0000 (logical $E000 once MPR7 = $00, which it is at
    // reset). Keep it trivial: set up a couple of mapping registers and loop.
    let program: &[u8] = &[
        0xA9, 0xFF, // LDA #$FF      ; hardware bank
        0x53, 0x01, // TAM #$01      ; map bank $FF -> MPR0 (logical $0000)
        0xA9, 0xF8, // LDA #$F8      ; work RAM bank
        0x53, 0x02, // TAM #$02      ; map bank $F8 -> MPR1 (logical $2000: zp/stack)
        0xA9, 0x42, // LDA #$42
        0x85, 0x00, // STA $00       ; write to zero page (-> RAM)
        0x4C, 0x0C, 0xE0, // JMP $E00C  ; spin in place on this instruction
    ];
    rom[0x0000..program.len()].copy_from_slice(program);

    // Reset vector at $1FFE/$1FFF -> $E000 (logical start with MPR7 = 0).
    rom[0x1FFE] = 0x00;
    rom[0x1FFF] = 0xE0;

    rom
}
