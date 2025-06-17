//! Default worker task implementation

use core::fmt::Debug;
use embassy_futures::select::{Either, select};
use embassy_time::Duration;
use mutex::ScopedRawMutex;

use crate::{
    NdlDataStorage, StorageList,
    error::{Error, LoadStoreError},
    logging::{error, info},
};

/// A default worker task
pub async fn default_worker_task<R: ScopedRawMutex + Sync, S: NdlDataStorage, T>(
    list: &'static StorageList<R>,
    mut flash: S,
    stopper: impl Future<Output = T>,
) where
    S::Error: Debug,
{
    // After PR #37 lands:
    //let mut buf = [0u8; S::MAX_ELEM_SIZE];

    let mut buf = [0u8; 4096];
    let waker_future = async {
        let mut first_gc_done = false;
        loop {
            let read_fut = async {
                loop {
                    // Make sure we go 100ms with no changes
                    if embassy_time::with_timeout(
                        Duration::from_millis(100),
                        list.needs_read().wait(),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
            };
            let write_fut = async {
                loop {
                    // Make sure we go 10s with no changes
                    if embassy_time::with_timeout(
                        Duration::from_millis(10_000),
                        list.needs_write().wait(),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
            };
            info!("worker_task waiting for signals");
            match embassy_futures::select::select(read_fut, write_fut).await {
                // needs_read signalled
                Either::First(_) => {
                    info!("worker task got needs_read signal. Waiting to trigger process_read");
                    embassy_time::Timer::after_millis(100).await;

                    if let Err(e) = list.process_reads(&mut flash, &mut buf).await {
                        error!("Error in process_reads: {:?}", e);
                    }
                    if !first_gc_done {
                        info!(
                            "worker task finished process_reads. Waiting to trigger process_garbage"
                        );
                        embassy_time::Timer::after_secs(120).await;

                        if let Err(e) = list.process_garbage(&mut flash, &mut buf).await {
                            error!("Error in process_garbage: {:?}", e);
                        } else {
                            first_gc_done = true;
                        }
                    }
                }
                // needs_write signalled
                Either::Second(_) => {
                    info!("worker task got needs_write signal. Waiting to trigger process_write");
                    embassy_time::Timer::after_millis(100).await;

                    if let Err(e) = list.process_writes(&mut flash, &mut buf).await {
                        error!("Error in process_writes: {:?}", e);
                    }

                    if let Err(e) = list.process_garbage(&mut flash, &mut buf).await {
                        error!("Error in process_garbage: {:?}", e);
                    }
                }
            };
        }
    };

    match select(waker_future, stopper).await {
        // One of the needs_* signals
        Either::First(_) => (),
        // The stopper future
        Either::Second(_) => match list.process_writes(&mut flash, &mut buf).await {
            Ok(()) => (),
            Err(LoadStoreError::AppError(Error::NeedsGarbageCollect)) => {
                if let Err(e) = list.process_garbage(&mut flash, &mut buf).await {
                    error!("Error in process_garbage: {:?}", e);
                }
                if let Err(e) = list.process_writes(&mut flash, &mut buf).await {
                    error!("Error in process_writes: {:?}", e);
                }
            }
            Err(LoadStoreError::AppError(Error::NeedsFirstRead)) => {
                todo!("What should happen then?")
            }
            Err(e) => error!("Error in process_writes: {:?}", e),
        },
    }
}
