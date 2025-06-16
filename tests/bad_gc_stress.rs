#![cfg_attr(miri, allow(dead_code, unused_imports))]

use std::{num::NonZeroU32, sync::Arc};

use cfg_noodle::{StorageList, StorageListNode};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::task::{yield_now, LocalSet};
use cfg_noodle::test_utils::{TestStorage, worker_task_tst_sto, worker_task_tst_sto_custom, TestItem, TestElem};
use test_log::test;

#[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
struct SimpleConfig {
    #[n(0)]
    data: u64,
}

#[test(tokio::test)]
async fn bad_gc_stress() {
    let local = LocalSet::new();
    local
        .run_until(bad_gc_stress_inner())
        .await;
}

async fn bad_gc_stress_inner() {
    let mut flash = TestStorage::default();

    // add a bunch of bad elements
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 1 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(4).unwrap());
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 2 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(5).unwrap());
    flash.end_write_record(NonZeroU32::new(4).unwrap(), 1234);

    // add one that is ALMOST good but with a bad CRC
    let mut wr = flash.start_write_record(NonZeroU32::new(5).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 3 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 4 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 5 });
    wr.end_write_record();
    {
        let item = flash.items.last_mut().unwrap();
        let TestElem::End { seq_no, calc_crc } = item.elem.as_mut().unwrap() else {
            panic!()
        };
        *calc_crc = !*calc_crc;
    }

    // add a good record, aliases with the almost-good item!
    let mut wr = flash.start_write_record(NonZeroU32::new(5).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 13 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 14 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 15 });
    wr.end_write_record();

    // Add another bad item
    let mut wr = flash.start_write_record(NonZeroU32::new(6).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 23 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 24 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 25 });
    wr.end_write_record();
    {
        let item = flash.items.last_mut().unwrap();
        let TestElem::End { seq_no, calc_crc } = item.elem.as_mut().unwrap() else {
            panic!()
        };
        *calc_crc = !*calc_crc;
    }

    // add a bunch of bad elements
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 1 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(4).unwrap());
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 2 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(5).unwrap());
    flash.end_write_record(NonZeroU32::new(4).unwrap(), 1234);

    // add a good record, aliases with the almost-good item!
    let mut wr = flash.start_write_record(NonZeroU32::new(6).unwrap());
    wr.add_data_elem("test/config1", &SimpleConfig { data: 33 });
    wr.add_data_elem("test/config2", &SimpleConfig { data: 34 });
    wr.add_data_elem("test/config3", &SimpleConfig { data: 35 });
    wr.end_write_record();

    // and finally a bunch more bad elements
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 1 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(4).unwrap());
    flash.add_data_elem::<SimpleConfig>(
        "test/configx",
        &SimpleConfig { data: 2 },
        None,
    );
    flash.start_write_record(NonZeroU32::new(5).unwrap());
    flash.end_write_record(NonZeroU32::new(4).unwrap(), 1234);

    static LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    static NODE_A: StorageListNode<SimpleConfig> = StorageListNode::new("test/config1");
    static NODE_B: StorageListNode<SimpleConfig> = StorageListNode::new("test/config2");
    static NODE_C: StorageListNode<SimpleConfig> = StorageListNode::new("test/config3");

    let stopper = Arc::new(WaitQueue::new());
    let hdl = tokio::task::spawn_local(worker_task_tst_sto_custom(&LIST, stopper.clone(), flash));

    let node_a = NODE_A.attach(&LIST).await.unwrap();
    let node_b = NODE_B.attach(&LIST).await.unwrap();
    let node_c = NODE_C.attach(&LIST).await.unwrap();

    assert_eq!(node_a.load().await.unwrap().data, 33);
    assert_eq!(node_b.load().await.unwrap().data, 34);
    assert_eq!(node_c.load().await.unwrap().data, 35);

    // yield to ensure initial gc has a chance to run
    yield_now().await;
}
