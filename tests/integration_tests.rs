use cfg_noodle::intrusive::{Flash, StorageList, StorageListNode};
use log::{error, info, warn};
use minicbor::{CborLen, Decode, Encode};
use mutex::{ScopedRawMutex, raw_impls::cs::CriticalSectionRawMutex};
use sequential_storage::{
    cache::NoCache,
    mock_flash::{MockFlashBase, WriteCountCheck},
    queue,
};
use test_log::test;
use tokio::{
    sync::watch::{self, Receiver},
    task::JoinHandle,
    time::{Duration, sleep},
};

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
/// Creates a test flash instance with mock storage for testing purposes.
fn get_test_flash() -> Flash<MockFlashBase<10, 16, 256>> {
    let mut flash = MockFlashBase::<10, 16, 256>::new(WriteCountCheck::Twice, None, true);
    flash.alignment_check = false;
    Flash::new(flash, 0x0000..0x1000)
}

/// Spawns an asynchronous worker task that manages storage operations for a storage list.
///
/// This function creates a background task that continuously monitors for read and write requests
/// from the storage list and processes them against the provided flash storage. The worker task
/// runs in a loop, waiting for signals from the storage list and handling them appropriately.
///
/// # Parameters
///
/// * `list` - A static reference to the storage list that manages configuration nodes
/// * `flash` - The flash storage instance used for persistent storage operations
/// * `kill_signal` - An optional receiver for graceful task termination. If None, the task
///   will use a timeout-based approach for periodic checks (10 seconds)
///
/// # Returns
///
/// Returns a `JoinHandle` that resolves to the flash instance when the worker task terminates.
/// This allows the caller to recover the flash storage for reuse after task shutdown.
///
/// # Behavior
///
/// The worker task continuously waits for one of three events:
/// 1. Read requests from the storage list
/// 2. Write requests from the storage list
/// 3. Termination signal or timeout
///
/// When a read or write request is received, the task processes it using the appropriate
/// storage list method. The task terminates gracefully when a kill signal is received,
/// returning the flash instance to the caller.
fn worker_task<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    mut flash: Flash<MockFlashBase<10, 16, 256>>,
    mut kill_signal: Option<Receiver<()>>,
) -> JoinHandle<Flash<MockFlashBase<10, 16, 256>>> {
    // Clear any pending default value from the kill signal receiver to ensure
    // we only respond to new termination signals sent after task startup
    if let Some(rx) = kill_signal.as_mut() {
        rx.borrow_and_update();
    }

    // Allocate buffers for storage operations - these are reused across operations
    // to avoid repeated allocations in the async task loop
    let mut read_buf = [0u8; BUF_LEN]; // Buffer for reading data from flash
    let mut serde_buf = [0u8; BUF_LEN]; // Buffer for serialization/deserialization

    tokio::task::spawn(async move {
        loop {
            info!("worker_task waiting for needs_* signal");

            // Wait for one of three possible events using embassy's select3:
            // 1. Storage list needs read operations
            // 2. Storage list needs write operations
            // 3. Termination signal or periodic timeout
            match embassy_futures::select::select3(
                list.needs_read().wait(),
                list.needs_write().wait(),
                async {
                    if let Some(signal) = kill_signal.as_mut() {
                        // Wait for explicit termination signal
                        signal.changed().await
                    } else {
                        // No kill signal provided, use periodic timeout as fallback
                        sleep(Duration::from_secs(10)).await;
                        Ok(())
                    }
                },
            )
            .await
            {
                // Handle read request from storage list
                embassy_futures::select::Either3::First(_) => {
                    info!("worker task got needs_read signal");
                    list.process_reads(&mut flash, &mut read_buf).await;
                }
                // Handle write request from storage list
                embassy_futures::select::Either3::Second(_) => {
                    info!("worker task got needs_write signal");
                    // Process writes and handle any errors that occur during the operation
                    if let Err(e) = list
                        .process_writes(&mut flash, &mut read_buf, &mut serde_buf)
                        .await
                    {
                        error!("Error in process_writes: {}", e);
                    }
                }
                // Handle termination signal or timeout - exit the loop and return flash
                embassy_futures::select::Either3::Third(_) => return flash,
            }
        }
    })
}

/// Test that verifies the storage system returns default configuration values when reading from empty flash.
///
/// This test validates the basic behavior of the storage system when no configuration data has been
/// previously written to flash storage. It creates a fresh flash instance without any data and
/// attempts to load configuration from it.
///
/// The test demonstrates that:
/// 1. The storage system can handle empty flash storage gracefully
/// 2. When no configuration data exists, the system returns the default configuration values
/// 3. The storage initialization and worker task startup work correctly with empty flash
///
/// This validates the expected fallback behavior for first-time system startup or after
/// flash memory has been completely erased.
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

/// Test that verifies configuration persistence across simulated system restarts.
///
/// This test writes a configuration to flash storage, then simulates a system restart
/// by creating a new storage list and node. It verifies that the previously written
/// configuration is correctly loaded from flash storage after the restart.
///
/// The test demonstrates that:
/// 1. Configuration data is properly persisted to flash storage
/// 2. The storage system can recover configuration data after a restart
/// 3. Data integrity is maintained across storage system lifecycle events
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
        "Loaded config should match test_config"
    );
}

/// Test that verifies config persistence across restarts and proper handling of multiple configs.
///
/// This test writes a config to one node, simulates a system restart by creating new storage lists
/// and nodes, then verifies that:
/// 1. The previously written config is correctly loaded from flash storage
/// 2. A new config node returns default values when no data exists for it
///
/// The test demonstrates that the storage system can handle multiple configuration nodes
/// independently and maintain data integrity across restarts.
#[test(tokio::test)]
async fn test_read_clean_state_new_config() {
    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config1");

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
    static NODE1: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
    static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

    info!(
        "Spawning new worker with flash content: {}",
        flash.flash().print_items().await
    );
    // Spawn worker task with the used flash
    let _worker = worker_task(&LIST2, flash, None);

    let handle1 = NODE1.attach(&LIST2).await.unwrap();
    let handle2 = NODE2.attach(&LIST2).await.unwrap();

    let loaded_config1 = handle1.load().await.unwrap();
    let loaded_config2 = handle2.load().await.unwrap();
    assert_eq!(
        loaded_config1, test_config,
        "Loaded config should match the test_config"
    );
    assert_eq!(
        loaded_config2,
        TestConfig::default(),
        "Loaded config should match default config"
    );
}

/// Test that verifies the storage system's resilience to interrupted writes and flash corruption.
///
/// This test simulates a scenario where flash storage becomes partially corrupted during or after
/// a write operation. It writes configurations to two separate nodes, then deliberately corrupts
/// part of the flash memory by filling bytes with 0xFF (simulating an interrupted erase/write cycle).
///
/// The test demonstrates that:
/// 1. The storage system can handle partial flash corruption gracefully
/// 2. Uncorrupted configuration data remains accessible after corruption occurs
/// 3. Corrupted configuration nodes fall back to default values appropriately
/// 4. Multiple configuration nodes can coexist with different corruption states
///
/// This validates the robustness of the storage system against real-world scenarios where
/// power loss or other interruptions could corrupt flash memory during write operations.
#[test(tokio::test)]
async fn test_read_interrupted_write() {
    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE1: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
    static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

    let flash = get_test_flash();
    info!("Spawning worker task");
    let (tx, rx) = watch::channel(());
    let worker = worker_task(&LIST, flash, Some(rx));

    let handle1 = NODE1.attach(&LIST).await.unwrap();
    let handle2 = NODE2.attach(&LIST).await.unwrap();
    let test_config2 = TestConfig {
        value: 1,
        truth: true,
        optional_truth: Some(true),
    };
    handle1
        .write(&TestConfig {
            value: 2,
            truth: false,
            optional_truth: Some(false),
        })
        .await
        .unwrap();
    handle2.write(&test_config2).await.unwrap();

    // Write the two nodes to the list
    LIST.needs_write().wake_all();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Stop worker task and save the flash data it produced
    tx.send(()).unwrap();
    let mut flash = worker.await.unwrap();

    warn!("Set some bytes in flash to zero");
    warn!("Flash before: {}", flash.flash().print_items().await);
    let flash_bytes = flash.flash().as_bytes_mut();
    // at byte 48 the 2nd item starts (may need to be adapted! check the print_items output)
    // The second item seems to be the first node, so this relies on knowing what the layout in flash is
    // TODO: improve how the flash is partly erased...
    flash_bytes[48..].fill(0xFF);

    warn!("Flash after: {}", flash.flash().print_items().await);

    // Create new list and node to simulate restart
    static LIST2: StorageList<CriticalSectionRawMutex> = StorageList::new();

    static NODE1_2: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
    static NODE2_2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

    info!("Spawning new worker task");
    // Spawn worker task with the used flash
    let _worker = worker_task(&LIST2, flash, None);

    let handle1 = NODE1_2.attach(&LIST2).await.unwrap();
    let handle2 = NODE2_2.attach(&LIST2).await.unwrap();

    let loaded_config1 = handle1.load().await.unwrap();
    let loaded_config2 = handle2.load().await.unwrap();
    assert_eq!(
        loaded_config1,
        TestConfig::default(),
        "Loaded config1 should match default config (was zeroed before)"
    );
    assert_eq!(
        loaded_config2, test_config2,
        "Loaded config1 should match test_config"
    );
}

/// Test that verifies the storage system's handling of multiple writes to the same configuration node.
///
/// This test performs two consecutive writes of identical configuration data to a single node and
/// verifies that the storage system correctly manages flash storage entries by cleaning up old data.
/// The test validates that:
/// 1. The first write creates initial flash entries for the configuration
/// 2. The second write of the same configuration data is handled properly
/// 3. After the second successful write, the old configuration entries are automatically deleted
/// 4. Only the expected number of flash entries remain (one for the current node and one for write confirmation)
///
/// The test demonstrates that the storage system efficiently manages flash space by removing
/// obsolete entries after successful writes, preventing flash storage from accumulating
/// unnecessary duplicate configuration data over multiple write operations.
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
    // Write the config for the first time
    handle.write(&test_config).await.unwrap();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Write again. This should delete the old nodes when write was successful.
    handle.write(&test_config).await.unwrap();

    // Give worker time to process the write
    sleep(Duration::from_millis(100)).await;

    // Stop worker task and save the flash data it produced
    tx.send(()).unwrap();
    let mut flash = worker.await.unwrap();

    info!("Flash content: {}", flash.flash().print_items().await);

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
