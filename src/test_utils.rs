//! Test utilities - NOT covered by semver guarantees

#![allow(clippy::unwrap_used)]

use core::{fmt::Write as _, num::NonZeroU32};
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
    Crc32, Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData, flash::Flash,
    storage_list::StorageList,
};

/// A high level fake storage impl.
///
/// Does not attempt to fully mimic the actual flash representation, instead
/// only serves as an abstract queue of [`Elem`]s. This allows for more direct
/// control when unit/integration testing this library.
///
/// Consider using [`MockFlash`] when it is desirable to emulate in more depth.
///
/// Assigns a 64-bit counter to each element to aid in finding and removing
/// items (rather than needing a reference).
#[derive(Default)]
pub struct TestStorage {
    ctr: u64,
    pub items: Vec<TestItem>,
}

/// An iterator over a [`TestStorage`]
pub struct TestStorageIter<'a> {
    sto: &'a mut TestStorage,
    remain_items: VecDeque<TestItem>,
}

/// A single element of a [`TestStorage`]
pub struct TestStorageItemNode<'a> {
    sto: &'a mut TestStorage,
    item: TestItem,
}

/// The inner contents of a [`TestStorageItemNode`]
#[derive(Clone, PartialEq, Debug)]
pub struct TestItem {
    pub ctr: u64,
    pub elem: TestElem,
}

/// The test owned/heapful version of [`Elem`].
#[derive(Clone, PartialEq, Debug)]
pub enum TestElem {
    Start { seq_no: NonZeroU32 },
    Data { data: Vec<u8> },
    End { seq_no: NonZeroU32, calc_crc: u32 },
}

/// A builder type for writing a "Write Record" into a [`TestStorage`].
pub struct RecordWriter<'a> {
    sto: &'a mut TestStorage,
    seq: NonZeroU32,
    crc: Crc32,
}

/// The outcome of a flash worker
///
/// Returns the contents of the flash after joining, as well as other metadata
/// about its operation.
pub struct WorkerReport {
    pub flash: TestStorage,
    pub read_errs: usize,
    pub write_errs: usize,
}

/// A more detailed simulation of the flash
///
/// This implementation uses the built-in blanket impl for sequential-storage Queue as
/// a storage backend, and uses the existing [`MockFlashBase`] from the sequential-storage
/// testing utilities to back the storage.
///
/// Useful for more in-depth end to end simulation testing, compared to [`TestStorage`]
//
// TODO: This type, the get_mock_flash() and worker_task() are not only specific to sequential storage's
// mock flash, but also copy&pasted in the integration tests. If we go with the flash-trait approach,
// these could be generic.
pub type MockFlash = Flash<MockFlashBase<10, 16, 256>, NoCache>;

// ---- impl TestStorage ----

impl TestStorage {
    /// Get the next element counter
    ///
    /// Note: this is only used by testing, not to be confused with the
    /// sequence number used for Write Records!
    ///
    /// Panics if you exhaust the full range `0..u64::MAX`.
    pub fn next_ctr(&mut self) -> u64 {
        let ctr = self.ctr;
        self.ctr = self.ctr.checked_add(1).unwrap();
        ctr
    }

    /// Print all items contained by this TestStorage
    pub fn print_items(&self) -> String {
        let mut out = String::new();
        for i in self.items.iter() {
            writeln!(&mut out, "- (ctr: {}): {:?}", i.ctr, i.elem).unwrap();
        }
        out
    }

    /// Insert a "Start" element into the queue, and return a record writer
    ///
    /// This returns a `RecordWriter` that can be used for writing a valid or
    /// mostly valid Write Record. It will keep track of the sequence number
    /// used and the running CRC.
    ///
    /// The Start element is still written if the RecordWriter is dropped without
    /// finalizing.
    pub fn start_write_record(&mut self, seq_no: NonZeroU32) -> RecordWriter<'_> {
        let ctr = self.next_ctr();
        self.items.push(TestItem {
            ctr,
            elem: TestElem::Start { seq_no },
        });
        RecordWriter {
            seq: seq_no,
            crc: Crc32::new(),
            sto: self,
        }
    }

    /// Insert a data element into the queue
    ///
    /// This method is only necessary to call when creating a malformed sequence directly,
    /// e.g. a Data element after an End without a Start.
    ///
    /// Use [`Self::start_write_record()`] and [`RecordWriter::add_data_elem()`] if you want
    /// to create a good record.
    pub fn add_data_elem<E>(&mut self, key: &str, data: &E, mut crc: Option<&mut Crc32>)
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

    /// Insert an End element into the queue
    ///
    /// This method is only necessary to call when creating a malformed sequence directly,
    /// e.g. an End element after another End without a Start.
    ///
    /// Use [`Self::start_write_record()`] and [`RecordWriter::end_write_record()`] if you want
    /// to create a good record.
    pub fn end_write_record(&mut self, seq_no: NonZeroU32, calc_crc: u32) {
        let ctr = self.next_ctr();
        self.items.push(TestItem {
            ctr,
            elem: TestElem::End { seq_no, calc_crc },
        });
    }
}

impl NdlDataStorage for TestStorage {
    type Iter<'this>
        = TestStorageIter<'this>
    where
        Self: 'this;

    type Error = ();

    async fn iter_elems<'this>(
        &'this mut self,
    ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as NdlElemIter>::Error> {
        // Create a clone of the items to make things easier.
        let remain_items = self.items.iter().cloned().collect();
        Ok(TestStorageIter {
            sto: self,
            remain_items,
        })
    }

    async fn push(&mut self, data: &Elem<'_>) -> Result<(), Self::Error> {
        info!("Pushing {data:?}");
        let ctr = self.next_ctr();
        let item: TestElem = data.into();
        self.items.push(TestItem { ctr, elem: item });
        Ok(())
    }
}

// ---- impl TestStorageIter ----

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

// ---- impl TestStorageItemNode ----

impl<'a> NdlElemIterNode for TestStorageItemNode<'a> {
    type Error = ();

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

    async fn invalidate(self) -> Result<(), Self::Error> {
        let item_ctr = self.item.ctr;
        // todo: find + remove might be faster, but whatever, test code
        self.sto.items.retain(|i| i.ctr != item_ctr);
        Ok(())
    }
}

// ---- impl TestElem ----

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

// ---- impl RecordWriter ----

impl RecordWriter<'_> {
    /// Add a data element, updating the running CRC properly
    pub fn add_data_elem<E>(&mut self, key: &str, data: &E)
    where
        E: minicbor::Encode<()>,
        E: minicbor::CborLen<()>,
    {
        let Self { sto, seq: _, crc } = self;
        sto.add_data_elem(key, data, Some(crc));
    }

    /// Finalize the write, writing an End element with the correct CRC and sequence number
    pub fn end_write_record(self) {
        let Self { sto, seq, crc } = self;
        let calc_crc = crc.finalize();
        sto.end_write_record(seq, calc_crc);
    }
}

// ---- impl WorkerReport ----

impl WorkerReport {
    /// Assert the report contains no known errors
    ///
    /// panics if an error was present
    pub fn assert_no_errs(&self) {
        let Self {
            flash: _,
            read_errs,
            write_errs,
        } = self;
        assert_eq!(*read_errs, 0);
        assert_eq!(*write_errs, 0);
    }
}

// ---- helper functions ----

/// Helper function for creating a sequential-storage based storage impl
pub fn get_mock_flash() -> MockFlash {
    let mut flash = MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true);
    // TODO: Figure out why miri tests with unaligned buffers and whether
    // this needs any fixing. For now just disable the alignment check in MockFlash
    flash.alignment_check = false;
    Flash::new(flash, 0x0000..0x1000, NoCache::new())
}

/// A simple worker task for sequential-storage based testing
///
/// Currently processes reads and writes automatically when signalled.
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

/// A simple worker task for [`TestStorage`] based testing
///
/// Creates a new empty [`TestStorage`]. Will stop executuion when
/// the stopper is awoken or closed. This can be done with [`WaitQueue::close()`].
pub async fn worker_task_tst_sto<R: ScopedRawMutex + Sync>(
    list: &'static StorageList<R>,
    stopper: Arc<WaitQueue>,
) -> WorkerReport {
    worker_task_tst_sto_custom(list, stopper, TestStorage::default()).await
}

/// A simple worker task for [`TestStorage`] based testing
///
/// The same as [`worker_task_tst_sto()`], but takes a flash, which can be pre-populated
/// with initial data
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
