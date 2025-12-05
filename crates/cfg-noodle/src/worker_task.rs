//! Default worker task implementation

use core::fmt::Debug;
use embassy_futures::select::{Either, Either3, select};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use embassy_time::Duration;
use mutex_traits::ScopedRawMutex;

use crate::{
    NdlDataStorage, StorageList,
    error::{Error, LoadStoreError},
    flash::empty::AlwaysEmptyFlash,
    logging::{debug, error, info, warn},
};

/// A default worker task that manages storage operations based on read/write signals.
///
/// This worker task continuously monitors for read and write signals from the storage list
/// and performs the appropriate operations when triggered. It implements a debouncing mechanism
/// to avoid excessive operations when multiple signals occur in quick succession.
///
/// This implementation may be used when very basic logic is sufficient to handle reading and writing.
/// If more sophisticated logic is required, you can implement your own worker task and trigger
/// reads and writes as appropriate for your application.
///
/// # Arguments
///
/// * `list` - A static reference to the storage list
/// * `flash` - The flash storage implementation used for persistent storage
/// * `stopper` - When this future completes, an immediate write is triggered and the worker task returns.
///   Usually this is some sort of signal's or waiter's `wait()` function
///
/// # Behavior
///
/// The task operates in a loop, waiting for either:
/// - Read signals: Triggers after 100ms of no new read signals, then processes reads.
///   After the first read operation, it also performs garbage collection after a 120-second delay.
/// - Write signals: Triggers after 10 seconds of no new write signals, then processes writes
///   followed by garbage collection.
///
/// When the stopper future completes, the task attempts to flush any pending writes before
/// terminating. If garbage collection is needed during shutdown, it will be performed automatically.
///
/// # Errors
///
/// Errors during storage operations are logged but do not cause the task to terminate.
/// The task will continue processing subsequent signals even if individual operations fail.
pub async fn default_worker_task<R: ScopedRawMutex + Sync, S: NdlDataStorage, T, const KEPT_RECORDS: usize>(
    list: &'static StorageList<R, KEPT_RECORDS>,
    mut flash: S,
    stopper: impl Future<Output = T>,
    buf: &mut [u8],
) where
    S::Error: Debug,
{
    // Create one future for the waker signals so that we can `select` between this
    // future and the `stopper` future.
    let waker_future = async {
        let mut first_gc_done = false;
        let needs_gc: Signal<NoopRawMutex, Duration> = Signal::new();

        // Wait for either the needs_read or needs_write to be signaled
        loop {
            let read_fut = async {
                // Wait for needs_read to be signaled once
                debug!("worker_task waiting for needs_read signal");
                list.needs_read().await;

                // Run debounce loop that breaks when needs_read hasn't been signaled
                // for more than 100ms
                'debounce: loop {
                    // Make sure we go 100ms with no changes
                    debug!("Debounce needs_read with 100ms timeout");
                    if embassy_time::with_timeout(Duration::from_millis(100), list.needs_read())
                        .await
                        .is_err()
                    {
                        // Error means timeout reached, so no more debouncing
                        debug!("Timeout reached, stop debouncing needs_read");
                        break 'debounce;
                    }
                }
            };
            let write_fut = async {
                // Wait for needs_read to be signaled once
                debug!("worker_task waiting for needs_write signal");
                list.needs_write().await;

                // Run debounce loop that breaks when needs_write hasn't been signaled
                // for more than 100ms
                'debounce: loop {
                    debug!("Debounce needs_write with 10s timeout");
                    // Make sure we go 10s with no changes
                    if embassy_time::with_timeout(Duration::from_secs(10), list.needs_write())
                        .await
                        .is_err()
                    {
                        // Error means timeout reached, so no more debouncing
                        debug!("Timeout reached, stop debouncing needs_write");
                        break 'debounce;
                    }
                }
            };

            let gc_fut = async {
                let delay = needs_gc.wait().await;
                debug!("Got needs_gc signal. Continuing after delay of {}", delay);
                embassy_time::Timer::after(delay).await;
            };

            info!("worker_task waiting for signals");
            match embassy_futures::select::select3(read_fut, write_fut, gc_fut).await {
                // needs_read signaled
                Either3::First(_) => {
                    info!("worker task got needs_read signal, processing reads");

                    match list.process_reads(&mut flash, buf).await {
                        Ok(rpt) => info!("process_reads success: {:?}", rpt),
                        Err(e) => error!("Error in process_reads: {:?}", e),
                    }

                    // On the first run (i.e., after starup) we want to run garbage collection once to
                    // have everything in a clean state.
                    // Delay this by 120 seconds to give other tasks time to do work before we potentially
                    // block the for a longer time with garbage collection.
                    if !first_gc_done {
                        info!(
                            "worker task finished process_reads. Trigger process_garbage with delay"
                        );

                        needs_gc.signal(Duration::from_secs(120));
                    }
                }
                // needs_write signaled
                Either3::Second(_) => {
                    info!("worker task got needs_write signal, first process_garbage then write");

                    match list.process_garbage(&mut flash, buf).await {
                        Ok(rpt) => info!("process_garbage success: {:?}", rpt),
                        Err(e) => error!("Error in process_garbage: {:?}", e),
                    }
                    match list.process_writes(&mut flash, buf).await {
                        Ok(rpt) => info!("process_writes success: {:?}", rpt),
                        Err(e) => error!("Error in process_writes: {:?}", e),
                    }

                    info!("worker task finished process_writes, triggering process_garbage");
                    match list.process_garbage(&mut flash, buf).await {
                        Ok(rpt) => info!("process_garbage success: {:?}", rpt),
                        Err(e) => error!("Error in process_garbage: {:?}", e),
                    }
                }
                Either3::Third(_) => {
                    info!("worker task got needs_gc signal, run process_garbage");
                    match list.process_garbage(&mut flash, buf).await {
                        Ok(rpt) => {
                            info!("process_garbage success: {:?}", rpt);
                            first_gc_done = true;
                        }
                        Err(e) => error!("Error in process_garbage: {:?}", e),
                    }
                }
            };
        }
    };

    // Top-level select that awaits the waker future and the stopper future so that whenever the
    // stopper future finishes, we do a clean "shutdown" of the worker task.
    //
    // The waker future diverges, so it can never occur.
    let Either::Second(_) = select(waker_future, stopper).await;

    info!("stopper future completed, stopping worker task");
    match list.process_writes(&mut flash, buf).await {
        Ok(rpt) => info!("process_writes success: {:?}", rpt),
        Err(LoadStoreError::AppError(Error::NeedsGarbageCollect)) => {
            match list.process_garbage(&mut flash, buf).await {
                Ok(rpt) => info!("process_garbage success: {:?}", rpt),
                Err(e) => error!("Error in process_garbage: {:?}", e),
            }
            match list.process_writes(&mut flash, buf).await {
                Ok(rpt) => info!("process_writes success: {:?}", rpt),
                Err(e) => error!("Error in process_writes: {:?}", e),
            }
        }
        Err(LoadStoreError::AppError(Error::NeedsFirstRead)) => {
            warn!("closing worker without performing first read");
        }
        Err(e) => error!("Error in process_writes: {:?}", e),
    }

    info!("worker task stopped!");
}

/// A worker tasks that always reads no entries and ignores all writes
///
/// This is useful in testing to always start out with an empty flash.
///
/// The no-op worker task ONLY serves `process_reads`, not `process_writes` or
/// `process_garbage`: these would fail when attempting to verify with read-back.
pub async fn no_op_worker_task<R: ScopedRawMutex + Sync, const KEPT_RECORDS: usize>(list: &'static StorageList<R, KEPT_RECORDS>) -> ! {
    loop {
        list.needs_read().await;
        list.process_reads(&mut AlwaysEmptyFlash, &mut [])
            .await
            .expect("AlwaysEmptyFlash never fails");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageListNode;
    use mutex::raw_impls::cs::CriticalSectionRawMutex;

    #[expect(
        clippy::unwrap_used,
        reason = "Only unwrapping flash errors which can never happen"
    )]
    #[test_log::test(tokio::test)]
    async fn no_op_worker_never_fails_and_ignores_all_writes() {
        static LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static NODE: StorageListNode<u64> = StorageListNode::new("config/node");

        let test = async {
            let mut handle = NODE.attach(&LIST).await.unwrap();

            // On start-up we read the default
            assert_eq!(handle.load(), 0);

            // Writing locally works
            handle.write(&42).await.unwrap();
            assert_eq!(handle.load(), 42);

            // But data will not be written to flash
            drop(handle);
            NODE.detach(&LIST).await.unwrap();

            // So when we re-attach, we get back the default value
            let handle = NODE.attach(&LIST).await.unwrap();
            assert_eq!(handle.load(), 0);
        };

        select(test, no_op_worker_task(&LIST)).await;
    }
}
