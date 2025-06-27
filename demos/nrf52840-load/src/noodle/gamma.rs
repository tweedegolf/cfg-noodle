use cfg_noodle::StorageListNode;
use defmt::Format;
use embassy_executor::task;
use embassy_futures::select::select;
use embassy_time::{Duration, Ticker};
use maitake_sync::WaitQueue;
use minicbor::{CborLen, Decode, Encode};

use super::LIST;

#[derive(Debug, Format, Encode, Decode, CborLen, Clone)]
pub struct GammaConfig {
    #[n(0)]
    data_a: [u16; 16],
    #[n(1)]
    data_b: [u16; 8],
    #[n(2)]
    data_c: [u16; 4],
    #[n(3)]
    data_d: [u16; 4],
}

pub static NODES: [StorageListNode<GammaConfig>; 10] = [
    StorageListNode::new("gamma/one"),
    StorageListNode::new("gamma/two"),
    StorageListNode::new("gamma/three"),
    StorageListNode::new("gamma/four"),
    StorageListNode::new("gamma/five"),
    StorageListNode::new("gamma/six"),
    StorageListNode::new("gamma/seven"),
    StorageListNode::new("gamma/eight"),
    StorageListNode::new("gamma/nine"),
    StorageListNode::new("gamma/ten"),
];

pub static INITS: [fn() -> GammaConfig; 10] = [
    || GammaConfig { data_a: [310; 16], data_b: [311; 8], data_c: [312; 4], data_d: [313; 4]  },
    || GammaConfig { data_a: [320; 16], data_b: [321; 8], data_c: [322; 4], data_d: [323; 4]  },
    || GammaConfig { data_a: [330; 16], data_b: [331; 8], data_c: [332; 4], data_d: [333; 4]  },
    || GammaConfig { data_a: [340; 16], data_b: [341; 8], data_c: [342; 4], data_d: [343; 4]  },
    || GammaConfig { data_a: [350; 16], data_b: [351; 8], data_c: [352; 4], data_d: [353; 4]  },
    || GammaConfig { data_a: [360; 16], data_b: [361; 8], data_c: [362; 4], data_d: [363; 4]  },
    || GammaConfig { data_a: [370; 16], data_b: [371; 8], data_c: [372; 4], data_d: [373; 4]  },
    || GammaConfig { data_a: [380; 16], data_b: [381; 8], data_c: [382; 4], data_d: [383; 4]  },
    || GammaConfig { data_a: [390; 16], data_b: [391; 8], data_c: [392; 4], data_d: [393; 4]  },
    || GammaConfig { data_a: [3100; 16], data_b: [3101; 8], data_c: [3102; 4], data_d: [3103; 4]  },
];

#[task(pool_size = 10)]
pub async fn gamma_worker(
    node: &'static StorageListNode<GammaConfig>,
    init: fn() -> GammaConfig,
    interval: Duration,
    stopper: &'static WaitQueue,
) {
    let mut hdl = node.attach_with_default(&LIST, init).await.unwrap();
    let mut val = hdl.load();
    let mut ticker = Ticker::every(interval);

    let worker_fut = async {
        loop {
            ticker.next().await;
            let GammaConfig { data_a, data_b, data_c, data_d } = &mut val;
            let iter = data_a.iter_mut()
                .chain(data_b.iter_mut())
                .chain(data_c.iter_mut())
                .chain(data_d.iter_mut());

            // square every word on every tick
            for word in iter {
                *word = word.rotate_right(1);
            }

            defmt::debug!("{=str} writing...", hdl.key());
            hdl.write(&val).await.unwrap();
        }
    };

    // Run until stopped
    let _ = select(worker_fut, stopper.wait()).await;
}
