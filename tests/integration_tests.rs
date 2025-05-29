use cfg_noodle::intrusive::{Flash, StorageList, StorageListNode};
use log::{error, info};
use minicbor::{CborLen, Decode, Encode};
use mutex::{ScopedRawMutex, raw_impls::cs::CriticalSectionRawMutex};
use sequential_storage::cache::NoCache;
use sequential_storage::mock_flash::{MockFlashBase, WriteCountCheck};
use sequential_storage::queue;
use test_log::test;
use tokio::sync::watch::{self, Receiver};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

// Scratch buffer size
const BUF_LEN: usize = 1024;

#[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
struct TestConfig {
    #[n(0)]
    value: u32,
    #[n(1)]
    truth: bool,
    #[n(2)]
    optional_truth: Option<bool>,
}

#[derive(Debug, Default, Encode, Decode, Clone, CborLen)]
struct SimpleConfig {
    #[n(0)]
    data: u8,
}

fn get_test_flash() -> Flash<MockFlashBase<10, 16, 256>> {
    let mut flash = MockFlashBase::<10, 16, 256>::new(WriteCountCheck::Twice, None, true);
    flash.alignment_check = false;
    Flash::new(flash, 0x0000..0x1000)
}

fn worker_task<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    mut flash: Flash<MockFlashBase<10, 16, 256>>,
    mut kill_signal: Option<Receiver<()>>,
) -> JoinHandle<Flash<MockFlashBase<10, 16, 256>>> {
    // Remove the default value
    if let Some(rx) = kill_signal.as_mut() {
        rx.borrow_and_update();
    }

    let mut read_buf = [0u8; BUF_LEN];
    let mut serde_buf = [0u8; BUF_LEN];

    tokio::task::spawn(async move {
        loop {
            info!("worker_task waiting for needs_* signal");
            match embassy_futures::select::select3(
                list.needs_read().wait(),
                list.needs_write().wait(),
                async {
                    if let Some(signal) = kill_signal.as_mut() {
                        signal.changed().await
                    } else {
                        sleep(Duration::from_secs(10)).await;
                        Ok(())
                    }
                },
            )
            .await
            {
                embassy_futures::select::Either3::First(_) => {
                    info!("worker task got needs_read signal");
                    list.process_reads(&mut flash, &mut read_buf).await;
                }
                embassy_futures::select::Either3::Second(_) => {
                    info!("worker task got needs_write signal");
                    if let Err(e) = list
                        .process_writes(&mut flash, &mut read_buf, &mut serde_buf)
                        .await
                    {
                        error!("Error in process_writes: {}", e);
                    }
                }
                embassy_futures::select::Either3::Third(_) => return flash,
            }
        }
    })
}

#[test(tokio::test)]
async fn test_read_from_empty_flash() {
    info!("Starting test");
    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config");

    // Create flash but do not populate with any data
    let flash = get_test_flash();

    info!("Spawning worker task");
    let _worker = worker_task(&LIST, flash, None);

    let handle = NODE.attach(&LIST).await.unwrap();

    // Should return default value
    let config = handle.load().await.unwrap();
    let default_config = TestConfig::default();
    assert_eq!(
        config, default_config,
        "Loaded config should match default config"
    );
}

#[test(tokio::test)]
async fn test_read_clean_state() {
    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config");

    let flash = get_test_flash();
    info!("Spawning worker task");
    let (tx, rx) = watch::channel(());
    let worker = worker_task(&LIST, flash, Some(rx));

    let handle = NODE.attach(&LIST).await.unwrap();

    let test_config = TestConfig {
        value: 42,
        truth: false,
        optional_truth: Some(true),
    };
    handle.write(&test_config).await.unwrap();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Stop worker task and save the flash data it produced
    tx.send(()).unwrap();
    let mut flash = worker.await.unwrap();

    // Create new list and node to simulate restart
    static LIST2: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config");

    info!(
        "Spawning new worker with flash content: {}",
        flash.flash().print_items().await
    );
    // Spawn worker task with the used flash
    let _worker = worker_task(&LIST2, flash, None);

    let handle2 = NODE2.attach(&LIST2).await.unwrap();

    let loaded_config = handle2.load().await.unwrap();
    assert_eq!(
        loaded_config, test_config,
        "Loaded config should match default config"
    );
}

#[test(tokio::test)]
async fn test_multiple_writes() {
    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config");

    let flash = get_test_flash();

    info!("Spawning worker task");
    let (tx, rx) = watch::channel(());
    let worker = worker_task(&LIST, flash, Some(rx));

    let handle = NODE.attach(&LIST).await.unwrap();

    let test_config = TestConfig {
        value: 42,
        truth: false,
        optional_truth: Some(true),
    };
    handle.write(&test_config).await.unwrap();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Write again
    handle.write(&test_config).await.unwrap();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Stop worker task and save the flash data it produced
    tx.send(()).unwrap();
    let mut flash = worker.await.unwrap();

    info!(
        "Spawning new worker with flash content: {}",
        flash.flash().print_items().await
    );

    // Iterate over the flash and count the number of items
    let range = flash.range();
    let mut cache = NoCache::new();
    let mut iter = queue::iter(flash.flash(), range, &mut cache).await.unwrap();
    let mut item_counter = 0;
    while iter.next(&mut [0u8; BUF_LEN]).await.unwrap().is_some() {
        item_counter += 1;
    }
    info!("Found {} items in flash (expecting 2).", item_counter);
    assert_eq!(
        item_counter, 2,
        "expected 2 items in the flash (one node and one write_confirm)"
    )
}
