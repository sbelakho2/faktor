// HAR-18 emulated-HIL firmware image (in-repo provenance).
//
// Bare-metal Cortex-M3 image for TI LM3S6965 as modelled by QEMU's
// `lm3s6965evb` machine. It boots, initialises UART0, prints a typed banner,
// and serves a line protocol (PING / RAND) over the emulated serial port.
// The protocol constants (seed, LCG parameters) are mirrored by an
// independent reference implementation in tests/hil/emulated-hil.mjs; the
// harness asserts firmware replies against that reference.
//
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process
#![no_std]
#![no_main]

use core::arch::global_asm;
use core::panic::PanicInfo;

/// Deterministic protocol seed; mirrored in the harness reference oracle.
const SEED: u32 = 424_242;
/// LCG parameters (Numerical Recipes); mirrored in the harness reference.
const LCG_A: u32 = 1_664_525;
const LCG_C: u32 = 1_013_904_223;

// TI LM3S6965 peripheral map (as implemented by QEMU lm3s6965evb).
const SYSCTL_RCGC1: *mut u32 = 0x400F_E104 as *mut u32; // UART0 clock gate
const SYSCTL_RCGC2: *mut u32 = 0x400F_E108 as *mut u32; // GPIOA clock gate
const GPIO_PORTA_AFSEL: *mut u32 = 0x4000_4420 as *mut u32;
const GPIO_PORTA_DEN: *mut u32 = 0x4000_451C as *mut u32;
const UART0_DR: *mut u32 = 0x4000_C000 as *mut u32;
const UART0_FR: *const u32 = 0x4000_C018 as *const u32;
const UART0_IBRD: *mut u32 = 0x4000_C024 as *mut u32;
const UART0_FBRD: *mut u32 = 0x4000_C028 as *mut u32;
const UART0_LCRH: *mut u32 = 0x4000_C02C as *mut u32;
const UART0_CTL: *mut u32 = 0x4000_C030 as *mut u32;

const FR_TXFF: u32 = 1 << 3; // transmit FIFO full
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty

// Cortex-M3 vector table + reset handler, emitted by the assembler: the
// table is `.word __stack_top` (RAM top, hard-coded 0x20008000 — well
// inside the 64 KiB RAM, stack grows down) and `.word reset_handler`
// (the `.thumb_func` marker makes the linker set the thumb bit). QEMU RAM
// starts zeroed and this image has no .data/.bss, so the reset handler
// only re-sets SP and branches to `rust_main`.
global_asm!(
    ".section .vector_table, \"ax\"",
    ".global __vector_table",
    "__vector_table:",
    ".word 0x20008000",
    ".word reset_handler",
    ".section .text.reset, \"ax\"",
    ".thumb",
    ".thumb_func",
    ".global reset_handler",
    "reset_handler:",
    "ldr r0, =0x20008000",
    "mov sp, r0",
    "bl rust_main",
    "1: b 1b",
);

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // A firmware panic is a typed terminal state, never a reboot loop:
    // the harness's boot/request deadlines convert silence into a typed
    // failure while QEMU stays bounded.
    loop {
        core::hint::spin_loop();
    }
}

// SAFETY: every `reg` argument is one of the documented LM3S6965 MMIO
// register addresses from the constant table above (linker-placed
// peripherals); a volatile write is the only way to drive device state.
unsafe fn mmio_write(reg: *mut u32, val: u32) {
    core::ptr::write_volatile(reg, val);
}

// SAFETY: same register-address contract as `mmio_write` — the caller
// passes only constants from the table above; volatile read of device
// state is required to observe hardware changes.
unsafe fn mmio_read(reg: *const u32) -> u32 {
    core::ptr::read_volatile(reg)
}

// SAFETY: touches only the MMIO constants defined in this file (clock
// gates, GPIOA, UART0) exactly once, before any other UART operation is
// issued from `rust_main`; no caller-supplied pointers exist here.
unsafe fn uart0_init() {
    // Gate clocks to UART0 + GPIOA, then give the (unmodled) settling a
    // bounded spin: QEMU does not model the delay, real silicon does.
    mmio_write(SYSCTL_RCGC1, mmio_read(SYSCTL_RCGC1) | 1);
    mmio_write(SYSCTL_RCGC2, mmio_read(SYSCTL_RCGC2) | 1);
    for _ in 0..16 {
        core::hint::spin_loop();
    }
    // PA0/PA1 alternate-function (U0Rx/U0Tx), digital enable.
    mmio_write(GPIO_PORTA_AFSEL, mmio_read(GPIO_PORTA_AFSEL) | 0x03);
    mmio_write(GPIO_PORTA_DEN, mmio_read(GPIO_PORTA_DEN) | 0x03);
    // 8N1 + FIFOs, arbitrary divisor (QEMU does not model baud timing).
    mmio_write(UART0_CTL, 0);
    mmio_write(UART0_IBRD, 8);
    mmio_write(UART0_FBRD, 44);
    mmio_write(UART0_LCRH, 0x60);
    mmio_write(UART0_CTL, 0x301); // enable | TXE | RXE
}

// SAFETY: `byte` is a plain `u8`; the unsafe operations are the MMIO
// constant accesses documented above — the TX spin is bounded by the
// harness's request deadline (QEMU's model never stalls).
unsafe fn uart0_send(byte: u8) {
    // QEMU's UART model always drains the TX FIFO, so this cannot hang in
    // the emulator; on hardware a stuck FIFO is bounded by the harness's
    // request deadline.
    while mmio_read(UART0_FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    mmio_write(UART0_DR, byte as u32);
}

// SAFETY: only calls `uart0_send`, whose safety contract holds for any
// input byte; this function introduces no pointer work of its own.
unsafe fn send_str(s: &str) {
    for b in s.bytes() {
        uart0_send(b);
    }
}

// SAFETY: only calls `uart0_send` (contract above) over a stack-local
// digit buffer; no raw pointers or addresses are formed.
unsafe fn send_u32(mut v: u32) {
    if v == 0 {
        uart0_send(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut n = 0;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        uart0_send(buf[n]);
    }
}

// SAFETY: the only unsafe operations are the MMIO constant accesses
// documented above; the caller's `buf` is a 64-byte stack slice and every
// store is guarded by `len < buf.len()` (overflow sets a typed flag
// instead of writing out of bounds).
unsafe fn recv_line(buf: &mut [u8; 64]) -> Option<usize> {
    let mut len = 0usize;
    let mut overflow = false;
    loop {
        while mmio_read(UART0_FR) & FR_RXFE != 0 {
            core::hint::spin_loop();
        }
        let byte = (mmio_read(UART0_DR) & 0xFF) as u8;
        match byte {
            b'\r' => {}
            b'\n' => {
                if overflow {
                    return None; // oversized line: typed protocol failure
                }
                return Some(len);
            }
            _ => {
                if len < buf.len() {
                    buf[len] = byte;
                    len += 1;
                } else {
                    overflow = true;
                }
            }
        }
    }
}

#[no_mangle]
extern "C" fn rust_main() -> ! {
    // SAFETY: init-before-use ordering — `uart0_init` runs before the first
    // send/recv, and every callee (`uart0_*`, `send_*`, `recv_line`) has its
    // own documented safety contract satisfied here; all inputs are
    // stack-local buffers and constants.
    unsafe {
        uart0_init();
        send_str("FAKTOR-HIL-FW v1 seed=");
        send_u32(SEED);
        send_str("\r\n");

        let mut lcg = SEED;
        let mut seq: u32 = 0;
        let mut line = [0u8; 64];

        loop {
            match recv_line(&mut line) {
                None => {
                    seq = seq.wrapping_add(1);
                    send_str("ERR line-too-long\r\n");
                }
                Some(len) => {
                    let cmd = &line[..len];
                    if cmd.is_empty() {
                        continue; // stray newline: ignored, not an error
                    }
                    seq = seq.wrapping_add(1);
                    if cmd == b"PING" {
                        send_str("PONG seed=");
                        send_u32(SEED);
                        send_str(" seq=");
                        send_u32(seq);
                        send_str("\r\n");
                    } else if cmd == b"RAND" {
                        lcg = lcg.wrapping_mul(LCG_A).wrapping_add(LCG_C);
                        send_str("RAND val=");
                        send_u32(lcg);
                        send_str(" seq=");
                        send_u32(seq);
                        send_str("\r\n");
                    } else {
                        send_str("ERR unknown seq=");
                        send_u32(seq);
                        send_str("\r\n");
                    }
                }
            }
        }
    }
}
