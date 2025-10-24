//! We had a report that in the case of a corrupt filesystem,
//! that we could end up with a lock-up condition.
//!
//! These tests show that if our first read fails (e.g. due to
//! a corrupted FS), then subsequent reads succeed, for example
//! after formatting the storage, that normal operation would
//! resume.

use std::num::NonZeroU32;

use cfg_noodle::{
    StorageList, StorageListNode,
    error::LoadStoreError,
    test_utils::{TestStorage, TestStorageError, get_mock_flash},
};
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::task::{LocalSet, yield_now};

#[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
struct SimpleConfig {
    #[n(0)]
    data: u64,
}

#[tokio::test]
async fn with_test_storage() {
    let mut flash = TestStorage::default();

    // Add a valid set of records...
    let mut wr = flash.start_write_record(NonZeroU32::new(5).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 13 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 14 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 15 });
    wr.end_write_record();

    // But ensure that the underlying storage will ALWAYS return an error
    flash.forced_error = Some(TestStorageError::FakeBadRead);

    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");

    let mut buf = [0u8; 4096];

    let local = LocalSet::new();
    local
        .run_until(async move {
            let node_a_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_A.attach(&LIST).await.unwrap();
                hdl.load()
            });
            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Do a process_reads, make sure it fails
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            assert!(matches!(
                res,
                Err(LoadStoreError::FlashRead(TestStorageError::FakeBadRead))
            ));

            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Clear the error, clear the storage as well
            flash.forced_error = None;
            flash.items.clear();

            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            res.unwrap();

            // Attach IS done...
            yield_now().await;
            assert!(node_a_hdl.is_finished());

            let hdl_a_res = node_a_hdl.await;
            let cfg = hdl_a_res.unwrap();
            assert_eq!(cfg, SimpleConfig::default());
        })
        .await;
}

#[tokio::test]
async fn with_mock_flash() {
    let mut flash = get_mock_flash();
    // Corrupt the flash, giving it all zeros
    flash.flash().as_bytes_mut().iter_mut().for_each(|b| *b = 0);

    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");

    let mut buf = [0u8; 4096];

    let local = LocalSet::new();
    local
        .run_until(async move {
            let node_a_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_A.attach(&LIST).await.unwrap();
                hdl.load()
            });
            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Do a process_reads, make sure it fails
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            assert!(matches!(res, Err(LoadStoreError::FlashRead(_))));

            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Clear the error, clear the storage as well
            flash
                .flash()
                .as_bytes_mut()
                .iter_mut()
                .for_each(|b| *b = 0xFF);

            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            res.unwrap();

            // Attach IS done...
            yield_now().await;
            assert!(node_a_hdl.is_finished());

            let hdl_a_res = node_a_hdl.await;
            let cfg = hdl_a_res.unwrap();
            assert_eq!(cfg, SimpleConfig::default());
        })
        .await;
}
