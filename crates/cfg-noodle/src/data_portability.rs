//! # Data Portability Guide
//!
//! ## Adding Fields
//! Fields may be added to existing configuration data if every new field is either
//! - an [`Option<T>`] or
//! - marked with [`#[cbor(default)]`](https://docs.rs/minicbor-derive/0.17.0/minicbor_derive/index.html#cbordefault)
//!
//! so that it can be deserialized from an old version of the config.
//!
//! ## Removing Fields
//! Removing fields is not a problem because minicbor just skips unknown fields.
//!
//! ## General Notes
//! The `#[n(...)]` annotation **MUST NOT CHANGE** even if you are removing fields.
//! They must remain numerically stable!
//!
//! If deserialization fails, cfg-noodle will initialize the config with the default.
//!
//! From [minicbor's documentation:](https://docs.rs/minicbor-derive/0.17.0/minicbor_derive/index.html)
//! > \[The\] encoding has the following characteristics:
//! >
//! >    1. The encoding does not contain any names, i.e. no field names, type names or variant names. Instead, every field and every constructor needs to be annotated with an index number, e.g. #[n(1)].
//! >    2. Unknown fields are ignored during decoding.
//! >    3. Optional types default to None if their value is not present during decoding.
//! >    4. Optional enums default to None if an unknown variant is encountered during decoding.
//!
//!
//! ## Example
//! The following example demonstrates how (not) to extend the struct `ConfigV1` with
//! a new field `vibration`.
//!
//! The expected behavior is:
//! - `ConfigV1_1a` is **invalid** and will be initialized to default
//! - `ConfigV1_1b` adds an `Option<f32>` field that will be initialized to `None`
//! - `ConfigV1_1c` marks the field with `#[cbor(default)]` and will be initialized
//!   to `f32::default()`
//!
//! ```
//! # use core::time::Duration;
//! # use std::sync::Arc;
//! # use log::info;
//! # use maitake_sync::WaitQueue;
//! # use minicbor::{CborLen, Decode, Encode};
//! # use mutex::raw_impls::cs::CriticalSectionRawMutex;
//! # use tokio::{task::LocalSet, time::sleep};
//! use cfg_noodle::{StorageList, StorageListNode, test_utils::worker_task_tst_sto};
//!
//! #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
//! struct ConfigV1 {
//!     #[n(1)]
//!     brightness: f32,
//!     #[n(2)]
//!     volume: f32,
//! }
//!
//! #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
//! struct ConfigV1_1a {
//!     #[n(1)]
//!     brightness: f32,
//!     #[n(2)]
//!     volume: f32,
//!     #[n(3)]
//!     vibration: f32,
//! }
//!
//! #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
//! struct ConfigV1_1b {
//!     #[n(1)]
//!     brightness: f32,
//!     #[n(2)]
//!     volume: f32,
//!     #[n(3)]
//!     vibration: Option<f32>,
//! }
//!
//! #[derive(Debug, Default, Clone, Decode, Encode, CborLen)]
//! struct ConfigV1_1c {
//!     #[n(1)]
//!     brightness: f32,
//!     #[n(2)]
//!     volume: f32,
//!     #[n(3)]
//!     #[cbor(default)]
//!     vibration: f32,
//! }
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//!
//! static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
//! // NOTE: normally you never want shadowed config keys like this,
//! // we will be ensuring only one of these is ever attached at once
//! // by detaching them after each step, to simulate firmware changes over time.
//! static CONFIGV1: StorageListNode<ConfigV1> = StorageListNode::new("config/v1");
//! static CONFIGV1_1A: StorageListNode<ConfigV1_1a> = StorageListNode::new("config/v1");
//! static CONFIGV1_1B: StorageListNode<ConfigV1_1b> = StorageListNode::new("config/v1");
//! static CONFIGV1_1C: StorageListNode<ConfigV1_1c> = StorageListNode::new("config/v1");
//!
//! # let local = LocalSet::new();
//! # local.run_until(async move {
//! # let stopper = Arc::new(WaitQueue::new());
//! #
//! let worker_task =
//!     tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));
//!
//! {
//!     // Obtain a handle for the first config
//!     let mut cfg_1 = CONFIGV1
//!         .attach(&GLOBAL_LIST)
//!         .await
//!         .expect("Attaching should not fail");
//!     cfg_1
//!         .write(&ConfigV1 {
//!             brightness: 1.0,
//!             volume: 2.0,
//!         })
//!         .await
//!         .expect("Write should not fail");
//!
//!     // Give the worker some time to process writes
//!     sleep(Duration::from_micros(20)).await;
//! }
//! CONFIGV1
//!     .detach(&GLOBAL_LIST)
//!     .await
//!     .expect("Detaching should not fail");
//!
//! {
//!     // ConfigV1_1a adds a new field without an Option<>/#[cbor(default)],
//!     // so deserialization will fail.
//!     // We expect the config to be initialized with the default then.
//!     let handle_1a = CONFIGV1_1A
//!         .attach_with_default(&GLOBAL_LIST, || ConfigV1_1a {
//!             brightness: 0.0,
//!             volume: 0.0,
//!             vibration: 10.0,
//!         })
//!         .await
//!         .expect("Attaching should not fail");
//!     let cfg_1a = handle_1a.load();
//!     info!("cfg_1a is {:?}", cfg_1a);
//!     assert_eq!(cfg_1a.brightness, 0.0, "Brightness default");
//!     assert_eq!(cfg_1a.volume, 0.0, "Volume default");
//!     assert_eq!(cfg_1a.vibration, 10.0, "Vibration default");
//! }
//!
//! CONFIGV1_1A
//!     .detach(&GLOBAL_LIST)
//!     .await
//!     .expect("Detaching should not fail");
//!
//! {
//!     // ConfigV1_1b adds a new field as an Option<>, so deserialization will work.
//!     // We expect it to be loaded with a None value on the new field.
//!     let handle_1b = CONFIGV1_1B
//!         .attach_with_default(&GLOBAL_LIST, || ConfigV1_1b {
//!             brightness: 0.0,
//!             volume: 0.0,
//!             vibration: Some(10.0),
//!         })
//!         .await
//!         .expect("Attaching should not fail");
//!     let cfg_1b = handle_1b.load();
//!     info!("cfg_1b is {:?}", cfg_1b);
//!     assert_eq!(cfg_1b.brightness, 1.0, "match brightness of cfg1");
//!     assert_eq!(cfg_1b.volume, 2.0, "Match volume of cfg1");
//!     assert_eq!(cfg_1b.vibration, None, "Vibration should be None");
//! }
//!
//! CONFIGV1_1B
//!     .detach(&GLOBAL_LIST)
//!     .await
//!     .expect("Detaching should not fail");
//!
//! {
//!     // ConfigV1_1c adds a new field with a marked as `#[cbor(default)]`, so deserialization will work.
//!     // We expect it to be loaded with the default value on the new field.
//!     let handle_1c = CONFIGV1_1C
//!         .attach_with_default(&GLOBAL_LIST, || ConfigV1_1c {
//!             brightness: 0.0,
//!             volume: 0.0,
//!             vibration: 10.0,
//!         })
//!         .await
//!         .expect("Attaching should not fail");
//!     let cfg_1c = handle_1c.load();
//!     info!("cfg_1c is {:?}", cfg_1c);
//!     assert_eq!(cfg_1c.brightness, 1.0, "match brightness of cfg1");
//!     assert_eq!(cfg_1c.volume, 2.0, "Match volume of cfg1");
//!     assert_eq!(
//!         cfg_1c.vibration,
//!         f32::default(),
//!         "Vibration should be default"
//!     );
//! }
//! # // Wait for the worker task to finish
//! # stopper.close();
//! # let report = tokio::time::timeout(Duration::from_secs(2), worker_task)
//! #   .await
//! #   .expect("shouldn't happen")
//! #   .expect("shouldn't happen");
//! # report.assert_no_errs();
//! # }).await;
//! # })
//! ```
