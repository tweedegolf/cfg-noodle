use cfg_noodle::{flash::Flash, StorageList, StorageListNode};
use defmt::{error, info};
use embassy_executor::task;
use embassy_futures::select::{select, Either};
use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use mx25r::address::PAGE_SIZE;
use sequential_storage::cache::PagePointerCache;
use static_cell::ConstStaticCell;

use crate::DkMX25R;


const TOTAL_SIZE: usize = 128 * 1024;
const PAGE_COUNT: usize = const { TOTAL_SIZE / (PAGE_SIZE as usize) };
//     pub flash: DkMX25R,
//     pub cache: PagePointerCache<PAGE_COUNT>,

pub static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
pub static LED_ONE_INTERVAL: StorageListNode<u64> = StorageListNode::new("led/1");
pub static LED_TWO_INTERVAL: StorageListNode<u64> = StorageListNode::new("led/2");
pub static LED_THREE_INTERVAL: StorageListNode<u64> = StorageListNode::new("led/3");
pub static LED_FOUR_INTERVAL: StorageListNode<u64> = StorageListNode::new("led/4");

static BUF: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0u8; 4096]);

#[task]
pub async fn worker(flash: DkMX25R) {
    let mut buf = BUF.take().as_mut_slice();

    let mut flash = Flash::new(
        flash,
        0..(TOTAL_SIZE as u32),
        PagePointerCache::<PAGE_COUNT>::new(),
    );

    let mut first_gc_done = false;
    loop {
        let read_fut = async {
            LIST.needs_read().wait().await.unwrap();

            loop {
                // Make sure we go 100ms with no changes
                let res = LIST.needs_read().wait().with_timeout(Duration::from_millis(100)).await;
                if res.is_err() {
                    break;
                }
            }
        };
        let write_fut = async {
            LIST.needs_write().wait().await.unwrap();

            loop {
                // Make sure we go 10s with no changes
                let res = LIST.needs_write().wait().with_timeout(Duration::from_millis(1_000)).await;
                if res.is_err() {
                    break;
                }
            }
        };
        info!("worker_task waiting for needs_* signal");
        match embassy_futures::select::select(read_fut, write_fut)
            .await
        {
            embassy_futures::select::Either::First(_) => {
                info!("worker task got needs_read signal");
                let start = Instant::now();
                if let Err(e) = LIST.process_reads(&mut flash, buf).await {
                    error!("Error in process_reads: {:?}", e);
                }
                info!("read completed in {:?}", start.elapsed());
                if !first_gc_done {
                    let start = Instant::now();
                    if let Err(e) = LIST.process_garbage(&mut flash, buf).await {
                        error!("Error in process_garbage: {:?}", e);
                    } else {
                        first_gc_done = true;
                    }
                    info!("(first) gc completed in {:?}", start.elapsed());
                }
            }
            embassy_futures::select::Either::Second(_) => {
                info!("worker task got needs_write signal");
                let start = Instant::now();
                if let Err(e) = LIST.process_writes(&mut flash, buf).await {
                    error!("Error in process_writes: {:?}", e);
                }
                info!("write completed in {:?}", start.elapsed());

                let start = Instant::now();
                if let Err(e) = LIST.process_garbage(&mut flash, buf).await {
                    error!("Error in process_garbage: {:?}", e);
                }
                info!("gc completed in {:?}", start.elapsed());
            }
        }
    }
}

#[task(pool_size = 4)]
pub async fn blinker(
    storage: &'static StorageListNode<u64>,
    mut led: Output<'static>,
    mut btn: Input<'static>,
) {
    let hdl = storage.attach(&LIST).await.unwrap();
    let key = hdl.key().await;
    let mut val = hdl.load().await.unwrap();
    defmt::info!("Attached node w/ key: {=str}, val: {=u64}", key, val);

    if val < 25 {
        defmt::info!("Flooring to 25ms");
        val = 25;
        hdl.write(&val).await.unwrap();
    }

    loop {
        let Either::First(()) = select(wait_press(&mut btn), blink(&mut led, val)).await;
        let mut new = val * 2;
        if new > 1500 {
            new = 25;
        }

        defmt::info!("Button pressed, changing interval {=u64} -> {=u64}", val, new);
        val = new;
        hdl.write(&val).await.unwrap();
        defmt::info!("...Written!");
    }
}

async fn wait_press(btn: &mut Input<'_>) {
    loop {
        btn.wait_for_high().await;
        btn.wait_for_low().await;
        defmt::info!("PRESS");
        let res = btn
            .wait_for_high()
            .with_timeout(Duration::from_millis(2))
            .await;
        match res {
            Ok(_) => {
                // button pressed too short, ignore
            },
            Err(_) => {
                // held for more than 2ms
                return
            }
        }
    }
}

async fn blink(led: &mut Output<'_>, millis: u64) -> ! {
    loop {
        led.set_low();
        Timer::after_millis(millis).await;
        led.set_high();
        Timer::after_millis(millis).await;
    }
}
