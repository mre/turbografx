//! Windowed front-end for the TurboGrafx-16 core, built on macroquad.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -- path/to/game.zip      # opens a window
//! cargo run --release -- path/to/game.pce
//! TRACE=1 cargo run --release -- path/to/game.zip   # headless derail tracer
//! ```
//!
//! ROM paths may be raw images (`.pce`/`.bin`) or `.zip` archives, which are
//! unpacked on the fly. With no ROM argument it runs a tiny built-in program.
//!
//! ## Controls
//!
//! | Key            | Pad        |
//! |----------------|------------|
//! | Arrow keys     | D-pad      |
//! | Z              | Button I   |
//! | X              | Button II  |
//! | Enter          | Run        |
//! | Right Shift    | Select     |
//! | Esc            | Quit       |

use macroquad::prelude::*;

// Disassembler-based debugging is disabled until the `mos6502` disassembler is
// released. See https://github.com/mre/mos6502/pull/135
// use mos6502::instruction::{AddressingMode, OpInput};
// use mos6502::instruction::{DisasmInstr, disassemble_one};

use turbografx::Cartridge;
use turbografx::Console;
use turbografx::io::PadState;

/// Integer scale factor for the window (256x239 -> 768x717).
const SCALE: i32 = 3;

fn window_conf() -> Conf {
    Conf {
        window_title: "TurboGrafx-16".to_owned(),
        window_width: 256 * SCALE,
        window_height: 239 * SCALE,
        // Hint the driver to disable vsync so our wall-clock frame limiter is
        // the sole pacing authority (see the main loop). This is only a hint
        // and is ignored on some platforms/macOS Metal, which is why the
        // limiter does the real work.
        platform: miniquad::conf::Platform {
            swap_interval: Some(0),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    let rom_path = std::env::args().nth(1);

    let cartridge = match &rom_path {
        Some(path) => match Cartridge::from_path(path) {
            Ok(cart) => {
                println!("Loaded HuCard: {path} ({} banks)", cart.bank_count());
                cart
            }
            Err(err) => {
                eprintln!("Failed to load {path}: {err}");
                return;
            }
        },
        None => {
            println!("No ROM provided; running the built-in smoke-test program.");
            Cartridge::from_bytes(smoke_test_rom())
        }
    };

    let mut console = Console::new(cartridge);
    println!("reset: PC={:#06x}", console.cpu().registers.program_counter);

    // TRACE=1 runs the headless derailment tracer instead of opening a window.
    if std::env::var("TRACE").is_ok() {
        trace_until_derail(&mut console);
        return;
    }

    // WATCH=1 runs the headless boot-state tracer: it logs RCR/CR writes, the
    // raster/vblank interrupts the VDC raises, every IRQ dispatch, and changes
    // to a set of watched RAM variables. Use it to find which interrupt is
    // supposed to advance the boot state machine and why it isn't firing.
    // Disabled until the mos6502 disassembler is released (see `watch_state`).
    // if std::env::var("WATCH").is_ok() {
    //     watch_state(&mut console);
    //     return;
    // }

    // TRACEDUMP=1 emits one line per executed instruction in the exact format
    // the Geargrafx trace harness produces, for a differential CPU diff from
    // reset. See `trace_dump` for the diffing workflow.
    if std::env::var("TRACEDUMP").is_ok() {
        trace_dump(&mut console);
        return;
    }

    // STATS=1 runs headlessly for a while and reports whether the VDC is drawing
    // (useful on machines without a display).
    if std::env::var("STATS").is_ok() {
        report_stats(&mut console);
        return;
    }

    let (w, h) = console.active_size();
    let mut texture: Option<Texture2D> = None;

    // Pace the emulation to the NTSC PC Engine's ~60 Hz frame rate. macroquad's
    // `next_frame().await` blocks on the display's vsync, so without this the
    // game would run at the monitor's refresh rate (e.g. 2x too fast on a
    // 120 Hz ProMotion panel). We track the wall-clock time each frame is due
    // and sleep off any surplus.
    let frame_period = std::time::Duration::from_secs_f64(1.0 / 60.0);
    let mut next_frame_due = std::time::Instant::now() + frame_period;

    loop {
        if is_key_pressed(KeyCode::Escape) {
            break;
        }

        console.set_pad(read_pad());
        console.run_frame();

        // Upload the freshly rendered frame as a texture and stretch it to the
        // window with nearest-neighbour filtering for crisp pixels.
        let rgba = console.active_frame_rgba();
        let image = Image {
            bytes: rgba,
            width: w as u16,
            height: h as u16,
        };
        match &texture {
            Some(t) => t.update(&image),
            None => {
                let t = Texture2D::from_image(&image);
                t.set_filter(FilterMode::Nearest);
                texture = Some(t);
            }
        }

        clear_background(BLACK);
        if let Some(t) = &texture {
            draw_texture_ex(
                t,
                0.0,
                0.0,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(screen_width(), screen_height())),
                    ..Default::default()
                },
            );
        }

        next_frame().await;

        // Hold the target frame rate. If we've fallen behind (slow host, or the
        // window was stalled), skip the sleep and re-base the schedule so we
        // don't try to "catch up" in a burst.
        let now = std::time::Instant::now();
        if next_frame_due > now {
            std::thread::sleep(next_frame_due - now);
            next_frame_due += frame_period;
        } else {
            next_frame_due = now + frame_period;
        }
    }
}

/// Map the current keyboard state to a controller.
fn read_pad() -> PadState {
    PadState {
        up: is_key_down(KeyCode::Up),
        down: is_key_down(KeyCode::Down),
        left: is_key_down(KeyCode::Left),
        right: is_key_down(KeyCode::Right),
        button_i: is_key_down(KeyCode::Z),
        button_ii: is_key_down(KeyCode::X),
        run: is_key_down(KeyCode::Enter),
        select: is_key_down(KeyCode::RightShift),
    }
}

/// Run headlessly for ~2 seconds and summarise what the VDC produced, so the
/// rendering path can be checked without a window.
fn report_stats(console: &mut Console) {
    for _ in 0..120 {
        console.run_frame();
    }
    let (w, h) = console.active_size();
    let rgba = console.active_frame_rgba();

    let mut colors = std::collections::BTreeSet::new();
    let mut non_backdrop = 0usize;
    let backdrop = (rgba[0], rgba[1], rgba[2]);
    for px in rgba.chunks_exact(4) {
        let c = (px[0], px[1], px[2]);
        colors.insert(c);
        if c != backdrop {
            non_backdrop += 1;
        }
    }
    println!(
        "frame {w}x{h}: {} distinct colours, {non_backdrop}/{} non-backdrop pixels, backdrop=#{:02x}{:02x}{:02x}",
        colors.len(),
        w * h,
        backdrop.0,
        backdrop.1,
        backdrop.2,
    );
}

/// Single-step until the CPU is about to execute from unmapped memory, then dump
/// the last instructions leading up to the derailment.
fn trace_until_derail(console: &mut Console) {
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
            return;
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
}

/// Emit one line per executed instruction, byte-for-byte compatible with the
/// Geargrafx `gg_trace` harness (`PC:.... A:.. X:.. Y:.. P:.. SP:..`), so the
/// two streams can be `diff`ed to the first divergence.
///
/// Semantics match Geargrafx exactly: each line is an instruction's address
/// together with the register file *after* that instruction retired. Interrupt
/// vector fetches are not logged as their own line (the next logged PC is the
/// first instruction of the handler), again matching the reference.
///
/// ```text
/// # reference (known-good):
/// cd ../geargrafx && GG_TRACE=1 ./gg_trace "<rom>" 20 > /tmp/gg.txt
/// # ours:
/// TRACEDUMP=1 TRACEDUMP_STEPS=120000 cargo run --release -- "<rom>" > /tmp/tg.txt
/// # first divergence:
/// diff <(head -120000 /tmp/gg.txt) /tmp/tg.txt | head
/// ```
///
/// Tunables: `TRACEDUMP_STEPS` (instruction budget, default 200000).
fn trace_dump(console: &mut Console) {
    let steps = env_u64("TRACEDUMP_STEPS", 200_000);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    use std::io::Write;
    for _ in 0..steps {
        let (pc, _op) = console.debug_step();
        let r = &console.cpu().registers;
        // Registers are now post-execution, matching Geargrafx's trace entry.
        let _ = writeln!(
            out,
            "PC:{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X}",
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

// The WATCH tracer and its instruction-decode helpers below are disabled until
// the mos6502 disassembler is released (https://github.com/mre/mos6502/pull/135).
/*
/// Headless boot-state tracer. Watches the interrupt machinery and a handful of
/// game RAM variables to answer: which interrupt is meant to advance the boot
/// state machine, does it fire, and does the game's RCR ever match a line?
///
/// Tunable through the environment:
///
/// ```text
/// WATCH=1 WATCH_ADDRS=0070,2dfa,0069,13a9 WATCH_FRAMES=600 WATCH_MAX_LINES=3000 \
///     cargo run --release -- game.zip
/// ```
fn watch_state(console: &mut Console) {
    // Logical RAM addresses to watch (game's own view, via the MMU). Defaults
    // are the boot-state variables seen in the SF2 attract loop.
    let watched: Vec<u16> = std::env::var("WATCH_ADDRS")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|t| u16::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
                .collect()
        })
        .unwrap_or_else(|| vec![0x0070, 0x2DFA, 0x0069, 0x13A9]);
    let max_frames: u64 = env_u64("WATCH_FRAMES", 600);
    let max_lines: u64 = env_u64("WATCH_MAX_LINES", 3000);

    // PC ranges to disassemble live (using the runtime MMU mapping, so banked
    // code reads the right bytes). Format: `start:end[,start:end...]` in hex.
    let dis_ranges: Vec<(u16, u16)> = std::env::var("WATCH_DIS")
        .ok()
        .as_deref()
        .unwrap_or("d93e:d98e")
        .split(',')
        .filter_map(parse_hex_range)
        .collect();

    // Logical addresses whose *writers* we want to find: every instruction that
    // stores to one of these is reported once (with its PC). This answers
    // "what code is supposed to set $70?" directly. Off unless WATCH_STORES set.
    let store_targets: Vec<u16> = std::env::var("WATCH_STORES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|t| u16::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
                .collect()
        })
        .unwrap_or_default();
    let mut stores_seen: std::collections::HashSet<(u16, u16)> = std::collections::HashSet::new();

    console.bus_mut().vdc.set_trace(true);

    // Watch the physical work-RAM window that backs the palette staging buffer
    // (bank $F8 offset $600, = logical $2600 under MPR1=$F8), catching writes
    // via ANY logical address / MPR mapping. Configurable via WATCH_RAM=lo:hi.
    let (ram_lo, ram_hi) = std::env::var("WATCH_RAM")
        .ok()
        .and_then(|s| parse_hex_range(&s))
        .map_or((0x0600usize, 0x0700usize), |(a, b)| {
            (a as usize, b as usize)
        });
    console.bus_mut().debug.ram_watch_lo = ram_lo;
    console.bus_mut().debug.ram_watch_hi = ram_hi;
    let mut prev_ram_writes = console.bus().debug.ram_watch_writes;
    let mut ram_writers: std::collections::BTreeMap<u16, (String, u64)> =
        std::collections::BTreeMap::new();

    println!(
        "watching {} RAM addr(s): {}",
        watched.len(),
        watched
            .iter()
            .map(|a| format!("${a:04x}"))
            .collect::<Vec<_>>()
            .join(" "),
    );

    let mut prev_vals: Vec<u8> = watched.iter().map(|&a| console.bus().peek(a)).collect();
    let mut prev_irqs = console.bus().debug.irq_vectors_fetched;
    let mut prev_scanline = console.bus().vdc.scanline();
    let mut frame: u64 = 0;
    let mut printed: u64 = 0;

    // Per-frame IRQ aggregation, so a high-frequency timer doesn't drown the
    // trace. Indexed 0=TIQ, 1=IRQ1, 2=IRQ2; each tracks count + last handler.
    let mut irq_counts = [0u64; 3];
    let mut irq_handler = [0u16; 3];

    // PC hotspot histogram: where is the CPU actually spending its time? This
    // reveals the wait loop the boot state machine is stuck in.
    let mut pc_hist: std::collections::HashMap<u16, u64> = std::collections::HashMap::new();
    let mut steps: u64 = 0;

    // Live disassembly of the configured PC range(s): first-seen instruction
    // text per PC, plus per-operand value tracking so a constant (never
    // satisfied) poll target stands out from one that varies.
    let mut listing: std::collections::BTreeMap<u16, String> = std::collections::BTreeMap::new();
    let mut operand_addr: std::collections::BTreeMap<u16, Option<u16>> =
        std::collections::BTreeMap::new();
    // operand address -> (first value, last value, ever changed)
    let mut reads: std::collections::BTreeMap<u16, (u8, u8, bool)> =
        std::collections::BTreeMap::new();

    // PCs that write the VCE (palette) ports, with a live disassembly and a hit
    // count, to locate the palette uploader precisely.
    let mut vce_writers: std::collections::BTreeMap<u16, (String, u64)> =
        std::collections::BTreeMap::new();
    let mut prev_vce_writes = console.bus().debug.vce_writes;

    // Every block transfer (TII/TDD/TIN/TIA/TAI) the game runs, deduped by PC,
    // with source/dest/length, to trace how tiles and palettes are staged.
    let mut block_xfers: std::collections::BTreeMap<u16, (String, u64)> =
        std::collections::BTreeMap::new();

    while frame < max_frames {
        let (pc, _op) = console.debug_step();
        *pc_hist.entry(pc).or_insert(0) += 1;
        steps += 1;

        // Locate the VCE palette uploader: if this instruction wrote a VCE
        // port, record its PC + live disassembly.
        let vw = console.bus().debug.vce_writes;
        if vw != prev_vce_writes {
            let delta = vw - prev_vce_writes;
            prev_vce_writes = vw;
            let entry = vce_writers.entry(pc).or_insert_with(|| {
                let d = disassemble_one(pc, |a| console.bus().peek(a));
                (d.text, 0)
            });
            entry.1 += delta;
        }

        // Locate writers of the watched physical RAM window (palette buffer).
        let rw = console.bus().debug.ram_watch_writes;
        if rw != prev_ram_writes {
            let delta = rw - prev_ram_writes;
            prev_ram_writes = rw;
            let entry = ram_writers.entry(pc).or_insert_with(|| {
                let d = disassemble_one(pc, |a| console.bus().peek(a));
                (d.text, 0)
            });
            entry.1 += delta;
        }

        // Capture block transfers (their dest reveals tile/palette staging).
        {
            let d = disassemble_one(pc, |a| console.bus().peek(a));
            if matches!(d.mode, AddressingMode::BlockTransfer) {
                block_xfers
                    .entry(pc)
                    .and_modify(|e| e.1 += 1)
                    .or_insert((d.text, 1));
            }
        }

        // Live disassembly capture for the configured range(s).
        if dis_ranges.iter().any(|&(lo, hi)| pc >= lo && pc <= hi) {
            if let std::collections::btree_map::Entry::Vacant(slot) = operand_addr.entry(pc) {
                let d = disassemble_one(pc, |a| console.bus().peek(a));
                let addr = direct_operand_addr(&d);
                let opcode = console.bus().peek(pc);
                let text = match addr {
                    Some(a) => format!("[{opcode:02X}] {:<16} ; reads ${a:04X}", d.text),
                    None => format!("[{opcode:02X}] {}", d.text),
                };
                listing.insert(pc, text);
                slot.insert(addr);
            }
            if let Some(Some(addr)) = operand_addr.get(&pc).copied() {
                let v = console.bus().peek(addr);
                reads
                    .entry(addr)
                    .and_modify(|e| {
                        e.1 = v;
                        if v != e.0 {
                            e.2 = true;
                        }
                    })
                    .or_insert((v, v, false));
            }
        }

        // Store-tracer: find which instructions write the watched targets,
        // resolving indexed and indirect effective addresses with live X/Y.
        if !store_targets.is_empty() {
            let d = disassemble_one(pc, |a| console.bus().peek(a));
            if is_store(&d) {
                let x = console.cpu().registers.index_x;
                let y = console.cpu().registers.index_y;
                if let Some(addr) = effective_store_addr(&d, x, y, |a| console.bus().peek(a))
                    && store_targets.contains(&addr)
                    && stores_seen.insert((pc, addr))
                    && printed < max_lines
                {
                    println!(
                        "[f{frame:>4}] store to ${addr:04X} by ${pc:04X}: {:<16} (X={x:02X} Y={y:02X})",
                        d.text
                    );
                    printed += 1;
                }
            }
        }

        // Frame accounting: the VDC scanline wraps once per frame.
        let scanline = console.bus().vdc.scanline();
        if scanline < prev_scanline {
            // Flush the IRQ summary for the frame that just ended.
            let labels = ["TIQ/timer", "IRQ1/VDC", "IRQ2/BRK"];
            let mut parts = Vec::new();
            for i in 0..3 {
                if irq_counts[i] > 0 {
                    parts.push(format!(
                        "{} x{} (h${:04x})",
                        labels[i], irq_counts[i], irq_handler[i]
                    ));
                }
            }
            if !parts.is_empty() && printed < max_lines {
                println!("[f{frame:>4} end] IRQ: {}", parts.join(", "));
                printed += 1;
            }
            irq_counts = [0; 3];
            frame += 1;
        }
        prev_scanline = scanline;

        // VDC events captured during this step (RCR/CR writes, RR/VD raises).
        for ev in console.bus_mut().vdc.drain_events() {
            use turbografx::vdc::VdcEventKind::*;
            let msg = match ev.kind {
                RcrWrite(v) => format!("RCR <- ${v:03x} (line {})", v.wrapping_sub(64)),
                CrWrite(v) => format!(
                    "CR  <- ${v:04x} [bg={} spr={} virq={} rirq={}]",
                    (v >> 7) & 1,
                    (v >> 6) & 1,
                    (v >> 3) & 1,
                    (v >> 2) & 1,
                ),
                RasterMatch { rcr, irq } => format!(
                    "RR  match rcr=${rcr:03x} irq={}",
                    if irq { "RAISED" } else { "off" }
                ),
                Vblank { irq } => {
                    format!("VD  vblank irq={}", if irq { "RAISED" } else { "off" })
                }
            };
            if printed < max_lines {
                println!("[f{frame:>4} sl{:>3}] {msg}", ev.scanline);
                printed += 1;
            }
        }

        // IRQ dispatch: tally by source for the current frame.
        let irqs = console.bus().debug.irq_vectors_fetched;
        if irqs != prev_irqs {
            let i = match console.bus().debug.last_irq_vector {
                0xFFFA => 0,
                0xFFF8 => 1,
                _ => 2,
            };
            irq_counts[i] += 1;
            irq_handler[i] = console.cpu().registers.program_counter;
            prev_irqs = irqs;
        }

        // Watched RAM variable changes (game's logical view). Skip frame 0,
        // whose "changes" are just the MMU being pointed at real RAM.
        for (i, &addr) in watched.iter().enumerate() {
            let now = console.bus().peek(addr);
            if now != prev_vals[i] {
                if frame > 0 && printed < max_lines {
                    println!(
                        "[f{frame:>4} sl{scanline:>3}] pc=${pc:04x}  ${addr:04x}: {:02x} -> {:02x}",
                        prev_vals[i], now
                    );
                    printed += 1;
                }
                prev_vals[i] = now;
            }
        }
    }

    println!(
        "\nwatch ended: {frame} frames, {steps} instructions, PC=${:04x}",
        console.cpu().registers.program_counter
    );
    println!("final watched values:");
    for &addr in &watched {
        println!("  ${addr:04x} = {:02x}", console.bus().peek(addr));
    }

    // VRAM occupancy: if tile graphics were uploaded, this is far from zero.
    let vram = console.bus().vdc.vram();
    let nonzero = vram.iter().filter(|&&w| w != 0).count();
    println!(
        "\nVRAM: {nonzero}/{} words non-zero ({:.1}%)",
        vram.len(),
        100.0 * nonzero as f64 / vram.len() as f64
    );

    // VDC register + BAT diagnostics: why is a populated VRAM rendering blank?
    let vdc = &console.bus().vdc;
    let cr = vdc.control();
    let mwr = vdc.register(0x09);
    println!(
        "VDC: CR=${cr:04X} (bg={} spr={}) MWR=${mwr:04X} BXR=${:04X} BYR=${:04X} HDR=${:04X} VDW=${:04X}",
        (cr >> 7) & 1,
        (cr >> 6) & 1,
        vdc.register(0x07),
        vdc.register(0x08),
        vdc.register(0x0B),
        vdc.register(0x0D),
    );
    // BAT occupancy across the four candidate widths (the BAT lives at VRAM 0).
    for &(w, h) in &[(32usize, 32usize), (64, 32), (128, 32), (64, 64)] {
        let words = w * h;
        let nz = vram[..words.min(vram.len())]
            .iter()
            .filter(|&&v| v & 0x0FFF != 0)
            .count();
        println!("  BAT {w}x{h}: {nz}/{words} entries with non-zero char code");
    }

    // Split the blank-screen diagnosis: did the BG renderer emit palette
    // indices (framebuffer), and does the VCE palette hold any color?
    let vdc = &console.bus().vdc;
    let fb_nz = vdc.framebuffer.iter().filter(|&&i| i != 0).count();
    println!(
        "framebuffer: {fb_nz}/{} pixels with non-zero palette index",
        vdc.framebuffer.len()
    );
    let vce = &console.bus().vce;
    let pal_nz = (0..512).filter(|&e| vce.color_raw(e) != 0).count();
    println!("VCE palette: {pal_nz}/512 entries non-zero");
    println!("VCE writes seen: {}", console.bus().debug.vce_writes);
    println!(
        "VCE writes by offset: {:?}  (data-lo $0404 non-zero: {})",
        console.bus().debug.vce_offset_writes,
        console.bus().debug.vce_data_lo_nonzero
    );
    if !vce_writers.is_empty() {
        println!("VCE writer instructions (pc: disasm x count):");
        for (pc, (text, count)) in &vce_writers {
            println!("  ${pc:04X}: {text:<18} x{count}");
        }
    }

    // Palette source buffer: is it actually populated? Show the live MPR map
    // and the first bytes the TIA uploads read from.
    println!("MPR = {:02X?}", console.cpu().registers.mpr);
    for base in [0x2600u16, 0x2700, 0x2800, 0x2900] {
        let bytes: Vec<String> = (0..16)
            .map(|i| format!("{:02X}", console.bus().peek(base + i)))
            .collect();
        println!("  ${base:04X}: {}", bytes.join(" "));
    }

    if !block_xfers.is_empty() {
        println!("block transfers (pc: disasm x count):");
        for (pc, (text, count)) in &block_xfers {
            println!("  ${pc:04X}: {text} x{count}");
        }
    }

    if !ram_writers.is_empty() {
        println!("writers of physical RAM ${ram_lo:04X}..${ram_hi:04X} (bank $F8):");
        for (pc, (text, count)) in &ram_writers {
            println!("  ${pc:04X}: {text:<20} x{count}");
        }
    } else {
        println!("NO writes to physical RAM ${ram_lo:04X}..${ram_hi:04X} (bank $F8)");
    }

    // Top PC hotspots: the spin loop and the busiest ISR code.
    let mut hot: Vec<(u16, u64)> = pc_hist.into_iter().collect();
    hot.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    println!("\ntop 15 PC hotspots (instr count, % of total):");
    for (pc, count) in hot.into_iter().take(15) {
        let pct = 100.0 * count as f64 / steps.max(1) as f64;
        println!("  ${pc:04x}  {count:>9}  {pct:5.1}%");
    }

    if !listing.is_empty() {
        println!("\nlive disassembly of watched range(s):");
        for (pc, text) in &listing {
            println!("  ${pc:04X}: {text}");
        }
        println!("\noperands read inside range (addr: first -> last):");
        for (addr, (first, last, changed)) in &reads {
            println!(
                "  ${addr:04X}: {first:02X} -> {last:02X}  {}",
                if *changed { "VARIES" } else { "constant" }
            );
        }
    }
}

/// Extract the directly-resolvable memory address a load/branch instruction
/// reads, for the simple addressing modes a poll loop uses (zero page,
/// absolute, and the BBR/BBS zero-page bit test). Indexed and indirect modes
/// need register/pointer state to resolve, so they're reported as `None`.
fn direct_operand_addr(d: &DisasmInstr) -> Option<u16> {
    match (d.mode, d.operand) {
        (
            AddressingMode::ZeroPage | AddressingMode::Absolute,
            OpInput::UseAddress { address, .. },
        ) => Some(address),
        (AddressingMode::ZeroPageRelative, OpInput::UseBitBranch { zp_address, .. }) => {
            Some(u16::from(zp_address))
        }
        _ => None,
    }
}

/// Whether a decoded instruction writes to its memory operand (so a matching
/// direct operand address is a real write target, not just a read).
fn is_store(d: &DisasmInstr) -> bool {
    let mnemonic = d.text.split_whitespace().next().unwrap_or("");
    matches!(
        mnemonic,
        "STA"
            | "STX"
            | "STY"
            | "STZ"
            | "INC"
            | "DEC"
            | "RMB"
            | "SMB"
            | "TRB"
            | "TSB"
            | "ASL"
            | "LSR"
            | "ROL"
            | "ROR"
    )
}

/// Resolve the effective memory address a store-class instruction writes,
/// including indexed and indirect modes, using the live `X`/`Y` registers and a
/// peek-style reader for pointer dereferences. Returns `None` for non-stores or
/// accumulator/implied forms.
fn effective_store_addr(d: &DisasmInstr, x: u8, y: u8, read: impl Fn(u16) -> u8) -> Option<u16> {
    if !is_store(d) {
        return None;
    }
    let OpInput::UseAddress { address: base, .. } = d.operand else {
        return None;
    };
    let x = u16::from(x);
    let y = u16::from(y);
    let zp_ptr = |p: u16| {
        let lo = u16::from(read(p & 0x00FF));
        let hi = u16::from(read((p + 1) & 0x00FF));
        lo | (hi << 8)
    };
    let addr = match d.mode {
        AddressingMode::ZeroPage | AddressingMode::Absolute => base,
        AddressingMode::ZeroPageX => (base + x) & 0x00FF,
        AddressingMode::ZeroPageY => (base + y) & 0x00FF,
        AddressingMode::AbsoluteX => base.wrapping_add(x),
        AddressingMode::AbsoluteY => base.wrapping_add(y),
        AddressingMode::ZeroPageIndirect => zp_ptr(base),
        AddressingMode::IndexedIndirectX => zp_ptr((base + x) & 0x00FF),
        AddressingMode::IndirectIndexedY => zp_ptr(base).wrapping_add(y),
        _ => return None,
    };
    Some(addr)
}

/// Parse a `start:end` hex PC range (e.g. `"d93e:d98e"`).
fn parse_hex_range(s: &str) -> Option<(u16, u16)> {
    let (lo, hi) = s.trim().split_once(':')?;
    let lo = u16::from_str_radix(lo.trim().trim_start_matches("0x"), 16).ok()?;
    let hi = u16::from_str_radix(hi.trim().trim_start_matches("0x"), 16).ok()?;
    Some((lo.min(hi), lo.max(hi)))
}
*/

/// Parse a `u64` environment variable, falling back to `default`.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A tiny HuCard that boots, sets up the MMU, and spins.
///
/// The reset vector (logical `$FFFE`, physical bank 0 `$1FFE`) points at a
/// routine that maps RAM/hardware into the logical space and then loops.
fn smoke_test_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x2000];
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
    rom[0x1FFE] = 0x00; // reset vector low  -> $E000
    rom[0x1FFF] = 0xE0; // reset vector high
    rom
}
