//! Headless frame dumper: runs a ROM for a number of frames and writes the
//! active display to a PNG. Handy for verifying the VDC without a window.
//!
//! ```text
//! cargo run --release --example dump_frame -- "roms/Bomberman.zip" 180 out.png
//! ```

use turbografx::{Cartridge, Console};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dump_frame <rom> [frames] [out.png]");
        std::process::exit(2);
    };
    let frames: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(180);
    let out = args.next().unwrap_or_else(|| "frame.png".to_owned());

    let cart = Cartridge::from_path(&path).expect("load ROM");
    let mut console = Console::new(cart);
    for _ in 0..frames {
        console.run_frame();
    }

    let (w, h) = console.active_size();
    let rgba = console.active_frame_rgba();

    // Quick summary so blank/!blank is obvious without opening the PNG.
    let mut colors = std::collections::BTreeSet::new();
    for px in rgba.chunks_exact(4) {
        colors.insert((px[0], px[1], px[2]));
    }
    image::save_buffer(&out, &rgba, w as u32, h as u32, image::ColorType::Rgba8)
        .expect("write PNG");
    println!(
        "wrote {out} ({w}x{h}) after {frames} frames; {} distinct colours, PC={:#06x}",
        colors.len(),
        console.cpu().registers.program_counter,
    );
}
