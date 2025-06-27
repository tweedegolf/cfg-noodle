use cfg_noodle::StorageListNode;
use defmt::Format;
use embassy_executor::task;
use embassy_futures::select::select;
use embassy_time::{Duration, Ticker};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};

use super::LIST;

#[derive(Debug, Format, Encode, Decode, CborLen, Clone)]
pub struct DeltaConfig {
    #[n(0)]
    data_a: [u32; 8],
    #[n(1)]
    data_b: [u32; 4],
    #[n(2)]
    data_c: [u32; 2],
    #[n(3)]
    data_d: [u32; 2],
}

pub static NODES: [StorageListNode<DeltaConfig>; 10] = [
    StorageListNode::new("delta/one"),
    StorageListNode::new("delta/two"),
    StorageListNode::new("delta/three"),
    StorageListNode::new("delta/four"),
    StorageListNode::new("delta/five"),
    StorageListNode::new("delta/six"),
    StorageListNode::new("delta/seven"),
    StorageListNode::new("delta/eight"),
    StorageListNode::new("delta/nine"),
    StorageListNode::new("delta/ten"),
];

pub static INITS: [fn() -> DeltaConfig; 10] = [
    || DeltaConfig { data_a: [210; 8], data_b: [211; 4], data_c: [212; 2], data_d: [213; 2]  },
    || DeltaConfig { data_a: [220; 8], data_b: [221; 4], data_c: [222; 2], data_d: [223; 2]  },
    || DeltaConfig { data_a: [230; 8], data_b: [231; 4], data_c: [232; 2], data_d: [233; 2]  },
    || DeltaConfig { data_a: [240; 8], data_b: [241; 4], data_c: [242; 2], data_d: [243; 2]  },
    || DeltaConfig { data_a: [250; 8], data_b: [251; 4], data_c: [252; 2], data_d: [253; 2]  },
    || DeltaConfig { data_a: [260; 8], data_b: [261; 4], data_c: [262; 2], data_d: [263; 2]  },
    || DeltaConfig { data_a: [270; 8], data_b: [271; 4], data_c: [272; 2], data_d: [273; 2]  },
    || DeltaConfig { data_a: [280; 8], data_b: [281; 4], data_c: [282; 2], data_d: [283; 2]  },
    || DeltaConfig { data_a: [290; 8], data_b: [291; 4], data_c: [292; 2], data_d: [293; 2]  },
    || DeltaConfig { data_a: [2100; 8], data_b: [2101; 4], data_c: [2102; 2], data_d: [2103; 2]  },
];

#[task(pool_size = 10)]
pub async fn delta_worker(
    node: &'static StorageListNode<DeltaConfig>,
    init: fn() -> DeltaConfig,
    interval: Duration,
    stopper: &'static WaitQueue,
) {
    let mut hdl = node.attach_with_default(&LIST, init).await.unwrap();
    let mut val = hdl.load();
    let mut ticker = Ticker::every(interval);

    let worker_fut = async {
        loop {
            ticker.next().await;
            let DeltaConfig { data_a, data_b, data_c, data_d } = &mut val;
            let iter = data_a.iter_mut()
                .chain(data_b.iter_mut())
                .chain(data_c.iter_mut())
                .chain(data_d.iter_mut());

            // square every word on every tick
            for word in iter {
                *word = word.wrapping_mul(*word);
            }

            defmt::debug!("{=str} writing...", hdl.key());
            hdl.write(&val).await.unwrap();
        }
    };

    // Run until stopped
    let _ = select(worker_fut, stopper.wait()).await;
}
