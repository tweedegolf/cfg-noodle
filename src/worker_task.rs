//! Default worker task implementation

use core::fmt::Debug;
use embassy_futures::select::{Either, select};
use embassy_time::Duration;
use mutex_traits::ScopedRawMutex;

use crate::{
    NdlDataStorage, StorageList,
    error::{Error, LoadStoreError},
    logging::{error, info, warn},
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
pub async fn default_worker_task<R: ScopedRawMutex + Sync, S: NdlDataStorage, T>(
    list: &'static StorageList<R>,
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

        // Wait for either the needs_read or needs_write to be signaled
        loop {
            let read_fut = async {
                'debounce: loop {
                    // Make sure we go 100ms with no changes
                    if embassy_time::with_timeout(Duration::from_millis(100), list.needs_read())
                        .await
                        .is_err()
                    {
                        // Error means timeout reached, so no more debouncing
                        break 'debounce;
                    }
                }
            };
            let write_fut = async {
                'debounce: loop {
                    // Make sure we go 10s with no changes
                    if embassy_time::with_timeout(Duration::from_secs(10), list.needs_write())
                        .await
                        .is_err()
                    {
                        // Error means timeout reached, so no more debouncing
                        break 'debounce;
                    }
                }
            };

            info!("worker_task waiting for signals");
            match embassy_futures::select::select(read_fut, write_fut).await {
                // needs_read signaled
                Either::First(_) => {
                    info!("worker task got needs_read signal, processing reads");

                    if let Err(e) = list.process_reads(&mut flash, buf).await {
                        error!("Error in process_reads: {:?}", e);
                    }

                    // On the first run (i.e., after starup) we want to run garbage collection once to
                    // have everything in a clean state.
                    // Delay this by 120 seconds to give other tasks time to do work before we potentially
                    // block the for a longer time with garbage collection.
                    if !first_gc_done {
                        info!(
                            "worker task finished process_reads. Waiting to trigger process_garbage"
                        );
                        embassy_time::Timer::after_secs(120).await;

                        if let Err(e) = list.process_garbage(&mut flash, buf).await {
                            error!("Error in process_garbage: {:?}", e);
                        } else {
                            first_gc_done = true;
                        }
                    }
                }
                // needs_write signaled
                Either::Second(_) => {
                    info!("worker task got needs_write signal, processing writes");
                    if let Err(e) = list.process_writes(&mut flash, buf).await {
                        error!("Error in process_writes: {:?}", e);
                    }

                    info!("worker task finished process_writes, triggering process_garbage");
                    if let Err(e) = list.process_garbage(&mut flash, buf).await {
                        error!("Error in process_garbage: {:?}", e);
                    }
                }
            };
        }
    };

    // Top-level select that awaits the waker future and the stopper future so that whenever the
    // stopper future finishes, we do a clean "shutdown" of the worker task.
    match select(waker_future, stopper).await {
        // One of the needs_* signals
        Either::First(_) => (),
        // The stopper future
        Either::Second(_) => match list.process_writes(&mut flash, buf).await {
            Ok(()) => (),
            Err(LoadStoreError::AppError(Error::NeedsGarbageCollect)) => {
                if let Err(e) = list.process_garbage(&mut flash, buf).await {
                    error!("Error in process_garbage: {:?}", e);
                }
                if let Err(e) = list.process_writes(&mut flash, buf).await {
                    error!("Error in process_writes: {:?}", e);
                }
            }
            Err(LoadStoreError::AppError(Error::NeedsFirstRead)) => {
                warn!("closing worker without performing first read");
            }
            Err(e) => error!("Error in process_writes: {:?}", e),
        },
    }
}
