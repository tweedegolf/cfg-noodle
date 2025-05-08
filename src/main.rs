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
    tokio::task::spawn(task_2(&GLOBAL_LIST));
    tokio::task::spawn(task_3(&GLOBAL_LIST));

    let mut flash = HashMap::<String, Vec<u8>>::new();
    flash.insert(
        "encabulator/config".to_string(),
        minicbor::to_vec(&EncabulatorConfigV1 { polarity: true }).unwrap(),
    );
    flash.insert(
        "grammeter/config".to_string(),
        minicbor::to_vec(&GrammeterConfig { radiation: 100.0 }).unwrap(),
    );
    // no positron config

    // give time for tasks to attach
    sleep(Duration::from_millis(100)).await;
    // process reads
    GLOBAL_LIST.process_reads(&flash);

    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;
        let mut flash2 = HashMap::<String, Vec<u8>>::new();
        GLOBAL_LIST.process_writes(&mut flash2);
        println!("NEW WRITES: {flash2:?}");
    }

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

//
// TASK 2: Has config, current version
//
#[derive(Debug, Default, Encode, Decode, Clone)]
struct GrammeterConfig {
    #[n(0)] radiation: f32,
}

static GRAMM_CONFIG: PinListNode<GrammeterConfig, CriticalSectionRawMutex> = PinListNode::new("grammeter/config");
async fn task_2(list: &'static PinList<CriticalSectionRawMutex>) {
    GRAMM_CONFIG.attach(list).await.unwrap();
    let data: GrammeterConfig = GRAMM_CONFIG.load();
    println!("T2 Got {data:?}");
    sleep(Duration::from_secs(3)).await;
    GRAMM_CONFIG.write(&GrammeterConfig { radiation: 200.0 });
}

//
// TASK 3: No config
//
#[derive(Debug, Encode, Decode, Clone)]
struct PositronConfig {
    #[n(0)] up: u8,
    #[n(1)] down: u16,
    #[n(2)] strange: u32,
}

impl Default for PositronConfig {
    fn default() -> Self {
        Self {
            up: 10,
            down: 20,
            strange: 103,
        }
    }
}

static POSITRON_CONFIG: PinListNode<PositronConfig, CriticalSectionRawMutex> = PinListNode::new("positron/config");
async fn task_3(list: &'static PinList<CriticalSectionRawMutex>) {
    POSITRON_CONFIG.attach(list).await.unwrap();
    let data: PositronConfig = POSITRON_CONFIG.load();
    println!("T3 Got {data:?}");
    sleep(Duration::from_secs(5)).await;
    POSITRON_CONFIG.write(&PositronConfig { up: 15, down: 25, strange: 108 });
}
