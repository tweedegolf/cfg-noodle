//! cfg-noodle exercising code
//!
//! This implements a basic firmware that allows for configurable blinking speed
//! of LEDs, with speed changable via user button press. The blinking speed of the
//! LEDs is persistently stored in the cfg-noodle storage in external flash.

use cfg_noodle::{flash::Flash, StorageList, StorageListNode};
use defmt::{error, info};
use embassy_executor::task;
use embassy_futures::select::{select, Either};
use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use mx25r::address::SECTOR_SIZE;
use sequential_storage::cache::PagePointerCache;
use static_cell::ConstStaticCell;

use crate::DkMX25R;

// The external flash is actually 64Mib/8MiB, but we don't necessarily want to use
// the whole thing for storage. As we are using a cache who's size is dependent on the
// number of pages, using the whole flash would cause the cache to be unreasonably large.
//
// This example can be done with no cache (if you need a larger external flash), at the
// cost of some read/write performance loss.
//
// See https://docs.rs/sequential-storage/latest/sequential_storage/#caching for more details.
const TOTAL_SIZE: usize = 128 * 1024;

// Calculate the number of pages (for the cache) using the total size and size of pages
//
// Note: what sequential-storage (and embedded-storage) call(s) "Pages" are actually
// "the granularity that we can erase", which is sectors on our flash part.
const PAGE_COUNT: usize = const { TOTAL_SIZE / (SECTOR_SIZE as usize) };

// Create a static list that will contain all of our nodes.
//
// This is generic over the kind of mutex. If you do not access the list in interrupt
// context, this could probably be relaxed to ThreadModeRawMutex, once
// https://github.com/tosc-rs/scoped-mutex/issues/15 has been fixed.
pub static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();

// These are our storage nodes, each containing their own `BlinkConfig` data.
//
// These could be spread out across multiple crates in practice, they just must all be
// attached to the same `&'static StorageList`. This can be achieved by passing the reference
// to the list when spawning tasks/async functions defined outside of the main binary that
// defines the LIST.
//
// Each node must have a "key" that is unique across all nodes of the list. Namespacing
// and naming conventions are left as an exercise to the user.
pub static LED_ONE_INTERVAL: StorageListNode<BlinkConfig> = StorageListNode::new("led/1");
pub static LED_TWO_INTERVAL: StorageListNode<BlinkConfig> = StorageListNode::new("led/2");
pub static LED_THREE_INTERVAL: StorageListNode<BlinkConfig> = StorageListNode::new("led/3");
pub static LED_FOUR_INTERVAL: StorageListNode<BlinkConfig> = StorageListNode::new("led/4");

// We put our scratch buffer in a `ConstStaticCell` to avoid ever creating it on the stack.
//
// It acts as a static singleton that we can then hold by reference in our I/O worker task.
static BUF: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0u8; 4096]);

// Our storage type, which implements various `minicbor` traits for serialization
// and deserialization.
//
// This type can change in forwards-compatible ways over time, for example adding new
// fields. See https://docs.rs/minicbor-derive/0.17.0/minicbor_derive/index.html for
// details on the syntax and what kind of changes are or are not compatible.
#[derive(Debug, Encode, Decode, Clone, PartialEq, CborLen, defmt::Format)]
pub struct BlinkConfig {
    #[n(0)]
    on_time_ms: u64,
    #[n(1)]
    off_time_ms: u64,
}

// If our configuration is not resident in flash, this value will be used instead,
// and then written back to flash on first boot.
impl Default for BlinkConfig {
    fn default() -> Self {
        Self {
            on_time_ms: 25,
            off_time_ms: 25,
        }
    }
}

#[task(pool_size = 4)]
pub async fn blinker(
    storage: &'static StorageListNode<BlinkConfig>,
    mut led: Output<'static>,
    mut btn: Input<'static>,
) {
    // Attach the node to the list. This call will yield until the the i/o worker
    // processes the request, and populates our node with data (or tells us that
    // the data does not exist in flash).
    //
    // ALL tasks should attach their nodes to the list AS SOON AS POSSIBLE at start up,
    // if a node is not present in the list, it's data will NOT be written to storage
    // whenever writes occur. The I/O worker can only "see" attached nodes.
    //
    // This function only returns an error if there is a duplicate key in the list.
    let hdl = storage.attach(&LIST).await.unwrap();

    // In the future, getting the key and val will not require async/failable functions,
    // see https://github.com/tweedegolf/cfg-noodle/issues/34 tracking this improvement
    let key = hdl.key().await;
    let mut val = hdl.load().await.unwrap();
    defmt::info!("Attached node w/ key: {=str}, val: {:?}", key, val);

    // Remember: data loaded from flash is still "untrusted" data! We should
    // ALWAYS perform validation of values, as data COULD be corrupted but pass
    // consistency checks (extremely unlikely), OR what a "correct" value means
    // across different firmware versions may change (much more likely).
    //
    // This is not specific to cfg-noodle, but always a good reminder!
    let mut written = false;
    if val.on_time_ms < 25 || val.on_time_ms > 1500 {
        defmt::info!("Flooring on time to 25ms");
        val.on_time_ms = 25;
        written = true;
    }
    if val.off_time_ms < 25 || val.off_time_ms > 1500 {
        defmt::info!("Flooring off time to 25ms");
        val.off_time_ms = 25;
        written = true;
    }

    // If we had to change the value, we can write back the new contents
    // to our node.
    //
    // This will yield until we are able to acquire the mutex, which may take
    // some time if the i/o worker is currently writing.
    //
    // When write returns, the data is NOT necessarily written to flash,
    // but it has been stored in our node, which is now marked as "needs writing",
    // and the I/O worker has been signalled that a node needs to be flushed to
    // disk.
    //
    // This only returns an error if there is an internal corruption of the node,
    // which should not be possible. This may be relaxed to not return a result
    // in the future.
    if written {
        hdl.write(&val).await.unwrap();
    }

    loop {
        // We start one future waiting for a button press, and one for blinking.
        //
        // Since the blinking future diverges (returns `!`), we always know that
        // once this select completes, the button has been pressed.
        let Either::First(()) = select(wait_press(&mut btn), blink(&mut led, &val)).await;

        // Update the times, by doubling until we reach a too-high value.
        let new = BlinkConfig {
            on_time_ms: {
                if (val.on_time_ms * 2) > 1500 {
                    25
                } else {
                    val.on_time_ms * 2
                }
            },
            off_time_ms: {
                if (val.off_time_ms * 2) > 1500 {
                    25
                } else {
                    val.off_time_ms * 2
                }
            }
        };

        defmt::info!(
            "Button pressed, changing {:?} -> {:?}",
            val,
            new,
        );
        val = new;

        // As above, this writes to the NODE, which will eventually be flushed
        // back to the disk.
        hdl.write(&val).await.unwrap();
        defmt::info!("...Written!");
    }
}

/// Helper function that waits for a button press. Performs basic
/// debouncing by requiring that the button be held low for at least
/// 2ms before triggering.
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
            }
            Err(_) => {
                // held for more than 2ms
                return;
            }
        }
    }
}

/// Helper function that blinks forever until cancelled.
async fn blink(led: &mut Output<'_>, cfg: &BlinkConfig) -> ! {
    loop {
        led.set_low();
        Timer::after_millis(cfg.on_time_ms).await;
        led.set_high();
        Timer::after_millis(cfg.off_time_ms).await;
    }
}

// This is the I/O worker task. Most users can use the default worker
// task provided by cfg-noodle, but in many cases, users will want
// more fine-grained control of when they perform reads and writes,
// for example doing an immediate write if a shutdown is pending, or
// a battery level falls below some level, or even shutting down
// external flash in between reads/writes to save power.
//
// Basically: we demonstrate here that you CAN fully control when
// I/O operations occur, depending on your use case.
#[task]
pub async fn worker(flash: DkMX25R) {
    // Take the scratch buffer singleton. This is a static cell to avoid
    // putting large data on the stack inside of a future.
    let buf = BUF.take().as_mut_slice();

    // Create our flash object, using the helper type from the `cfg-noodle`
    // crate which is generic over any flash that implements the
    // [`MultiwriteNorFlash`] trait.
    //
    // [`MultiwriteNorFlash`]: https://docs.rs/embedded-storage-async/latest/embedded_storage_async/nor_flash/trait.MultiwriteNorFlash.html
    let mut flash = Flash::new(
        flash,
        0..(TOTAL_SIZE as u32),
        PagePointerCache::<PAGE_COUNT>::new(),
    );

    let mut first_gc_done = false;
    loop {

        // This is a helper future that waits until a node has been attached and
        // has requested a read, but then begins a 100ms debouncing-timer to see
        // if any more nodes are attached. If another node DOES request a read in
        // this window, the timer is reset. If another node DOES NOT request a read,
        // then the read is processed, filling any nodes that have called attach.
        //
        // This debouncing is done to reduce the total number of times we need to
        // do flash reads, to hopefully batch the reading of data for all nodes.
        //
        // The tradeoff here is that the longer we wait, the longer nodes will be
        // stuck waiting for `attach` to complete. The shorter we wait, the more
        // likely that nodes could show up "late", and require a second call to
        // process_reads, which is not terrible, but is undesirable.
        //
        // You likely can fine-tune this to values typical for your operation,
        // based on the time it takes for nodes to all reach their `attach` calls.
        let read_fut = async {
            LIST.needs_read().wait().await.unwrap();

            loop {
                // Make sure we go 100ms with no changes
                let res = LIST
                    .needs_read()
                    .wait()
                    .with_timeout(Duration::from_millis(100))
                    .await;
                if res.is_err() {
                    break;
                }
            }
        };

        // Similarly, this is a helper function that debounces requests to
        // write to flash. Once a node calls `write`, we will be notified,
        // and then see if more nodes request writes within a 1s window.
        //
        // This debouncing avoids cases where many writes are made to one node,
        // or when many nodes are updated around the same time.
        //
        // The tradeoff here is that the longer the debouncing time, the higher
        // the risk of data loss is if we are shut down or panic before completing
        // the write operation. In contrast, the shorter the wait, the more likely we
        // are to perform multiple writes to the flash, which can be slow and consume
        // the limited number of erase/write cycles in our flash.
        //
        // In practice you can likely fine-tune this to values suitable for your
        // lifetime flash durability, and wait MUCH longer than 1s between writes,
        // however it is useful to having this be reasonably short (1s) for the
        // demo so that you can see writes occur.
        let write_fut = async {
            LIST.needs_write().wait().await.unwrap();

            loop {
                // Make sure we go 1s with no changes
                let res = LIST
                    .needs_write()
                    .wait()
                    .with_timeout(Duration::from_millis(1_000))
                    .await;
                if res.is_err() {
                    break;
                }
            }
        };
        info!("worker_task waiting for needs_* signal");

        // Wait for one of the two futures to complete, including their
        // respective debounce times.
        match select(read_fut, write_fut).await {
            Either::First(_) => {
                // Perform the read
                info!("worker task got needs_read signal");
                let start = Instant::now();
                if let Err(e) = LIST.process_reads(&mut flash, buf).await {
                    error!("Error in process_reads: {:?}", e);
                }
                info!("read completed in {:?}", start.elapsed());

                // If this was the FIRST read, we need to perform garbage collection at
                // before handling our first write. This demo schedules this immediately
                // after the first read, though you may choose to defer this later if
                // desired.
                //
                // Garbage collection erases any invalid data in external flash, as well
                // as any data that is now too old. We generally keep the last three valid
                // writes of all data, and erase any data older than that.
                //
                // This may take time to complete, as it may require writing or erasing flash,
                // which is slow. The mutex is not held for the majority of this time, so nodes
                // may still attach and load data while writing/erasing. See
                // `StorageList::process_garbage`'s documentation for more details.
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
            Either::Second(_) => {
                // Perform the write
                info!("worker task got needs_write signal");
                let start = Instant::now();
                if let Err(e) = LIST.process_writes(&mut flash, buf).await {
                    error!("Error in process_writes: {:?}", e);
                }
                info!("write completed in {:?}", start.elapsed());

                // In general, whenever we write, we may need to perform garbage collection
                // before the next write will succeed. In this example, we just always perform
                // garbage collection after writing. See above for more on garbage collection.
                let start = Instant::now();
                if let Err(e) = LIST.process_garbage(&mut flash, buf).await {
                    error!("Error in process_garbage: {:?}", e);
                }
                info!("gc completed in {:?}", start.elapsed());
            }
        }
    }
}

