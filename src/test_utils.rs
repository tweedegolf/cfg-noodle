#![allow(clippy::unwrap_used)]

use std::{collections::VecDeque, sync::Arc};

use log::{debug, error, info};
use maitake_sync::WaitQueue;
use minicbor::encode::write::{Cursor, EndOfSlice};
use mutex::ScopedRawMutex;
use sequential_storage::{
    cache::NoCache,
    mock_flash::{MockFlashBase, WriteCountCheck},
};
use tokio::select;

use crate::{
    flash::{Elem, Flash, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData},
    intrusive::{FakeCrc32, StorageList},
};

// TODO: This type, the get_mock_flash() and worker_task() are not only specific to sequential storage's
// mock flash, but also copy&pasted in the integration tests. If we go with the flash-trait approach,
// these could be generic.
type MockFlash = Flash<MockFlashBase<10, 16, 256>, NoCache>;

pub fn get_mock_flash() -> MockFlash {
    let mut flash = MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true);
    // TODO: Figure out why miri tests with unaligned buffers and whether
    // this needs any fixing. For now just disable the alignment check in MockFlash
    flash.alignment_check = false;
    Flash::new(flash, 0x0000..0x1000, NoCache::new())
}

#[derive(Clone, PartialEq, Debug)]
pub enum TestElem {
    Start { seq_no: u32 },
    Data { data: Vec<u8> },
    End { seq_no: u32, calc_crc: u32 },
}

impl From<&Elem<'_>> for TestElem {
    fn from(value: &Elem<'_>) -> Self {
        match value {
            Elem::Start { seq_no } => TestElem::Start { seq_no: *seq_no },
            Elem::Data { data } => TestElem::Data {
                data: {
                    let mut item = Vec::with_capacity(1 + data.key_val().len());
                    item.push(data.hdr());
                    item.extend_from_slice(data.key_val());
                    item
                },
            },
            Elem::End { seq_no, calc_crc } => TestElem::End {
                seq_no: *seq_no,
                calc_crc: *calc_crc,
            },
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct TestItem {
    pub ctr: u64,
    pub elem: TestElem,
}

#[derive(Default)]
pub struct TestStorage {
    ctr: u64,
    pub items: Vec<TestItem>,
}

pub struct RecordWriter<'a> {
    sto: &'a mut TestStorage,
    seq: u32,
    crc: FakeCrc32,
}

impl RecordWriter<'_> {
    pub fn add_data_elem<E>(&mut self, key: &str, data: &E)
    where
        E: minicbor::Encode<()>,
        E: minicbor::CborLen<()>,
    {
        let Self { sto, seq: _, crc } = self;
        sto.add_data_elem(key, data, Some(crc));
    }

    pub fn end_write_record(self) {
        let Self { sto, seq, crc } = self;
        let calc_crc = crc.finalize();
        sto.end_write_record(seq, calc_crc);
    }
}

impl TestStorage {
    pub fn next_ctr(&mut self) -> u64 {
        let ctr = self.ctr;
        self.ctr = self.ctr.checked_add(1).unwrap();
        ctr
    }

    pub fn print_items(&self) -> String {
        String::new()
    }

    pub fn start_write_record(&mut self, seq_no: u32) -> RecordWriter<'_> {
        let ctr = self.next_ctr();
        self.items.push(TestItem {
            ctr,
            elem: TestElem::Start { seq_no },
        });
        RecordWriter {
            seq: seq_no,
            crc: FakeCrc32::new(),
            sto: self,
        }
    }

    pub fn add_data_elem<E>(&mut self, key: &str, data: &E, mut crc: Option<&mut FakeCrc32>)
    where
        E: minicbor::Encode<()>,
        E: minicbor::CborLen<()>,
    {
        let ctr = self.next_ctr();
        let mut buffer = vec![1];
        let mut scratch = [0u8; 4096];
        {
            let mut cursor = Cursor::new(scratch.as_mut_slice());
            let res: Result<(), minicbor::encode::Error<EndOfSlice>> =
                minicbor::encode(key, &mut cursor);
            res.unwrap();
            let pos = cursor.position();
            if let Some(crc) = crc.as_mut() {
                crc.update(&scratch[..pos]);
            }
            buffer.extend_from_slice(&scratch[..pos]);
        }
        {
            let mut cursor = Cursor::new(scratch.as_mut_slice());
            let res: Result<(), minicbor::encode::Error<EndOfSlice>> =
                minicbor::encode(data, &mut cursor);
            res.unwrap();
            let pos = cursor.position();
            if let Some(crc) = crc.as_mut() {
                crc.update(&scratch[..pos]);
            }
            buffer.extend_from_slice(&scratch[..pos]);
        }
        self.items.push(TestItem {
            ctr,
            elem: TestElem::Data { data: buffer },
        });
    }

    fn end_write_record(&mut self, seq_no: u32, calc_crc: u32) {
        let ctr = self.next_ctr();
        self.items.push(TestItem {
            ctr,
            elem: TestElem::End { seq_no, calc_crc },
        });
    }
}

pub struct TestStorageIter<'a> {
    sto: &'a mut TestStorage,
    remain_items: VecDeque<TestItem>,
}

pub struct TestStorageItemNode<'a> {
    sto: &'a mut TestStorage,
    item: TestItem,
}

impl NdlDataStorage for TestStorage {
    type Iter<'this>
        = TestStorageIter<'this>
    where
        Self: 'this;

    type PushError = ();

    async fn iter_elems<'this>(
        &'this mut self,
    ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as crate::flash::NdlElemIter>::Error> {
        let remain_items = self.items.iter().cloned().collect();
        Ok(TestStorageIter {
            sto: self,
            remain_items,
        })
    }

    async fn push(&mut self, data: &Elem<'_>) -> Result<(), Self::PushError> {
        info!("Pushing {data:?}");
        let ctr = self.next_ctr();
        let item: TestElem = data.into();
        self.items.push(TestItem { ctr, elem: item });
        Ok(())
    }
}

impl<'a> NdlElemIter for TestStorageIter<'a> {
    type Item<'this, 'buf>
        = TestStorageItemNode<'this>
    where
        Self: 'this,
        Self: 'buf;

    type Error = ();

    async fn next<'iter, 'buf>(
        &'iter mut self,
        _buf: &'buf mut [u8],
    ) -> Result<Option<Option<Self::Item<'iter, 'buf>>>, Self::Error>
    where
        Self: 'buf,
        Self: 'iter,
    {
        if let Some(item) = self.remain_items.pop_front() {
            debug!("Popping {item:?}");
            Ok(Some(Some(TestStorageItemNode {
                item,
                sto: self.sto,
            })))
        } else {
            Ok(None)
        }
    }
}

impl<'a> NdlElemIterNode for TestStorageItemNode<'a> {
    type InvalidateError = ();

    fn data(&self) -> Elem<'_> {
        match &self.item.elem {
            TestElem::Start { seq_no } => Elem::Start { seq_no: *seq_no },
            TestElem::Data { data } => Elem::Data {
                data: SerData::from_existing(data),
            },
            TestElem::End { seq_no, calc_crc } => Elem::End {
                seq_no: *seq_no,
                calc_crc: *calc_crc,
            },
        }
    }

    async fn invalidate(self) -> Result<(), Self::InvalidateError> {
        let item_ctr = self.item.ctr;
        // todo: find + remove might be faster, but whatever, test code
        self.sto.items.retain(|i| i.ctr != item_ctr);
        Ok(())
    }
}

pub async fn worker_task_seq_sto<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    mut flash: MockFlash,
) {
    let mut buf = [0u8; 4096];
    loop {
        info!("worker_task waiting for needs_* signal");
        match embassy_futures::select::select(list.needs_read().wait(), list.needs_write().wait())
            .await
        {
            embassy_futures::select::Either::First(_) => {
                info!("worker task got needs_read signal");
                if let Err(e) = list.process_reads(&mut flash, &mut buf).await {
                    error!("Error in process_reads: {:?}", e);
                }
            }
            embassy_futures::select::Either::Second(_) => {
                info!("worker task got needs_write signal");
                if let Err(e) = list.process_writes(&mut flash, &mut buf).await {
                    error!("Error in process_writes: {:?}", e);
                }

                info!("Wrote to flash: {}", flash.flash().print_items().await);
            }
        }
    }
}

pub async fn worker_task_tst_sto<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    stopper: Arc<WaitQueue>,
) -> WorkerReport {
    worker_task_tst_sto_custom(list, stopper, TestStorage::default()).await
}

pub struct WorkerReport {
    pub flash: TestStorage,
    pub read_errs: usize,
    pub write_errs: usize,
}

impl WorkerReport {
    pub fn assert_no_errs(&self) {
        assert_eq!(self.read_errs, 0);
        assert_eq!(self.write_errs, 0);
    }
}

pub async fn worker_task_tst_sto_custom<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    stopper: Arc<WaitQueue>,
    mut flash: TestStorage,
) -> WorkerReport {
    let mut read_errs = 0;
    let mut write_errs = 0;

    let fut = async {
        let mut buf = [0u8; 4096];
        loop {
            info!("worker_task waiting for needs_* signal");
            match embassy_futures::select::select(
                list.needs_read().wait(),
                list.needs_write().wait(),
            )
            .await
            {
                embassy_futures::select::Either::First(_) => {
                    info!("worker task got needs_read signal");
                    if let Err(e) = list.process_reads(&mut flash, &mut buf).await {
                        read_errs += 1;
                        error!("Error in process_reads: {:?}", e);
                    }
                }
                embassy_futures::select::Either::Second(_) => {
                    info!("worker task got needs_write signal");
                    if let Err(e) = list.process_writes(&mut flash, &mut buf).await {
                        write_errs += 1;
                        error!("Error in process_writes: {:?}", e);
                    }

                    info!("Wrote to flash: {}", flash.print_items());
                }
            }
        }
    };
    select! {
        _ = stopper.wait() => {},
        _ = fut => {},
    };
    WorkerReport {
        flash,
        read_errs,
        write_errs,
    }
}
