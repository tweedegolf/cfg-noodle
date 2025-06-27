//! cfg-noodle exercising code
//!
//! This implements a basic firmware that allows for configurable blinking speed
//! of LEDs, with speed changable via user button press. The blinking speed of the
//! LEDs is persistently stored in the cfg-noodle storage in external flash.

use cfg_noodle::{flash::Flash, StorageList};
use defmt::{error, info};
use embassy_executor::task;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, WithTimeout};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use mx25r::address::SECTOR_SIZE;
use sequential_storage::cache::PagePointerCache;
use static_cell::ConstStaticCell;

use crate::DkMX25R;
pub mod alpha;
pub mod beta;
pub mod gamma;
pub mod delta;
pub mod epsilon;

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

// We put our scratch buffer in a `ConstStaticCell` to avoid ever creating it on the stack.
//
// It acts as a static singleton that we can then hold by reference in our I/O worker task.
static BUF: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0u8; 4096]);

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
            LIST.needs_read().await;

            loop {
                // Make sure we go 100ms with no changes
                let res = LIST
                    .needs_read()
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
            LIST.needs_write().await;

            loop {
                // Make sure we go 1s with no changes
                let res = LIST
                    .needs_write()
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
                match LIST.process_reads(&mut flash, buf).await {
                    Ok(rpt) => info!("process_reads success: {:?}", rpt),
                    Err(e) => error!("Error in process_reads: {:?}", e),
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
                    match LIST.process_garbage(&mut flash, buf).await {
                        Ok(rpt) => {
                            info!("process_garbage success: {:?}", rpt);
                            first_gc_done = true;
                        }
                        Err(e) => error!("Error in process_garbage: {:?}", e),
                    }
                    info!("(first) gc completed in {:?}", start.elapsed());
                }
            }
            Either::Second(_) => {
                // Perform the write
                info!("worker task got needs_write signal");
                let start = Instant::now();
                match LIST.process_writes(&mut flash, buf).await {
                    Ok(rpt) => info!("process_writes success: {:?}", rpt),
                    Err(e) => error!("Error in process_writes: {:?}", e),
                }
                info!("write completed in {:?}", start.elapsed());

                // In general, whenever we write, we may need to perform garbage collection
                // before the next write will succeed. In this example, we just always perform
                // garbage collection after writing. See above for more on garbage collection.
                let start = Instant::now();
                match LIST.process_garbage(&mut flash, buf).await {
                    Ok(rpt) => info!("process_garbage success: {:?}", rpt),
                    Err(e) => error!("Error in process_garbage: {:?}", e),
                }
                info!("gc completed in {:?}", start.elapsed());
            }
        }
    }
}
