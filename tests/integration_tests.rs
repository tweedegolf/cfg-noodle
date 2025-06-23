use std::{num::NonZeroU32, sync::Arc};

use cfg_noodle::{StorageList, StorageListNode, test_utils};
use log::{info, warn};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use test_log::test;
use test_utils::TestElem;
use tokio::{
    task::LocalSet,
    time::{Duration, sleep},
};

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
    let flash = test_utils::get_mock_flash();

    let local = LocalSet::new();
    local
        .run_until(async move {
            info!("Spawning worker task");
            let _worker = tokio::task::spawn_local(test_utils::worker_task_seq_sto(&LIST, flash));

            let handle = NODE.attach(&LIST).await.unwrap();

            // Should return default value
            let config = handle.load();
            let default_config = TestConfig::default();
            assert_eq!(
                config, default_config,
                "Loaded config should match default config"
            );
        })
        .await;
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
    let local = LocalSet::new();
    local
        .run_until(async move {
            static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
            static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config");
            info!("Spawning worker task");
            let stopper = Arc::new(WaitQueue::new());
            let worker =
                tokio::task::spawn_local(test_utils::worker_task_tst_sto(&LIST, stopper.clone()));

            let mut handle = NODE.attach(&LIST).await.unwrap();

            let test_config = TestConfig {
                value: 42,
                truth: false,
                optional_truth: Some(true),
            };
            handle.write(&test_config).await.unwrap();

            // Give worker time to process the write
            sleep(Duration::from_millis(100)).await;

            // Stop worker task and save the flash data it produced
            stopper.close();
            let report = worker.await.unwrap();
            report.assert_no_errs();
            let flash = report.flash;

            // Create new list and node to simulate restart
            static LIST2: StorageList<CriticalSectionRawMutex> = StorageList::new();
            static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config");

            info!(
                "Spawning new worker with flash content: {}",
                flash.print_items()
            );
            // Spawn worker task with the used flash
            let stopper = Arc::new(WaitQueue::new());
            let worker = tokio::task::spawn_local(test_utils::worker_task_tst_sto_custom(
                &LIST2,
                stopper.clone(),
                flash,
            ));

            let handle2 = NODE2.attach(&LIST2).await.unwrap();

            let loaded_config = handle2.load();
            assert_eq!(
                loaded_config, test_config,
                "Loaded config should match test_config"
            );
            stopper.close();
            let res = worker.await.unwrap();
            res.assert_no_errs();
        })
        .await
}

/// Test that verifies config persistence across restarts and proper handling of multiple configs.
///
/// This test writes a config to one node, simulates a system restart by creating new storage lists
/// and nodes, then verifies that:
/// 1. The previously written config is correctly loaded from flash storage
/// 2. A newly added config node returns default values becauseno data exists for it
///
/// The test demonstrates that the storage system can handle multiple configuration nodes
/// independently and maintain data integrity across restarts.
#[test(tokio::test)]
async fn test_read_clean_state_new_config() {
    let local = LocalSet::new();
    local
        .run_until(async move {
            static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
            static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config1");

            info!("Spawning worker task");
            let stopper = Arc::new(WaitQueue::new());
            let worker =
                tokio::task::spawn_local(test_utils::worker_task_tst_sto(&LIST, stopper.clone()));

            let mut handle = NODE.attach(&LIST).await.unwrap();

            let test_config = TestConfig {
                value: 42,
                truth: false,
                optional_truth: Some(true),
            };
            handle.write(&test_config).await.unwrap();

            // Give worker time to process the write
            sleep(Duration::from_millis(100)).await;

            // Stop worker task and save the flash data it produced
            stopper.close();
            let report = worker.await.unwrap();
            report.assert_no_errs();
            let flash = report.flash;

            // Create new list and node to simulate restart
            static LIST2: StorageList<CriticalSectionRawMutex> = StorageList::new();
            static NODE1: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
            static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

            info!(
                "Spawning new worker with flash content: {}",
                flash.print_items()
            );
            // Spawn worker task with the used flash
            info!("Spawning worker task");
            let stopper = Arc::new(WaitQueue::new());
            let worker = tokio::task::spawn_local(test_utils::worker_task_tst_sto_custom(
                &LIST2,
                stopper.clone(),
                flash,
            ));

            let handle1 = NODE1.attach(&LIST2).await.unwrap();
            let handle2 = NODE2.attach(&LIST2).await.unwrap();

            let loaded_config1 = handle1.load();
            let loaded_config2 = handle2.load();
            assert_eq!(
                loaded_config1, test_config,
                "Loaded config should match the test_config"
            );
            assert_eq!(
                loaded_config2,
                TestConfig::default(),
                "Loaded config should match default config"
            );

            stopper.close();
            let rpt = worker.await.unwrap();
            rpt.assert_no_errs();
        })
        .await;
}

/// Test that verifies the storage system's resilience to interrupted writes and flash corruption.
///
/// This test simulates a scenario where flash storage becomes partially corrupted during or after
/// a write operation. It writes configurations to two separate nodes twice, then deliberately corrupts
/// part of the flash memory by overwriting a byte with 0xFF (simulating an interrupted erase/write cycle).
///
/// The test demonstrates that:
/// 1. The storage system can handle partial flash corruption gracefully
/// 2. Uncorrupted configuration data remains accessible after corruption occurs
/// 3. The system "rolls back" to the last valid value
///
/// This validates the robustness of the storage system against real-world scenarios where
/// power loss or other interruptions could corrupt flash memory during write operations.
#[test(tokio::test)]
async fn test_read_interrupted_write() {
    let local = LocalSet::new();
    local
        .run_until(async move {
            static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
            static NODE1: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
            static NODE2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

            info!("Spawning worker task");
            let stopper = Arc::new(WaitQueue::new());
            let worker =
                tokio::task::spawn_local(test_utils::worker_task_tst_sto(&LIST, stopper.clone()));

            let mut handle1 = NODE1.attach(&LIST).await.unwrap();
            let mut handle2 = NODE2.attach(&LIST).await.unwrap();
            let test_config1 = TestConfig {
                value: 1,
                truth: false,
                optional_truth: Some(false),
            };
            let test_config2 = TestConfig {
                value: 2,
                truth: false,
                optional_truth: Some(true),
            };
            let test_config3 = TestConfig {
                value: 3,
                truth: true,
                optional_truth: Some(false),
            };
            let test_config4 = TestConfig {
                value: 4,
                truth: true,
                optional_truth: Some(true),
            };
            handle1.write(&test_config1).await.unwrap();
            handle2.write(&test_config2).await.unwrap();

            // Give worker time to process the write
            sleep(Duration::from_millis(100)).await;

            handle1.write(&test_config3).await.unwrap();
            handle2.write(&test_config4).await.unwrap();

            // Give worker time to process the write
            sleep(Duration::from_millis(100)).await;

            // Stop worker task and save the flash data it produced
            stopper.close();
            let report = worker.await.unwrap();
            report.assert_no_errs();
            let mut flash = report.flash;

            warn!("Corrupt an item from write record 2");
            warn!("State before:");
            for i in flash.items.iter() {
                warn!("{i:?}");
            }

            let mut iter = flash.items.iter_mut();
            loop {
                let item = iter.next().unwrap();
                if matches!(item.elem, Some(TestElem::Start { seq_no }) if seq_no == NonZeroU32::new(2).unwrap()) {
                    break;
                }
            }
            let item = iter.next().unwrap();
            let Some(TestElem::Data { data }) = &mut item.elem else {
                panic!();
            };
            *data.get_mut(3).unwrap() = 0xFF;

            warn!("State after:");
            for i in flash.items.iter() {
                warn!("{i:?}");
            }

            // Create new list and node to simulate restart
            static LIST2: StorageList<CriticalSectionRawMutex> = StorageList::new();

            static NODE1_2: StorageListNode<TestConfig> = StorageListNode::new("test/config1");
            static NODE2_2: StorageListNode<TestConfig> = StorageListNode::new("test/config2");

            info!("Spawning worker task");
            let stopper = Arc::new(WaitQueue::new());
            let worker = tokio::task::spawn_local(test_utils::worker_task_tst_sto_custom(
                &LIST2,
                stopper.clone(),
                flash,
            ));

            let handle1 = NODE1_2.attach(&LIST2).await.unwrap();
            let handle2 = NODE2_2.attach(&LIST2).await.unwrap();

            let loaded_config1 = handle1.load();
            let loaded_config2 = handle2.load();
            assert_eq!(
                loaded_config1, test_config1,
                "Loaded config1 should match test_config"
            );
            assert_eq!(
                loaded_config2, test_config2,
                "Loaded config1 should match test_config"
            );

            stopper.close();
            let rpt = worker.await.unwrap();
            rpt.assert_no_errs();
        })
        .await
}

// TODO(AJM): We have not restored "garbage collection" yet, this test will need to be updated.
//
// /// Test that verifies the storage system's handling of multiple writes to the same configuration node.
// ///
// /// This test performs two consecutive writes of identical configuration data to a single node and
// /// verifies that the storage system correctly manages flash storage entries by cleaning up old data.
// /// The test validates that:
// /// 1. The first write creates initial flash entries for the configuration
// /// 2. The second write of the same configuration data is handled properly
// /// 3. After the second successful write, the old configuration entries are automatically deleted
// /// 4. Only the expected number of flash entries remain (one for the current node and one for write confirmation)
// ///
// /// The test demonstrates that the storage system efficiently manages flash space by removing
// /// obsolete entries after successful writes, preventing flash storage from accumulating
// /// unnecessary duplicate configuration data over multiple write operations.
// #[test(tokio::test)]
// async fn test_multiple_writes() {
//     static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
//     static NODE: StorageListNode<TestConfig> = StorageListNode::new("test/config");

//     let flash = get_test_flash();

//     info!("Spawning worker task");
//     let (tx, rx) = watch::channel(());
//     let worker = worker_task(&LIST, flash, Some(rx));

//     let handle = NODE.attach(&LIST).await.unwrap();

//     let test_config = TestConfig {
//         value: 42,
//         truth: false,
//         optional_truth: Some(true),
//     };
//     // Write the config for the first time
//     handle.write(&test_config).await.unwrap();

//     // Give worker time to process the write
//     sleep(Duration::from_millis(100)).await;

//     // Write again. This should delete the old nodes when write was successful.
//     handle.write(&test_config).await.unwrap();

//     // Give worker time to process the write
//     sleep(Duration::from_millis(100)).await;

//     // Stop worker task and save the flash data it produced
//     tx.send(()).unwrap();
//     let mut flash = worker.await.unwrap();

//     info!("Flash content: {}", flash.flash().print_items().await);

//     // Iterate over the flash and count the number of items
//     let mut iter = flash.iter().await.unwrap();
//     let mut item_counter = 0;
//     while iter.next(&mut [0u8; BUF_LEN]).await.unwrap().is_some() {
//         item_counter += 1;
//     }
//     info!("Found {} items in flash (expecting 2).", item_counter);
//     assert_eq!(
//         item_counter, 2,
//         "expected 2 items in the flash (one node and one write_confirm)"
//     )
// }
