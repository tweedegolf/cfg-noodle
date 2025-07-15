#[cfg(test)]
mod tests {
    use crate::sync::Arc;
    use crate::{StorageList, StorageListNode, test_utils};
    use log::info;
    use loom::future::block_on;
    use maitake_sync::WaitQueue;
    use maitake_sync::blocking::DefaultMutex;
    use minicbor::{CborLen, Decode, Encode};
    use std::sync::LazyLock;

    #[derive(Debug, Default, Encode, Decode, Clone, CborLen, PartialEq)]
    struct TestConfig {
        #[n(0)]
        value: u32,
        #[n(1)]
        truth: bool,
        #[n(2)]
        optional_truth: Option<bool>,
    }

    // #[cfg(loom)]
    #[test]
    fn works() {
        loom::model(|| {
            info!("Starting test");

            let list: &_ = Box::leak(Box::new(StorageList::<DefaultMutex>::new()));
            let node: &_ = Box::leak(Box::new(StorageListNode::<TestConfig>::new("test/config")));
            let stopper = Arc::new(WaitQueue::new());

            let worker = {
                let stopper = stopper.clone();
                loom::thread::Builder::new()
                    .stack_size(10 * 1024 * 1024)
                    .name("worker".into())
                    .spawn(move || block_on(test_utils::worker_task_tst_sto(list, stopper)))
                    .expect("spawning cannot fail")
            };

            let handle = loom::thread::Builder::new()
                .stack_size(10 * 1024 * 1024)
                .name("node".into())
                .spawn(move || block_on(async { node.attach(list).await.unwrap() }))
                .expect("spawning cannot fail")
                .join()
                .expect("spawning cannot fail");

            // Should return default value
            let config = handle.load();
            let default_config = TestConfig::default();
            assert_eq!(
                config, default_config,
                "Loaded config should match default config"
            );

            drop(handle);
            block_on(async { node.detach(list).await.expect("detaching must succeed") });

            stopper.wake();
            let report = worker.join().expect("joining can not fail");
            report.assert_no_errs();
        })
    }
}
