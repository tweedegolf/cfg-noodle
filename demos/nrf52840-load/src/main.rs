//! cfg-noodle demo
//!
//! This is a minimal application that exercises the cfg-noodle crate.
//!
//! It can be run using the nrf52840-dk.

#![no_std]
#![no_main]

use cortex_m::asm::bkpt;
use defmt::info;
use embassy_executor::Spawner;
use embassy_nrf::{
    bind_interrupts,
    config::{Config as NrfConfig, HfclkSource},
    gpio::{Level, Output, OutputDrive},
    spim::{self, Spim},
};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer, WithTimeout};
use maitake_sync::WaitQueue;
use mx25r::asynchronous::AsyncMX25R6435F;
use noodle::{alpha, beta, delta, gamma};
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
    let mut config = NrfConfig::default();
    config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(Default::default());

    // Let the debugger attach...
    Timer::after_secs(1).await;
    info!("Starting!");

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

    static STOPPER: WaitQueue = WaitQueue::new();
    let mut interval = Duration::from_millis(500);

    // Spawn all the workers at scattered intervals
    for (node, init) in alpha::NODES.iter().zip(alpha::INITS.iter()) {
        spawner.must_spawn(alpha::alpha_worker(node, *init, interval, &STOPPER));
        interval += Duration::from_millis(49);
    }
    for (node, init) in beta::NODES.iter().zip(beta::INITS.iter()) {
        spawner.must_spawn(beta::beta_worker(node, *init, interval, &STOPPER));
        interval += Duration::from_millis(49);
    }
    for (node, init) in delta::NODES.iter().zip(delta::INITS.iter()) {
        spawner.must_spawn(delta::delta_worker(node, *init, interval, &STOPPER));
        interval += Duration::from_millis(49);
    }
    for (node, init) in gamma::NODES.iter().zip(gamma::INITS.iter()) {
        spawner.must_spawn(gamma::gamma_worker(node, *init, interval, &STOPPER));
        interval += Duration::from_millis(49);
    }
    // WITHOUT epsilon, the write size is JUST under the 4KiB block size. WITH epsilon,
    // it is just over. Uncomment this if you want to see the relative cost of being
    // under/over a single page.
    //
    // for (node, init) in noodle::epsilon::NODES.iter().zip(epsilon::INITS.iter()) {
    //     spawner.must_spawn(epsilon::epsilon_worker(node, *init, interval, &STOPPER));
    //     interval += Duration::from_millis(49);
    // }

    let mut total_reads = 0;
    let mut total_writes = 0;
    let mut total_garbage = 0;
    let mut total_read_time = Duration::from_ticks(0);
    let mut total_write_time = Duration::from_ticks(0);
    let mut total_garbage_time = Duration::from_ticks(0);

    let test_duration = Duration::from_secs(60 * 3);
    let write_interval = Duration::from_secs(15);

    let worker_fut = noodle::worker(
        flash,
        write_interval,
        &mut total_reads,
        &mut total_writes,
        &mut total_garbage,
        &mut total_read_time,
        &mut total_write_time,
        &mut total_garbage_time,
    );

    let _ = worker_fut.with_timeout(test_duration).await;
    STOPPER.close();

    // Wait for all the tasks to stop.
    Timer::after_millis(2000).await;

    defmt::warn!("TEST COMPLETE");
    defmt::warn!("total_reads: {:?}", total_reads);
    defmt::warn!("total_writes: {:?}", total_writes);
    defmt::warn!("total_garbage: {:?}", total_garbage);
    defmt::warn!("total_read_time: {:?}ms", total_read_time.as_millis());
    defmt::warn!("total_write_time: {:?}ms", total_write_time.as_millis());
    defmt::warn!("total_garbage_time: {:?}ms", total_garbage_time.as_millis());

    // Wait for defmt logs to arrive at the host
    Timer::after_millis(2000).await;

    // Exit the program, print a breakpoint
    bkpt();
}
