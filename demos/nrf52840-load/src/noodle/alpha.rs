use cfg_noodle::StorageListNode;
use defmt::Format;
use embassy_executor::task;
use embassy_futures::select::select;
use embassy_time::{Duration, Ticker};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};

use super::LIST;

#[derive(Debug, Format, Encode, Decode, CborLen, Clone)]
pub struct AlphaConfig {
    #[n(0)]
    data_a: [u8; 32],
    #[n(1)]
    data_b: [u8; 16],
    #[n(2)]
    data_c: [u8; 8],
    #[n(3)]
    data_d: [u8; 8],
}

pub static NODES: [StorageListNode<AlphaConfig>; 10] = [
    StorageListNode::new("alpha/one"),
    StorageListNode::new("alpha/two"),
    StorageListNode::new("alpha/three"),
    StorageListNode::new("alpha/four"),
    StorageListNode::new("alpha/five"),
    StorageListNode::new("alpha/six"),
    StorageListNode::new("alpha/seven"),
    StorageListNode::new("alpha/eight"),
    StorageListNode::new("alpha/nine"),
    StorageListNode::new("alpha/ten"),
];

pub static INITS: [fn() -> AlphaConfig; 10] = [
    || AlphaConfig { data_a: [10; 32], data_b: [11; 16], data_c: [12; 8], data_d: [13; 8]  },
    || AlphaConfig { data_a: [20; 32], data_b: [21; 16], data_c: [22; 8], data_d: [23; 8]  },
    || AlphaConfig { data_a: [30; 32], data_b: [31; 16], data_c: [32; 8], data_d: [33; 8]  },
    || AlphaConfig { data_a: [40; 32], data_b: [41; 16], data_c: [42; 8], data_d: [43; 8]  },
    || AlphaConfig { data_a: [50; 32], data_b: [51; 16], data_c: [52; 8], data_d: [53; 8]  },
    || AlphaConfig { data_a: [60; 32], data_b: [61; 16], data_c: [62; 8], data_d: [63; 8]  },
    || AlphaConfig { data_a: [70; 32], data_b: [71; 16], data_c: [72; 8], data_d: [73; 8]  },
    || AlphaConfig { data_a: [80; 32], data_b: [81; 16], data_c: [82; 8], data_d: [83; 8]  },
    || AlphaConfig { data_a: [90; 32], data_b: [91; 16], data_c: [92; 8], data_d: [93; 8]  },
    || AlphaConfig { data_a: [100; 32], data_b: [101; 16], data_c: [102; 8], data_d: [103; 8]  },
];

#[task(pool_size = 10)]
pub async fn alpha_worker(
    node: &'static StorageListNode<AlphaConfig>,
    init: fn() -> AlphaConfig,
    interval: Duration,
    stopper: &'static WaitQueue,
) {
    let mut hdl = node.attach_with_default(&LIST, init).await.unwrap();
    let mut val = hdl.load();
    let mut ticker = Ticker::every(interval);

    let worker_fut = async {
        loop {
            ticker.next().await;
            let AlphaConfig { data_a, data_b, data_c, data_d } = &mut val;
            let iter = data_a.iter_mut()
                .chain(data_b.iter_mut())
                .chain(data_c.iter_mut())
                .chain(data_d.iter_mut());

            // Just increment every byte on every tick
            for byte in iter {
                *byte = byte.wrapping_add(1);
            }

            defmt::debug!("{=str} writing...", hdl.key());
            hdl.write(&val).await.unwrap();
        }
    };

    // Run until stopped
    let _ = select(worker_fut, stopper.wait()).await;
}
