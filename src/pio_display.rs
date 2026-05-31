/// PIO-based LED matrix driver for the Pimoroni Galactic Unicorn (53×11).
///
/// Implements the FM6047 constant-current LED driver protocol with BCD dimming,
/// matching Pimoroni's reference implementation (galactic_unicorn.pio / .cpp).
///
/// Pin assignments:
///   GPIO13 = CLK (side-set)   GPIO14 = DATA (SET base)
///   GPIO15 = LATCH (SET+1)    GPIO16 = BLANK (SET+2)
///   GPIO17-20 = ROW0-3 (OUT base)
///
/// Bitstream format — 60 bytes per BCD frame per row (9240 bytes total):
///   [0]      pixel count − 1 (= 52)
///   [1]      row select (0–10)
///   [2..54]  53 pixel bytes, each xxxxxbgr (bit0=B, bit1=G, bit2=R)
///   [55]     dummy
///   [56..59] BCD tick count (little-endian u32, = 1 << frame_index)

use cortex_m::asm::delay as asm_delay;
use embassy_rp::dma::AnyChannel;
use embassy_rp::gpio::Level;
use embassy_rp::pac;
use embassy_rp::peripherals::{DMA_CH1, PIN_13, PIN_14, PIN_15, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIO0};
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::pio::{
    Config, Direction, FifoJoin, Instance, InterruptHandler, Pio, ShiftConfig, ShiftDirection,
    StateMachine,
};
use embassy_rp::{into_ref, Peripheral, PeripheralRef};

use crate::display::{HEIGHT, WIDTH};

const BCD_FRAME_COUNT: usize = 14;
const BCD_FRAME_BYTES: usize = 60;
const ROW_BYTES:       usize = BCD_FRAME_COUNT * BCD_FRAME_BYTES; // 840
const BITSTREAM_LEN:   usize = HEIGHT * ROW_BYTES;                // 9240
const BITSTREAM_WORDS: usize = BITSTREAM_LEN / 4;                 // 2310

// 14-bit gamma correction (gamma ≈ 2.0, compile-time computed)
const fn gamma_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = ((i * i * 16383) / (255 * 255)) as u16;
        i += 1;
    }
    t
}
static GAMMA_14BIT: [u16; 256] = gamma_table();

// Static 9240-byte bitstream, 4-byte aligned for u32 DMA
#[repr(C, align(4))]
struct AlignedBuf([u8; BITSTREAM_LEN]);
static mut BS: AlignedBuf = AlignedBuf([0u8; BITSTREAM_LEN]);

pub struct PioDisplay<'d> {
    sm:  StateMachine<'d, PIO0, 0>,
    dma: PeripheralRef<'d, AnyChannel>,
}

impl<'d> PioDisplay<'d> {
    pub fn new(
        pio:       PIO0,
        irq:       impl Binding<<PIO0 as Instance>::Interrupt, InterruptHandler<PIO0>>,
        dma:       impl Peripheral<P = DMA_CH1> + 'd,
        pin_clk:   impl Peripheral<P = PIN_13>  + 'd,
        pin_data:  impl Peripheral<P = PIN_14>  + 'd,
        pin_latch: impl Peripheral<P = PIN_15>  + 'd,
        pin_blank: impl Peripheral<P = PIN_16>  + 'd,
        pin_r0:    impl Peripheral<P = PIN_17>  + 'd,
        pin_r1:    impl Peripheral<P = PIN_18>  + 'd,
        pin_r2:    impl Peripheral<P = PIN_19>  + 'd,
        pin_r3:    impl Peripheral<P = PIN_20>  + 'd,
    ) -> Self {
        // Step 1: FM6047 chip init via bit-bang before PIO takes these pins
        fm6047_init();

        into_ref!(dma, pin_clk, pin_data, pin_latch, pin_blank,
                  pin_r0, pin_r1, pin_r2, pin_r3);

        // Claim PIO0 and state machine 0 for the display.
        let Pio { mut common, sm0: mut sm, .. } = Pio::new(pio, irq);

        // Pimoroni galactic_unicorn.pio — exact port.
        // .side_set 1 opt  → CLK on GPIO13; opt means instructions without
        //                     a side value leave CLK unchanged.
        // SET base  → DATA=GPIO14 (bit0), LATCH=GPIO15 (bit1), BLANK=GPIO16 (bit2)
        // OUT base  → ROW0=GPIO17 … ROW3=GPIO20
        // Shift: RIGHT, autopull at 32 bits → LSByte first, matches little-endian layout
        let prg = pio_proc::pio_asm!(
            ".side_set 1 opt",
            ".wrap_target",
            "out y, 8",                    // pixel count (52) → Y
            "out pins, 8",                 // row index → ROW pins

            "pixels:",
            // blue bit (bit 0 of pixel byte)
            "out x, 1        side 0 [1]",  // X = blue, CLK low
            "set pins, 0b100",             // DATA=0, LATCH=0, BLANK=1
            "jmp !x endb",
            "set pins, 0b101",             // DATA=1, LATCH=0, BLANK=1
            "endb:",
            "nop             side 1 [2]",  // CLK high

            // green bit (bit 1)
            "out x, 1        side 0 [1]",
            "set pins, 0b100",
            "jmp !x endg",
            "set pins, 0b101",
            "endg:",
            "nop             side 1 [2]",

            // red bit (bit 2) + discard upper 5 bits
            "out x, 1        side 0 [1]",
            "set pins, 0b100",
            "jmp !x endr",
            "set pins, 0b101",
            "endr:",
            "out null, 5     side 1 [2]",  // CLK high, discard bits 7–3

            "jmp y-- pixels",

            "out null, 8",                 // discard dummy byte (offset 55)

            "set pins, 0b110 [5]",         // LATCH=1, BLANK=1 (hold 6 cycles)
            "set pins, 0b000",             // LATCH=0, BLANK=0 → row enabled

            "out y, 32",                   // BCD tick count → Y
            "bcd_delay:",
            "jmp y-- bcd_delay",

            "set pins, 0b100",             // BLANK=1 → row disabled
            ".wrap",
        );

        let pin_d   = common.make_pio_pin(pin_data);
        let pin_ck  = common.make_pio_pin(pin_clk);
        let pin_lt  = common.make_pio_pin(pin_latch);
        let pin_bl  = common.make_pio_pin(pin_blank);
        let pin_r0p = common.make_pio_pin(pin_r0);
        let pin_r1p = common.make_pio_pin(pin_r1);
        let pin_r2p = common.make_pio_pin(pin_r2);
        let pin_r3p = common.make_pio_pin(pin_r3);

        let loaded = common.load_program(&prg.program);
        let mut cfg = Config::default();

        cfg.use_program(&loaded, &[&pin_ck]);
        cfg.set_out_pins(&[&pin_r0p, &pin_r1p, &pin_r2p, &pin_r3p]);
        cfg.set_set_pins(&[&pin_d, &pin_lt, &pin_bl]);

        cfg.shift_out = ShiftConfig {
            direction: ShiftDirection::Right,
            auto_fill: true,
            threshold:  32,
        };
        cfg.fifo_join = FifoJoin::TxOnly;

        sm.set_config(&cfg);
        sm.set_pin_dirs(Direction::Out, &[
            &pin_ck, &pin_d, &pin_lt, &pin_bl,
            &pin_r0p, &pin_r1p, &pin_r2p, &pin_r3p,
        ]);
        // Start blanked on a non-visible row (0b1111 > 10)
        sm.set_pins(Level::High, &[&pin_bl, &pin_r0p, &pin_r1p, &pin_r2p, &pin_r3p]);
        sm.set_pins(Level::Low,  &[&pin_ck, &pin_d, &pin_lt]);

        unsafe { init_bitstream(); }

        sm.set_enable(true);

        Self { sm, dma: dma.map_into() }
    }

    /// Convert frame to BCD bitstream + push once (~3 ms, one full refresh).
    pub async fn flush(&mut self, frame: &[[(u8, u8, u8); WIDTH]; HEIGHT]) {
        self.build(frame);
        self.push().await;
    }

    /// Convert frame to BCD bitstream, then keep pushing for `ms` milliseconds.
    /// Use instead of `flush` + `Timer::after` so the display stays lit.
    pub async fn flush_for_ms(&mut self, frame: &[[(u8, u8, u8); WIDTH]; HEIGHT], ms: u64) {
        use embassy_time::{Duration, Instant};
        self.build(frame);
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            self.push().await;
        }
    }

    fn build(&mut self, frame: &[[(u8, u8, u8); WIDTH]; HEIGHT]) {
        unsafe {
            let bs = core::slice::from_raw_parts_mut(
                (&raw mut BS.0).cast::<u8>(),
                BITSTREAM_LEN,
            );
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let (r, g, b) = frame[y][x];
                    let gamma_r = GAMMA_14BIT[r as usize] as u32;
                    let gamma_g = GAMMA_14BIT[g as usize] as u32;
                    let gamma_b = GAMMA_14BIT[b as usize] as u32;
                    // Hardware coordinate flip (Pimoroni: x = (W-1)-x, y = (H-1)-y)
                    let fx = (WIDTH  - 1) - x;
                    let fy = (HEIGHT - 1) - y;
                    for f in 0..BCD_FRAME_COUNT {
                        let off = fy * ROW_BYTES + f * BCD_FRAME_BYTES + 2 + fx;
                        // pixel byte: bit2=R, bit1=G, bit0=B
                        bs[off] = (((gamma_r >> f) & 1) << 2
                                 | ((gamma_g >> f) & 1) << 1
                                 |  (gamma_b >> f) & 1) as u8;
                    }
                }
            }
        }
    }

    async fn push(&mut self) {
        unsafe {
            let words = core::slice::from_raw_parts(
                (&raw const BS.0).cast::<u32>(),
                BITSTREAM_WORDS,
            );
            self.sm.tx().dma_push(self.dma.reborrow(), words).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Bitstream structure initialisation (headers + BCD tick counts, called once)
// ---------------------------------------------------------------------------
unsafe fn init_bitstream() {
    let bs = core::slice::from_raw_parts_mut((&raw mut BS.0).cast::<u8>(), BITSTREAM_LEN);
    for row in 0..HEIGHT {
        for frame in 0..BCD_FRAME_COUNT {
            let base = row * ROW_BYTES + frame * BCD_FRAME_BYTES;
            bs[base]     = (WIDTH - 1) as u8; // pixel count - 1 = 52
            bs[base + 1] = row as u8;          // row select
            // bs[base+2 .. base+54]: pixel data, zero = all off
            // bs[base+55]: dummy byte, stays 0
            let ticks: u32 = 1 << frame;
            bs[base + 56] = (ticks         & 0xff) as u8;
            bs[base + 57] = ((ticks >>  8) & 0xff) as u8;
            bs[base + 58] = ((ticks >> 16) & 0xff) as u8;
            bs[base + 59] = ((ticks >> 24) & 0xff) as u8;
        }
    }
}

// ---------------------------------------------------------------------------
// FM6047 initialisation — bit-bang register write before PIO takes the pins
// ---------------------------------------------------------------------------

/// Write reg1 = 0b1111_1111_1100_1110 to all 10 FM6047 shift-register chips.
/// The 10th chip uses a special LATCH timing to select register-write mode.
fn fm6047_init() {
    const REG1: u16 = 0b1111_1111_1100_1110;
    const HALF: u32 = 1250; // ≥10 µs half-period at 125 MHz

    // Configure GPIO13–16 as SIO outputs
    for pin in [13u32, 14, 15, 16] {
        pac::IO_BANK0.gpio(pin as usize).ctrl().write(|w| w.set_funcsel(5));
        pac::SIO.gpio_oe(0).value_set().write_value(1 << pin);
    }
    pac::SIO.gpio_out(0).value_clr().write_value(0b1111 << 13); // all low
    pac::SIO.gpio_out(0).value_set().write_value(1 << 16);      // BLANK=1

    // First 9 chips: standard 16-bit shift
    for _ in 0..9 {
        for bit in (0..16).rev() {
            clk_bit((REG1 >> bit) & 1 != 0, HALF);
        }
    }

    // 10th chip: assert LATCH high after the 5th clock rising edge (i==4)
    for i in 0u32..16 {
        gpio_set(14, (REG1 >> (15 - i)) & 1 != 0); // DATA
        asm_delay(HALF);
        gpio_set(13, true);  // CLK high
        asm_delay(HALF);
        gpio_set(13, false); // CLK low
        if i == 4 {
            gpio_set(15, true); // LATCH high — stays asserted for remaining bits
        }
    }
    gpio_set(15, false); // LATCH low

    // Brief blank-low pulse per Pimoroni to clear residual glow
    gpio_set(16, false);
    asm_delay(HALF);
    gpio_set(16, true);
}

fn clk_bit(data: bool, half_period: u32) {
    gpio_set(14, data);
    asm_delay(half_period);
    gpio_set(13, true);
    asm_delay(half_period);
    gpio_set(13, false);
}

fn gpio_set(pin: u32, high: bool) {
    if high {
        pac::SIO.gpio_out(0).value_set().write_value(1 << pin);
    } else {
        pac::SIO.gpio_out(0).value_clr().write_value(1 << pin);
    }
}
