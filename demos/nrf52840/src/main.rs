//! cfg-noodle demo
//!
//! This is a minimal application that exercises the cfg-noodle crate.
//!
//! It can be run using the nrf52840-dk.

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_nrf::{
    bind_interrupts,
    gpio::{Input, Level, Output, OutputDrive, Pull},
    spim::{self, Spim},
};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, mutex::Mutex};
use embassy_time::Timer;
use mx25r::asynchronous::AsyncMX25R6435F;
use static_cell::StaticCell;

bind_interrupts!(pub struct Irqs {
    TWISPI0 => spim::InterruptHandler<embassy_nrf::peripherals::TWISPI0>;
});

use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use {defmt_rtt as _, panic_probe as _};

pub mod noodle;

// Helper types for the SPI, SpiDevice impl, and specific flash part we
// are using. These are all specific to the nrf52840-dk.
pub type SpimInstance = Spim<'static, embassy_nrf::peripherals::TWISPI0>;
pub type SpiDev = SpiDevice<'static, ThreadModeRawMutex, SpimInstance, Output<'static>>;
pub type DkMX25R = AsyncMX25R6435F<SpiDev>;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // SYSTEM INIT
    info!("Start");
    let p = embassy_nrf::init(Default::default());

    // External flash init
    //
    // We only use single-spi for simplicity. Note that SPI frequency is limited to
    // 8MHz instead of the 840's 32MHz due to various errata that can cause data corruption
    // at higher speeds.
    let mut spim_cfg = spim::Config::default();
    spim_cfg.frequency = spim::Frequency::M8;
    spim_cfg.mode = spim::MODE_0;
    spim_cfg.sck_drive = OutputDrive::Standard;
    spim_cfg.mosi_drive = OutputDrive::Standard;

    static SPI: StaticCell<Mutex<ThreadModeRawMutex, SpimInstance>> = StaticCell::new();
    let spi = SPI.init_with(|| {
        Mutex::new(Spim::new(
            p.TWISPI0, Irqs, p.P0_19, p.P0_21, p.P0_20, spim_cfg,
        ))
    });
    let cs = Output::new(p.P0_17, Level::High, OutputDrive::Standard);
    let spi = SpiDevice::new(spi, cs);
    let _ = Output::new(p.P0_22, Level::High, OutputDrive::Standard);

    // Do a hardware reset of the external flash part
    //
    // This is not typically needed, but useful for testing
    let mut reset = Output::new(p.P0_23, Level::Low, OutputDrive::Standard);
    Timer::after_millis(10).await;
    reset.set_high();
    Timer::after_millis(10).await;
    // forget the reset pin so it stays high forever (to avoid resetting the part).
    core::mem::forget(reset);

    let mut flash = DkMX25R::new(spi);

    let status = flash.read_status().await.unwrap();
    defmt::warn!("Status: {:?}", status);

    // You can uncomment this if you want to explicitly do a full erase of flash.
    //
    // This takes about 2 minutes to complete.
    //
    // defmt::warn!("Waiting not busy...");
    // flash.wait_wip().await.unwrap();
    // defmt::warn!("Erasing chip...");
    // flash.erase_chip().await.unwrap();
    // defmt::warn!("Waiting not busy...");
    // flash.wait_wip().await.unwrap();
    // defmt::warn!("Chip erase complete!");

    // Spawn each of the four worker tasks. They are all the same async function,
    // but are given their own LEDs and buttons that correspond with each other
    // on the nRF52840-dk (e.g. the top left button will correspond to the top left
    // LED). All buttons and LEDs are active-low.
    spawner.must_spawn(noodle::blinker(
        &noodle::LED_ONE_INTERVAL,
        Output::new(p.P0_13, Level::High, OutputDrive::Standard),
        Input::new(p.P0_11, Pull::Up),
    ));
    spawner.must_spawn(noodle::blinker(
        &noodle::LED_TWO_INTERVAL,
        Output::new(p.P0_14, Level::High, OutputDrive::Standard),
        Input::new(p.P0_12, Pull::Up),
    ));
    spawner.must_spawn(noodle::blinker(
        &noodle::LED_THREE_INTERVAL,
        Output::new(p.P0_15, Level::High, OutputDrive::Standard),
        Input::new(p.P0_24, Pull::Up),
    ));
    spawner.must_spawn(noodle::blinker(
        &noodle::LED_FOUR_INTERVAL,
        Output::new(p.P0_16, Level::High, OutputDrive::Standard),
        Input::new(p.P0_25, Pull::Up),
    ));

    // Spawn the I/O worker task that serves requests from the nodes to
    // read, write, and garbage collect old data.
    spawner.must_spawn(noodle::worker(flash));

    // Tasks continue running after main returns.
}
