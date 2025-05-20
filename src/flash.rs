use cordyceps::List;
use core::{ops::Range, str};
use embedded_storage_async::nor_flash::MultiwriteNorFlash;
use mutex::ScopedRawMutex;
use sequential_storage::cache::NoCache;
use sequential_storage::queue;
use std::{num::Wrapping, ptr::NonNull};

use crate::intrusive::{Node, NodeHeader, State, StorageList};

struct Flash<T: MultiwriteNorFlash> {
    flash: T,
    flash_range: Range<u32>,
}

impl<T: MultiwriteNorFlash> Flash<T> {
    pub(crate) async fn read(&mut self, list: &List<NodeHeader>, buf: &mut [u8]) -> Result<(), ()> {
        // Create a `QueueIterator` without caching
        let mut cache = NoCache::new();
        let mut queue_iter = queue::iter(&mut self.flash, self.flash_range.clone(), &mut cache)
            .await
            .unwrap();

        let mut write_confirm_found = false;
        while let Some(item) = queue_iter.next(buf).await.unwrap() {
            if write_confirm_found {
                todo!()
                // This should not happen, it means that we have more data after a write_confirm
                // and the old data has not been deleted.
                // In the best case, we find another write_confirm later on and just throw away
                // the old data (delete it later).
            }
            // Extract metadata and payload from queue item
            let key: &str = str::from_utf8(&item[0..4]).unwrap();
            let counter: u8 = item[4];
            let payload = &item[5..];

            // Check for the write_confirm key
            if key == "WRITE-CONFIRM" {
                write_confirm_found = true;
            } else if let Some(node_header) = find_key(list, key) {
                if node_header.counter.is_none() {
                    node_header.counter.replace(Wrapping(counter));
                } else {
                    todo!("Handle this case")
                }
                let nodeptr: NonNull<Node<()>> = NonNull::dangling(); // TODO: add cast from header
                let res = (node_header.vtable.deserialize)(nodeptr, payload);
            }
        }

        set_non_resident(list);

        // TODO: We should only do this iteration once after startup
        // Maybe make this a two-step process: first, find the latest counter (and maybe cache the keys?)
        // second, hydrate the list nodes
        Ok(())
    }

    pub async fn write(&mut self, list: &mut List<NodeHeader>, buf: &mut [u8]) -> Result<(), ()> {
        // Set to true if at least one node has `State::NeedsWrite`
        let mut write_needed: bool = false;

        // Iterate over the list and write all nodes to flash
        for node in list {
            // ... does this node need writing?
            let vtable = {
                match node.state {
                    State::Initial => None,
                    State::NonResident => None,
                    State::DefaultUnwritten => None,
                    State::ValidNoWriteNeeded => None,
                    State::NeedsWrite => Some((node.vtable, node.key, node.counter)),
                    // TODO: the demo doesn't handle "writestarted" at all
                    State::WriteStarted => panic!("shouldn't be here"),
                }
            };
            // yes, it needs writing
            if let Some((vtable, key, counter)) = vtable {
                // See `process_reads` for the tricky safety caveats here!
                // TODO: Miri gives an error here. Investigate whether this is correct and fix it.
                let mut hdrptr: NonNull<NodeHeader> =
                    NonNull::from(unsafe { core::pin::Pin::into_inner_unchecked(node) });
                let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                
                // Attempt to serialize
                let res = (vtable.serialize)(nodeptr, &mut buf[33..]);

                if let Ok(used) = res {
                    node.counter
                    buf[0..32].copy_from_slice(key[0..32].as_bytes());
                    buf[32] = counter.map(|c| c.0).unwrap_or(0);
                    // "Store" in our "flash"
                    let res = queue::push(
                        &mut self.flash,
                        self.flash_range.clone(),
                        &mut NoCache::new(),
                        &buf[..used],
                        false,
                    )
                    .await;
                    assert!(res.is_ok());

                    // Mark the store as complete, wake the node if it cares about state
                    // changes.
                    // TODO: will nodes ever care about state changes other than
                    // the initial hydration?
                    let hdrmut = unsafe { hdrptr.as_mut() };
                    hdrmut.state = State::ValidNoWriteNeeded;
                } else {
                    // I'm honestly not sure what we should do if serialization failed.
                    // If it was a simple "out of space" because we have a batch of writes
                    // already, we might want to try again later. If it failed for some
                    // other reason, or it fails even with an empty page (e.g. serializes
                    // to more than 1k or 4k or something), there's really not much to be
                    // done, other than log.
                    panic!("why did ser fail");
                }
            }
        }

        // TODO: Read back the data to check if write has been successful

        let counter: u8 = 0; // TODO: which counter should we use?

        let mut confirm_buf = [0u8; 33];
        confirm_buf[0..13].copy_from_slice(b"WRITE-CONFIRM");
        confirm_buf[32] = counter;

        queue::push(
            &mut self.flash,
            self.flash_range.clone(),
            &mut NoCache::new(),
            confirm_buf.as_mut_slice(),
            false,
        );
        Ok(())
    }
}

fn find_key<'a>(list: &'a List<NodeHeader>, key: &str) -> Option<&'a mut NodeHeader> {
    todo!()
}

fn set_non_resident(list: &List<NodeHeader>) {
    todo!("Set state to NonResident whenever the counter is None")
}
