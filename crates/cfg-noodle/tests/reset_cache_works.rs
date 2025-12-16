use std::num::NonZeroU32;

use cfg_noodle::{StorageList, StorageListNode, error::LoadStoreError, test_utils::TestStorage};
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::task::{LocalSet, yield_now};

#[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
struct SimpleConfig {
    #[n(0)]
    data: u64,
}

// This test case is the "base case": We do an extraordinary reset of the external
// flash chip, but DO NOT call `reset_cache`. This leads to later failures of `InconsistentFlash`
// because the reality of the storage no longer matches our cache state.
#[test_log::test(tokio::test)]
async fn bad_with_test_storage() {
    let mut flash = TestStorage::default();

    // Add a valid set of records...
    let mut wr = flash.start_write_record(NonZeroU32::new(5).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 13 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 14 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 15 });
    wr.end_write_record();

    static LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let mut buf = [0u8; 4096];

    let local = LocalSet::new();
    local
        .run_until(async move {
            // Do our normal attach for two of the three items...
            let node_a_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_A.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });
            let node_b_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_B.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });

            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            res.unwrap();

            // Attach IS done...
            yield_now().await;
            assert!(node_a_hdl.is_finished());

            let hdl_a_res = node_a_hdl.await;
            let (mut hdl_a, cfg_a) = hdl_a_res.unwrap();
            assert_eq!(cfg_a, SimpleConfig { data: 13 });
            hdl_a.write(&SimpleConfig { data: 33 }).await.unwrap();

            let hdl_b_res = node_b_hdl.await;
            let (mut hdl_b, cfg_a) = hdl_b_res.unwrap();
            assert_eq!(cfg_a, SimpleConfig { data: 14 });
            hdl_b.write(&SimpleConfig { data: 44 }).await.unwrap();

            LIST.process_garbage(&mut flash, &mut buf).await.unwrap();
            LIST.process_writes(&mut flash, &mut buf).await.unwrap();

            // oh no everything has gone terribly wrong, reset everything
            flash = TestStorage::default();
            // LIST.reset_cache().await;

            let _node_c_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_C.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });

            // Let the node-c handle attach
            yield_now().await;

            // process reads still works to hydrate the newest item
            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            let err = res.unwrap_err();
            assert_eq!(
                err,
                LoadStoreError::AppError(cfg_noodle::error::Error::InconsistentFlash)
            );
        })
        .await;
}

// And this is the positive test case, it is generally identical to the above test, but
// we DO call `reset_cache`, meaning that we do not see `InconsistentFlash`, and normal
// operation resumes.
#[test_log::test(tokio::test)]
async fn with_test_storage() {
    let mut flash = TestStorage::default();

    // Add a valid set of records...
    let mut wr = flash.start_write_record(NonZeroU32::new(5).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 13 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 14 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 15 });
    wr.end_write_record();

    static LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let mut buf = [0u8; 4096];

    let local = LocalSet::new();
    local
        .run_until(async move {
            // Do our normal attach for two of the three items...
            let node_a_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_A.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });
            let node_b_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_B.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });

            // Attach not done...
            yield_now().await;
            assert!(!node_a_hdl.is_finished());

            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            res.unwrap();

            // Attach IS done...
            yield_now().await;
            assert!(node_a_hdl.is_finished());

            let hdl_a_res = node_a_hdl.await;
            let (mut hdl_a, cfg_a) = hdl_a_res.unwrap();
            assert_eq!(cfg_a, SimpleConfig { data: 13 });
            hdl_a.write(&SimpleConfig { data: 33 }).await.unwrap();

            let hdl_b_res = node_b_hdl.await;
            let (mut hdl_b, cfg_a) = hdl_b_res.unwrap();
            assert_eq!(cfg_a, SimpleConfig { data: 14 });
            hdl_b.write(&SimpleConfig { data: 44 }).await.unwrap();

            LIST.process_garbage(&mut flash, &mut buf).await.unwrap();
            LIST.process_writes(&mut flash, &mut buf).await.unwrap();

            // oh no everything has gone terribly wrong, reset everything
            flash = TestStorage::default();
            LIST.reset_cache().await;

            let node_c_hdl = tokio::task::spawn_local(async {
                let hdl = NODE_C.attach(&LIST).await.unwrap();
                let val = hdl.load();
                (hdl, val)
            });

            // Let the node-c handle attach
            yield_now().await;

            // process reads still works to hydrate the newest item
            // Do a process_reads, make sure it succeeds
            let res = LIST.process_reads(&mut flash, &mut buf).await;
            res.unwrap();

            // Attach IS done...
            yield_now().await;
            assert!(node_c_hdl.is_finished());

            let hdl_c_res = node_c_hdl.await;
            let (_hdl_c, cfg_c) = hdl_c_res.unwrap();
            // Note: we get default, not 15, because we reset the TestStorage!
            assert_eq!(cfg_c, SimpleConfig::default());
            assert_eq!(hdl_b.load(), SimpleConfig { data: 44 });
            assert_eq!(hdl_a.load(), SimpleConfig { data: 33 });
        })
        .await;
}
