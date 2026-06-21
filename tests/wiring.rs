//! End-to-end wiring tests for the console scaffold.
//!
//! These don't aim for hardware accuracy; they prove that the CPU, the MMU
//! shadow, the memory map and the VDC→interrupt-controller→CPU interrupt path
//! are connected correctly, so the graphics work can build on a known-good base.

use turbografx::{Cartridge, Console};

/// Build a single-bank HuCard with `program` placed at physical `$0000`
/// (logical `$E000` at reset) and the reset vector pointing there.
fn rom_with(program: &[u8]) -> Cartridge {
    let mut rom = vec![0u8; 0x2000];
    rom[..program.len()].copy_from_slice(program);
    rom[0x1FFE] = 0x00; // reset vector low  -> $E000
    rom[0x1FFF] = 0xE0; // reset vector high
    Cartridge::from_bytes(rom)
}

#[test]
fn reset_vector_is_remapped_to_huc6280_layout() {
    // The reset vector lives at $FFFE on the HuC6280, but the mos6502 core reads
    // $FFFC. The bus must remap it so the CPU starts at $E000.
    let console = Console::new(rom_with(&[0x4C, 0x00, 0xE0])); // JMP $E000
    assert_eq!(console.cpu().registers.program_counter, 0xE000);
}

#[test]
fn mmu_maps_zero_page_writes_into_work_ram() {
    // Map work RAM (bank $F8) to MPR1 so logical $2000 (the HuC6280 zero page)
    // lands in RAM, then store a byte through the zero page.
    let program = &[
        0xA9, 0xF8, // LDA #$F8
        0x53, 0x02, // TAM #$02   ; MPR1 = $F8
        0xA9, 0x42, // LDA #$42
        0x85, 0x10, // STA $10    ; -> logical $2010 -> RAM
        0x4C, 0x08, 0xE0, // JMP $E008 (spin)
    ];
    let mut console = Console::new(rom_with(program));

    for _ in 0..5 {
        console.run_frame();
        if console.cpu().registers.accumulator == 0x42 {
            break;
        }
    }
    assert_eq!(console.cpu().registers.accumulator, 0x42);
    // The store landed in work RAM at offset $0010.
    assert_eq!(console.bus().ram_byte(0x0010), 0x42);
}

#[test]
fn vdc_vblank_raises_irq_and_cpu_services_it() {
    // Boot code:
    //   - map work RAM to MPR1 (for the stack, used by interrupt servicing)
    //   - map the hardware page to MPR0 (for VDC / IRQ controller access)
    //   - enable the VDC vblank interrupt (CR bit 3) via the VDC ports
    //   - unmask IRQ1 in the interrupt controller
    //   - CLI, then spin
    // The IRQ1 handler (vector $FFF8) increments a RAM counter we can observe.
    let boot = &[
        // MPR0 = $FF (hardware page at logical $0000)
        0xA9, 0xFF, 0x53, 0x01, // LDA #$FF : TAM #$01
        // MPR1 = $F8 (work RAM at logical $2000)
        0xA9, 0xF8, 0x53, 0x02, // LDA #$F8 : TAM #$02
        // VDC: select Control register (5), then write $0008 (vblank IRQ enable).
        0xA9, 0x05, 0x8D, 0x00, 0x00, // LDA #$05 : STA $0000  (VDC reg select)
        0xA9, 0x08, 0x8D, 0x02, 0x00, // LDA #$08 : STA $0002  (CR low = vblank IRQ)
        0xA9, 0x00, 0x8D, 0x03, 0x00, // LDA #$00 : STA $0003  (CR high)
        // Interrupt controller: unmask everything ($1402 = 0).
        0xA9, 0x00, 0x8D, 0x02, 0x14, // LDA #$00 : STA $1402
        // Clear the counter at $2000, enable IRQs, and spin.
        0xA9, 0x00, 0x85, 0x00, // LDA #$00 : STA $00  (counter = 0)
        0x58, // CLI
        0x4C, 0x21, 0xE0, // JMP $E021 (spin on this instruction)
    ];

    let mut rom = vec![0u8; 0x2000];
    rom[..boot.len()].copy_from_slice(boot);

    // IRQ1 handler at $E100: INC $00 ; RTI
    let handler = 0x0100;
    rom[handler] = 0xE6; // INC $00
    rom[handler + 1] = 0x00;
    rom[handler + 2] = 0x40; // RTI

    // Vectors (logical $FFFx -> physical bank-0 $1FFx via MPR7 = 0).
    rom[0x1FF8] = 0x00; // IRQ1 low  -> $E100
    rom[0x1FF9] = 0xE1; // IRQ1 high
    rom[0x1FFE] = 0x00; // reset low -> $E000
    rom[0x1FFF] = 0xE0; // reset high

    let mut console = Console::new(Cartridge::from_bytes(rom));

    // Run a couple of frames; each should fire at least one vblank IRQ.
    console.run_frame();
    console.run_frame();

    // The handler increments the counter at work-RAM offset 0 (logical $2000).
    let counter = console.bus().ram_byte(0x0000);
    assert!(
        counter >= 1,
        "expected the vblank IRQ handler to run at least once, counter = {counter}"
    );
}
