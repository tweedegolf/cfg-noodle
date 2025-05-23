use std::time::Duration;

use cfg_noodle::intrusive::{Flash, StorageList, StorageListNode};
use log::{error, info};
use minicbor::{CborLen, Decode, Encode};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use sequential_storage::mock_flash::WriteCountCheck;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    simple_logger::SimpleLogger::new()
        .without_timestamps()
        .init()
        .unwrap();
    tokio::task::spawn(task_1(&GLOBAL_LIST));
    tokio::task::spawn(task_2(&GLOBAL_LIST));
    tokio::task::spawn(task_3(&GLOBAL_LIST));

    //let mut flash = HashMap::<String, Vec<u8>>::new();
    let mut flash = Flash::new(
        sequential_storage::mock_flash::MockFlashBase::<10, 16, 256>::new(
            WriteCountCheck::OnceOnly,
            None,
            true,
        ),
        0x0000..0x1000,
    );
    let range = flash.range();

    sequential_storage::erase_all(&mut flash.flash(), range)
        .await
        .unwrap();

    // give time for tasks to attach
    sleep(Duration::from_millis(100)).await;
    // process reads
    let buf = &mut [0u8; 4096];
    GLOBAL_LIST.process_reads(&mut flash, buf).await;

    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;
        let mut flash2 = Flash::new(
            sequential_storage::mock_flash::MockFlashBase::<10, 16, 256>::new(
                WriteCountCheck::OnceOnly,
                None,
                true,
            ),
            0x0000..0x1000,
        );
        if let Err(e) = GLOBAL_LIST.process_writes(&mut flash2, buf).await {
            error!("Error in process_writes: {}", e);
        }
        info!("NEW WRITES: {}", flash2.flash().print_items().await);
    }
}

static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();

//
// TASK 1: Has config, but an old version
//
#[derive(Debug, Default, Encode, Decode, Clone, CborLen)]
struct EncabulatorConfigV1 {
    #[n(0)]
    polarity: bool,
}

#[derive(Debug, Default, Encode, Decode, Clone, CborLen)]
struct EncabulatorConfigV2 {
    #[n(0)]
    polarity: bool,
    #[n(1)]
    spinrate: Option<u32>,
}

static ENCAB_CONFIG: StorageListNode<EncabulatorConfigV2> =
    StorageListNode::new("encabulator/config");
async fn task_1(list: &'static StorageList<CriticalSectionRawMutex>) {
    let config_handle = ENCAB_CONFIG.attach(list).await.unwrap();
    let data: EncabulatorConfigV2 = config_handle.load().await.unwrap();
    println!("T1 Got {data:?}");
    sleep(Duration::from_secs(1)).await;
    config_handle
        .write(&EncabulatorConfigV2 {
            polarity: true,
            spinrate: Some(100),
        })
        .await
        .unwrap();
}

//
// TASK 2: Has config, current version
//
#[derive(Debug, Default, Encode, Decode, Clone, CborLen)]
struct GrammeterConfig {
    #[n(0)]
    radiation: f32,
}

static GRAMM_CONFIG: StorageListNode<GrammeterConfig> = StorageListNode::new("grammeter/config");
async fn task_2(list: &'static StorageList<CriticalSectionRawMutex>) {
    let config_handle = GRAMM_CONFIG.attach(list).await.unwrap();
    let data: GrammeterConfig = config_handle.load().await.unwrap();
    println!("T2 Got {data:?}");
    sleep(Duration::from_secs(3)).await;
    config_handle
        .write(&GrammeterConfig { radiation: 200.0 })
        .await
        .unwrap();
}

//
// TASK 3: No config
//
#[derive(Debug, Encode, Decode, Clone, CborLen)]
struct PositronConfig {
    #[n(0)]
    up: u8,
    #[n(1)]
    down: u16,
    #[n(2)]
    strange: u32,
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

static POSITRON_CONFIG: StorageListNode<PositronConfig> = StorageListNode::new("positron/config");

async fn task_3(list: &'static StorageList<CriticalSectionRawMutex>) {
    let config_handle = POSITRON_CONFIG.attach(list).await.unwrap();
    let data: PositronConfig = config_handle.load().await.unwrap();
    println!("T3 Got {data:?}");
    sleep(Duration::from_secs(5)).await;
    config_handle
        .write(&PositronConfig {
            up: 15,
            down: 25,
            strange: 108,
        })
        .await
        .unwrap();
}
