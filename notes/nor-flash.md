# Properties of NOR flash
- Read/Write works byte-wise -> **BUT:** modern NOR flash chips have per-page ECC, which is usually invalidated when writing fewer bytes than the page size
- Erase (set all bits to 1) works only sector-wise, usually 4 KiB/sector

## Naming Conventions
- **Word:** Smallest addressable unit for programming operations (8/16/32 bits).
- **Page:** Buffer unit for optimized programming, typically 256 bytes.
- **Sector:** Minimum erasable unit, typically 4 KiB.
- **Block:** Group of sectors that can be erased together, typically 64 KiB.

# How to store list nodes
## Ideas (so far)
- Use [ekv](https://github.com/embassy-rs/ekv), now that we have somewhere to store pending writes (in their intrusive node) (see 2025-05-07.md)
    - uses skiplists, O(log n) read and O(log n) write
    - "works best" with >1000 keys -> we expect only 50-100
        - "If the dataset fits in RAM, you could read/write it all at once. Either making it repr(C) and transmuting, or serializing it with some compact serde flavor such as postcard"
- Tock's [tickv](https://github.com/tock/tock/blob/master/libraries/tickv/SPEC.md)
    - Relies on single-byte writes -> not desirable with ECC on modern NOR flash
- Use something like sequential-storage::Queue, and ALWAYS write ALL values
    - Only a good option if the data fits in 1 sector (or 2 max), 
    - Lookups are O(1), after an initial O(n) "cursor scan" (see 2025-05-07.md)


## Handling Pages
- Presumably we would want to use ECC if the chips have it and program page-wise only (no individual bit/byte writes!). Therefore we should avoid writing small chunks of data, but collect data to be written.
- If we always write **the entire list**, this is not an issue because we will write one huge chunk, spanning multiple pages. -> **Is there any reason to NOT do it like this?**
    - Still there is the question: how do we find the keys (and values) when hydrating nodes?
    - One approach is to store header information for the entire sector at its beginning, i.e.,
        ```
        key1: u64 (could be combined with offset?)
        offset1: u16 (not all bits used!)
        key2 ...
        ...
        data 1: [u8; offset2 - offset1]
        ```
        - Pros:
            - no need to deserialize anything that has not been requested (= node not attached to list)
            - caching keys + offsets is easy
        - Cons:
            - wasting space for offset values
            - crossing sectors not very elegant -> need to write "sector header", which makes using leftover space on the sector for a new list-revision a bit awkward (how do we invalidate the old list and tell the reader where the new one begins?)
- If we want to make **incremental changes**, i.e., add/update single nodes, we have to care about page boundaries and how we invalidate old entries when the new ones have been written.
    - What if a page contains **multiple nodes**? -> Both have to be re-written. If the size increases, they might now span **multiple pages**.



## R/W procedure
### Read
- Node with key is attached to the StorageList
- Search for valid key -> **How?** Where do we start? Should we do a full scan at startup and find all latest/valid keys?

### Write
- Do we want/need CRC? Where should it be stored? -> Might be unnecessary with ECC memory, though. So rather make it an optional feature?
