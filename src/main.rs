use std::{collections::HashMap, time::Duration};

use intrusive::{PinList, PinListNode};
use minicbor::{Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::time::sleep;

pub mod intrusive;
pub mod hashmap;

#[tokio::main]
async fn main() {
    tokio::task::spawn(task_1(&GLOBAL_LIST));

    let mut flash = HashMap::<String, Vec<u8>>::new();
    flash.insert(
        "encabulator/config".to_string(),
        minicbor::to_vec(&EncabulatorConfigV1 { polarity: true }).unwrap(),
    );

    GLOBAL_LIST.process_reads(&flash);
    sleep(Duration::from_secs(2)).await;
    let mut flash2 = HashMap::<String, Vec<u8>>::new();
    GLOBAL_LIST.process_writes(&mut flash2);
    println!("NEW WRITES: {flash2:?}");
    sleep(Duration::from_secs(2)).await;
    let mut flash2 = HashMap::<String, Vec<u8>>::new();
    GLOBAL_LIST.process_writes(&mut flash2);
    println!("NEW WRITES: {flash2:?}");
}

static GLOBAL_LIST: PinList<CriticalSectionRawMutex> = PinList::new();

//
// TASK 1: Has config, but an old version
//
#[derive(Debug, Default, Encode, Decode, Clone)]
struct EncabulatorConfigV1 {
    #[n(0)] polarity: bool,
}

#[derive(Debug, Default, Encode, Decode, Clone)]
struct EncabulatorConfigV2 {
    #[n(0)] polarity: bool,
    #[n(1)] spinrate: Option<u32>,
}

static ENCAB_CONFIG: PinListNode<EncabulatorConfigV2, CriticalSectionRawMutex> = PinListNode::new("encabulator/config");
async fn task_1(list: &'static PinList<CriticalSectionRawMutex>) {
    ENCAB_CONFIG.attach(list).await.unwrap();
    let data: EncabulatorConfigV2 = ENCAB_CONFIG.load();
    println!("T1 Got {data:?}");
    sleep(Duration::from_secs(1)).await;
    ENCAB_CONFIG.write(&EncabulatorConfigV2 { polarity: true, spinrate: Some(100) });
}
