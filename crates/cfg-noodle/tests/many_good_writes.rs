#![cfg_attr(miri, allow(dead_code, unused_imports))]

use std::{num::NonZeroU32, sync::Arc};

use cfg_noodle::test_utils::{TestElem, TestItem, worker_task_tst_sto, worker_task_tst_sto_custom};
use cfg_noodle::{StorageList, StorageListNode};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::task::{LocalSet, yield_now};

#[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
struct SimpleConfig {
    #[n(0)]
    data: u64,
}

// Test runs a bit slow to reasonably run in miri
#[tokio::test]
#[cfg(not(miri))]
async fn many_good_writes() {
    let local = LocalSet::new();
    local.run_until(many_good_writes_inner()).await;
}

/// This is a test that just does a bunch of nominally valid writes, to ensure we
/// behave reasonably in the happy path case over time.
async fn many_good_writes_inner() {
    static LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto(&LIST, stopper.clone()));

    let mut node_a = NODE_A.attach(&LIST).await.unwrap();
    let mut node_b = NODE_B.attach(&LIST).await.unwrap();
    let mut node_c = NODE_C.attach(&LIST).await.unwrap();

    assert_eq!(node_a.load(), SimpleConfig::default());
    assert_eq!(node_b.load(), SimpleConfig::default());
    assert_eq!(node_c.load(), SimpleConfig::default());

    // yield to ensure initial gc has a chance to run
    yield_now().await;

    // One thousand write cycles later...
    for i in 1..1000 {
        node_a.write(&SimpleConfig { data: i }).await.unwrap();
        node_b.write(&SimpleConfig { data: i * 10 }).await.unwrap();
        node_c.write(&SimpleConfig { data: i * 100 }).await.unwrap();
        // yield to allow writing and gc to take place
        yield_now().await;
    }

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    let contents = &rpt.flash.items;

    // This is a basic snapshot that we end up with the last three valid write records
    // as the only contents in flash. All older items have been continually invalidated,
    // leaving us with a fairly straightforward set in flash.
    #[rustfmt::skip]
    let expected = &[
        // Oldest item, seq_no 997
        TestItem { ctr: 4980, elem: TestElem::Start { seq_no: NonZeroU32::new(997).unwrap() } },
            TestItem { ctr: 4981, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 133, 116] } },
            TestItem { ctr: 4982, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 38, 242] } },
            TestItem { ctr: 4983, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 229] } },
        TestItem { ctr: 4984, elem: TestElem::End { seq_no: NonZeroU32::new(997).unwrap(), calc_crc: 2789166760 } },
        // Middle item, seq_no 998
        TestItem { ctr: 4985, elem: TestElem::Start { seq_no: NonZeroU32::new(998).unwrap() } },
            TestItem { ctr: 4986, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 133, 216] } },
            TestItem { ctr: 4987, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 38, 252] } },
            TestItem { ctr: 4988, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 230] } },
        TestItem { ctr: 4989, elem: TestElem::End { seq_no: NonZeroU32::new(998).unwrap(), calc_crc: 1413754379 } },
        // Newest item, seq_no 999
        TestItem { ctr: 4990, elem: TestElem::Start { seq_no: NonZeroU32::new(999).unwrap() } },
            TestItem { ctr: 4991, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 134, 60] } },
            TestItem { ctr: 4992, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 39, 6] } },
            TestItem { ctr: 4993, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 231] } },
        TestItem { ctr: 4994, elem: TestElem::End { seq_no: NonZeroU32::new(999).unwrap(), calc_crc: 2388236464 } },
    ];

    assert_eq!(contents, expected);

    // Simulate a reboot
    static LIST2: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    static NODE_A2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto_custom(
        &LIST2,
        stopper.clone(),
        rpt.flash,
    ));

    let node_a = NODE_A2.attach(&LIST2).await.unwrap();
    let node_b = NODE_B2.attach(&LIST2).await.unwrap();
    let node_c = NODE_C2.attach(&LIST2).await.unwrap();

    assert_eq!(node_a.load().data, 999);
    assert_eq!(node_b.load().data, 9990);
    assert_eq!(node_c.load().data, 99900);

    yield_now().await;

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    let contents = &rpt.flash.items;
    // Just rebooting did not cause any change to the contents of the flash
    assert_eq!(contents, expected);

    // Simulate a reboot
    static LIST3: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    static NODE_A3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto_custom(
        &LIST3,
        stopper.clone(),
        rpt.flash,
    ));

    let mut node_a = NODE_A3.attach(&LIST3).await.unwrap();
    let mut node_b = NODE_B3.attach(&LIST3).await.unwrap();
    let mut node_c = NODE_C3.attach(&LIST3).await.unwrap();

    assert_eq!(node_a.load().data, 999);
    assert_eq!(node_b.load().data, 9990);
    assert_eq!(node_c.load().data, 99900);

    yield_now().await;

    // Do one more write
    node_a.write(&SimpleConfig { data: 1000 }).await.unwrap();
    node_b.write(&SimpleConfig { data: 10000 }).await.unwrap();
    node_c.write(&SimpleConfig { data: 100000 }).await.unwrap();

    yield_now().await;

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    // Similar to our snapshot above, we want to see exactly one write record (997)
    // get rotated out, with the newest one (1000) as the newest item.
    #[rustfmt::skip]
    let expected2 = &[
        // Oldest item, seq_no 998
        TestItem { ctr: 4985, elem: TestElem::Start { seq_no: NonZeroU32::new(998).unwrap() } },
            TestItem { ctr: 4986, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 133, 216] } },
            TestItem { ctr: 4987, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 38, 252] } },
            TestItem { ctr: 4988, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 230] } },
        TestItem { ctr: 4989, elem: TestElem::End { seq_no: NonZeroU32::new(998).unwrap(), calc_crc: 1413754379 } },
        // Middle item, seq_no 999
        TestItem { ctr: 4990, elem: TestElem::Start { seq_no: NonZeroU32::new(999).unwrap() } },
            TestItem { ctr: 4991, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 134, 60] } },
            TestItem { ctr: 4992, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 39, 6] } },
            TestItem { ctr: 4993, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 231] } },
        TestItem { ctr: 4994, elem: TestElem::End { seq_no: NonZeroU32::new(999).unwrap(), calc_crc: 2388236464 } },
        // Newest item, seq_no 1000
        TestItem { ctr: 4995, elem: TestElem::Start { seq_no: NonZeroU32::new(1000).unwrap() } },
            TestItem { ctr: 4996, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 134, 160] } },
            TestItem { ctr: 4997, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 39, 16] } },
            TestItem { ctr: 4998, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 232] } },
        TestItem { ctr: 4999, elem: TestElem::End { seq_no: NonZeroU32::new(1000).unwrap(), calc_crc: 2217662377 } },
    ];

    let contents = &rpt.flash.items;
    // New write resumed correctly
    assert_eq!(contents, expected2);
}

// Test runs a bit slow to reasonably run in miri
#[tokio::test]
#[cfg(not(miri))]
async fn many_good_writes_one_kept_record() {
    let local = LocalSet::new();
    local
        .run_until(many_good_writes_inner_one_kept_record())
        .await;
}

/// This is a test that just does a bunch of nominally valid writes, to ensure we
/// behave reasonably in the happy path case over time. Same as above but with only 1 kept record
async fn many_good_writes_inner_one_kept_record() {
    static LIST: StorageList<CriticalSectionRawMutex, 1> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto(&LIST, stopper.clone()));

    let mut node_a = NODE_A.attach(&LIST).await.unwrap();
    let mut node_b = NODE_B.attach(&LIST).await.unwrap();
    let mut node_c = NODE_C.attach(&LIST).await.unwrap();

    assert_eq!(node_a.load(), SimpleConfig::default());
    assert_eq!(node_b.load(), SimpleConfig::default());
    assert_eq!(node_c.load(), SimpleConfig::default());

    // yield to ensure initial gc has a chance to run
    yield_now().await;

    // One thousand write cycles later...
    for i in 1..1000 {
        node_a.write(&SimpleConfig { data: i }).await.unwrap();
        node_b.write(&SimpleConfig { data: i * 10 }).await.unwrap();
        node_c.write(&SimpleConfig { data: i * 100 }).await.unwrap();
        // yield to allow writing and gc to take place
        yield_now().await;
    }

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    let contents = &rpt.flash.items;

    // This is a basic snapshot that we end up with the last valid write record
    // as the only contents in flash. All older items have been continually invalidated,
    // leaving us with a fairly straightforward set in flash.
    #[rustfmt::skip]
    let expected = &[
        // Newest item, seq_no 999
        TestItem { ctr: 4990, elem: TestElem::Start { seq_no: NonZeroU32::new(999).unwrap() } },
            TestItem { ctr: 4991, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 134, 60] } },
            TestItem { ctr: 4992, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 39, 6] } },
            TestItem { ctr: 4993, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 231] } },
        TestItem { ctr: 4994, elem: TestElem::End { seq_no: NonZeroU32::new(999).unwrap(), calc_crc: 2388236464 } },
    ];

    assert_eq!(contents, expected);

    // Simulate a reboot
    static LIST2: StorageList<CriticalSectionRawMutex, 1> = StorageList::new();
    static NODE_A2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C2: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto_custom(
        &LIST2,
        stopper.clone(),
        rpt.flash,
    ));

    let node_a = NODE_A2.attach(&LIST2).await.unwrap();
    let node_b = NODE_B2.attach(&LIST2).await.unwrap();
    let node_c = NODE_C2.attach(&LIST2).await.unwrap();

    assert_eq!(node_a.load().data, 999);
    assert_eq!(node_b.load().data, 9990);
    assert_eq!(node_c.load().data, 99900);

    yield_now().await;

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    let contents = &rpt.flash.items;
    // Just rebooting did not cause any change to the contents of the flash
    assert_eq!(contents, expected);

    // Simulate a reboot
    static LIST3: StorageList<CriticalSectionRawMutex, 1> = StorageList::new();
    static NODE_A3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C3: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto_custom(
        &LIST3,
        stopper.clone(),
        rpt.flash,
    ));

    let mut node_a = NODE_A3.attach(&LIST3).await.unwrap();
    let mut node_b = NODE_B3.attach(&LIST3).await.unwrap();
    let mut node_c = NODE_C3.attach(&LIST3).await.unwrap();

    assert_eq!(node_a.load().data, 999);
    assert_eq!(node_b.load().data, 9990);
    assert_eq!(node_c.load().data, 99900);

    yield_now().await;

    // Do one more write
    node_a.write(&SimpleConfig { data: 1000 }).await.unwrap();
    node_b.write(&SimpleConfig { data: 10000 }).await.unwrap();
    node_c.write(&SimpleConfig { data: 100000 }).await.unwrap();

    yield_now().await;

    stopper.close();
    let rpt = hdl.await.unwrap();
    rpt.assert_no_errs();

    // Similar to our snapshot above, we want to see all records (999)
    // get rotated out, with the newest one (1000) as the newest item.
    #[rustfmt::skip]
    let expected2 = &[
        // Newest item, seq_no 1000
        TestItem { ctr: 4995, elem: TestElem::Start { seq_no: NonZeroU32::new(1000).unwrap() } },
            TestItem { ctr: 4996, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 51, 129, 26, 0, 1, 134, 160] } },
            TestItem { ctr: 4997, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 50, 129, 25, 39, 16] } },
            TestItem { ctr: 4998, elem: TestElem::Data { data: vec![1, 108, 116, 101, 115, 116, 47, 99, 111, 110, 102, 105, 103, 49, 129, 25, 3, 232] } },
        TestItem { ctr: 4999, elem: TestElem::End { seq_no: NonZeroU32::new(1000).unwrap(), calc_crc: 2217662377 } },
    ];

    let contents = &rpt.flash.items;
    // New write resumed correctly
    assert_eq!(contents, expected2);
}
