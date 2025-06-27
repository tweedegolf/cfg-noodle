//! Data Portability Guide
//!
//! <https://docs.rs/minicbor-derive/0.17.0/minicbor_derive/index.html>
//!

#[cfg(test)]
mod test {
    use core::time::Duration;
    use std::sync::Arc;

    use log::info;
    use maitake_sync::WaitQueue;
    use minicbor::{CborLen, Decode, Encode};
    use mutex::raw_impls::cs::CriticalSectionRawMutex;
    use test_log::test;
    use tokio::{task::LocalSet, time::sleep};

    use crate::{
        StorageList, StorageListNode,
        test_utils::
            worker_task_tst_sto
        ,
    };

    #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
    struct ConfigV1 {
        #[n(1)]
        brightness: f32,
        #[n(2)]
        volume: f32,
    }

    #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
    struct ConfigV1_1a {
        #[n(1)]
        brightness: f32,
        #[n(2)]
        volume: f32,
        #[n(3)]
        vibration: f32,
    }

    #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
    struct ConfigV1_1b {
        #[n(1)]
        brightness: f32,
        #[n(2)]
        volume: f32,
        #[n(3)]
        vibration: Option<f32>,
    }

    #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
    struct ConfigV1_1c {
        #[n(1)]
        brightness: f32,
        #[n(2)]
        volume: f32,
        #[n(3)]
        #[cbor(default)]
        vibration: f32,
    }
    /// This test will write a config to the flash first and then read it back to check
    /// whether reading an item from flash works.
    #[test(tokio::test)]
    async fn test_load_existing() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
        static CONFIGV1: StorageListNode<ConfigV1> = StorageListNode::new("config/v1");
        static CONFIGV1_1A: StorageListNode<ConfigV1_1a> = StorageListNode::new("config/v1");
        static CONFIGV1_1B: StorageListNode<ConfigV1_1b> = StorageListNode::new("config/v1");
        static CONFIGV1_1C: StorageListNode<ConfigV1_1c> = StorageListNode::new("config/v1");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                let worker_task =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));

                {
                    // Obtain a handle for the first config
                    let mut cfg_1 = CONFIGV1
                        .attach(&GLOBAL_LIST)
                        .await
                        .expect("Attaching should not fail");
                    cfg_1
                        .write(&ConfigV1 {
                            brightness: 1.0,
                            volume: 2.0,
                        })
                        .await
                        .expect("Write should not fail");

                    // Give the worker some time to process writes
                    sleep(Duration::from_micros(20)).await;
                }
                CONFIGV1
                    .detach(&GLOBAL_LIST)
                    .await
                    .expect("Detaching should not fail");

                {
                    // This ConfigV1_1a adds a new field without an Option<>, so deserialization will fail.
                    // We expect it to be initialized with the default then.
                    let handle_1a = CONFIGV1_1A
                        .attach_with_default(&GLOBAL_LIST, || ConfigV1_1a {
                            brightness: 0.0,
                            volume: 0.0,
                            vibration: 10.0,
                        })
                        .await
                        .expect("Attaching should not fail");
                    let cfg_1a = handle_1a.load();
                    info!("cfg_1a is {:?}", cfg_1a);
                    assert_eq!(cfg_1a.brightness, 0.0, "Brightness default");
                    assert_eq!(cfg_1a.volume, 0.0, "Volume default");
                    assert_eq!(cfg_1a.vibration, 10.0, "Vibration default");
                }

                CONFIGV1_1A
                    .detach(&GLOBAL_LIST)
                    .await
                    .expect("Detaching should not fail");

                {
                    // This ConfigV1_1b adds a new field as an Option<>, so deserialization will work.
                    // We expect it to be loaded with a None value on the new field.
                    let handle_1b = CONFIGV1_1B
                        .attach_with_default(&GLOBAL_LIST, || ConfigV1_1b {
                            brightness: 0.0,
                            volume: 0.0,
                            vibration: Some(10.0),
                        })
                        .await
                        .expect("Attaching should not fail");
                    let cfg_1b = handle_1b.load();
                    info!("cfg_1b is {:?}", cfg_1b);
                    assert_eq!(cfg_1b.brightness, 1.0, "match brightness of cfg1");
                    assert_eq!(cfg_1b.volume, 2.0, "Match volume of cfg1");
                    assert_eq!(cfg_1b.vibration, None, "Vibration should be None");
                }

                CONFIGV1_1B
                    .detach(&GLOBAL_LIST)
                    .await
                    .expect("Detaching should not fail");

                {
                    // This ConfigV1_1c adds a new field with a marked as `#[cbor(default)]`, so deserialization will work.
                    // We expect it to be loaded with the default value on the new field.
                    let handle_1c = CONFIGV1_1C
                        .attach_with_default(&GLOBAL_LIST, || ConfigV1_1c {
                            brightness: 0.0,
                            volume: 0.0,
                            vibration: 10.0,
                        })
                        .await
                        .expect("Attaching should not fail");
                    let cfg_1c = handle_1c.load();
                    info!("cfg_1c is {:?}", cfg_1c);
                    assert_eq!(cfg_1c.brightness, 1.0, "match brightness of cfg1");
                    assert_eq!(cfg_1c.volume, 2.0, "Match volume of cfg1");
                    assert_eq!(
                        cfg_1c.vibration,
                        f32::default(),
                        "Vibration should be default"
                    );
                }
                // Wait for the worker task to finish
                stopper.close();
                let report = tokio::time::timeout(Duration::from_secs(2), worker_task)
                    .await
                    .expect("shouldn't happen")
                    .expect("shouldn't happen");
                report.assert_no_errs();
            })
            .await;
    }
}
