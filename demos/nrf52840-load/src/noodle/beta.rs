use cfg_noodle::StorageListNode;
use defmt::Format;
use embassy_executor::task;
use embassy_futures::select::select;
use embassy_time::{Duration, Ticker};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};

use super::LIST;

#[derive(Debug, Format, Encode, Decode, CborLen, Clone)]
pub struct BetaConfig {
    #[n(0)]
    data_a: [u32; 8],
    #[n(1)]
    data_b: [u32; 4],
    #[n(2)]
    data_c: [u32; 2],
    #[n(3)]
    data_d: [u32; 2],
}

pub static NODES: [StorageListNode<BetaConfig>; 10] = [
    StorageListNode::new("beta/one"),
    StorageListNode::new("beta/two"),
    StorageListNode::new("beta/three"),
    StorageListNode::new("beta/four"),
    StorageListNode::new("beta/five"),
    StorageListNode::new("beta/six"),
    StorageListNode::new("beta/seven"),
    StorageListNode::new("beta/eight"),
    StorageListNode::new("beta/nine"),
    StorageListNode::new("beta/ten"),
];

pub static INITS: [fn() -> BetaConfig; 10] = [
    || BetaConfig { data_a: [110; 8], data_b: [111; 4], data_c: [112; 2], data_d: [113; 2]  },
    || BetaConfig { data_a: [120; 8], data_b: [121; 4], data_c: [122; 2], data_d: [123; 2]  },
    || BetaConfig { data_a: [130; 8], data_b: [131; 4], data_c: [132; 2], data_d: [133; 2]  },
    || BetaConfig { data_a: [140; 8], data_b: [141; 4], data_c: [142; 2], data_d: [143; 2]  },
    || BetaConfig { data_a: [150; 8], data_b: [151; 4], data_c: [152; 2], data_d: [153; 2]  },
    || BetaConfig { data_a: [160; 8], data_b: [161; 4], data_c: [162; 2], data_d: [163; 2]  },
    || BetaConfig { data_a: [170; 8], data_b: [171; 4], data_c: [172; 2], data_d: [173; 2]  },
    || BetaConfig { data_a: [180; 8], data_b: [181; 4], data_c: [182; 2], data_d: [183; 2]  },
    || BetaConfig { data_a: [190; 8], data_b: [191; 4], data_c: [192; 2], data_d: [193; 2]  },
    || BetaConfig { data_a: [1100; 8], data_b: [1101; 4], data_c: [1102; 2], data_d: [1103; 2]  },
];

#[task(pool_size = 10)]
pub async fn beta_worker(
    node: &'static StorageListNode<BetaConfig>,
    init: fn() -> BetaConfig,
    interval: Duration,
    stopper: &'static WaitQueue,
) {
    let mut hdl = node.attach_with_default(&LIST, init).await.unwrap();
    let mut val = hdl.load();
    let mut ticker = Ticker::every(interval);

    let worker_fut = async {
        loop {
            ticker.next().await;
            let BetaConfig { data_a, data_b, data_c, data_d } = &mut val;
            let iter = data_a.iter_mut()
                .chain(data_b.iter_mut())
                .chain(data_c.iter_mut())
                .chain(data_d.iter_mut());

            // Just increment every word on every tick
            for word in iter {
                *word = word.wrapping_add(1);
            }

            defmt::debug!("{=str} writing...", hdl.key());
            hdl.write(&val).await.unwrap();
        }
    };

    // Run until stopped
    let _ = select(worker_fut, stopper.wait()).await;
}
