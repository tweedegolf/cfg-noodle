#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_nrf::{
    bind_interrupts,
    config::{Config as NrfConfig, HfclkSource},
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

pub type SpimInstance = Spim<'static, embassy_nrf::peripherals::TWISPI0>;
pub type SpiDev = SpiDevice<'static, ThreadModeRawMutex, SpimInstance, Output<'static>>;
pub type DkMX25R = AsyncMX25R6435F<SpiDev>;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // SYSTEM INIT
    info!("Start");
    let mut config = NrfConfig::default();
    config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(Default::default());

    // External flash init
    let mut spim_cfg = spim::Config::default();
    spim_cfg.frequency = spim::Frequency::M1;
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
    let mut reset = Output::new(p.P0_23, Level::Low, OutputDrive::Standard);
    Timer::after_millis(10).await;
    reset.set_high();
    Timer::after_millis(10).await;
    // let flash = DkMX25R::new(spi);

    let mut flash = DkMX25R::new(spi);

    let status = flash.read_status().await.unwrap();
    defmt::warn!("Status: {:?}", status);

    // defmt::warn!("Waiting not busy...");
    // flash.wait_wip().await.unwrap();
    // defmt::warn!("Erasing chip...");
    // flash.erase_chip().await.unwrap();
    // defmt::warn!("Waiting not busy...");
    // flash.wait_wip().await.unwrap();
    // defmt::warn!("Chip erase complete!");

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
    spawner.must_spawn(noodle::worker(flash));

    // Begin running!
    loop {
        Timer::after_millis(10_000).await;
    }
}
