# Properties of NOR flash
- Read/Write works byte-wise
    - **BUT:** modern NOR flash chips have **ECC** (error correction), which is invalidated when writing individual bits/bytes
        - e.g. the MX25UM51345G on the i.MX RT600 Evaluation Kit has ECC per 16-byte chunk (16 bytes/page)
- Erase (set all bits to 1) works only sector-wise, usually 4 KiB/sector

# Naming Convention
- **Word:** Smallest addressable unit for programming operations (8/16/32 bits).
- **Page:** Buffer unit for optimized programming, typically 256 bytes.
- **Sector:** Minimum erasable unit, typically 4 KiB.
- **Block:** Group of sectors that can be erased together, typically 64 KiB.

# How to store list nodes (= the actual config data)
## Ideas (so far)
- Use [ekv](https://github.com/embassy-rs/ekv), now that we have somewhere to store pending writes (in their intrusive node) (see 2025-05-07.md)
    - uses skiplists, `O(log n)` read and `O(log n)` write
    - "works best" with >1000 keys -> we expect only 50-100
        - "If the dataset fits in RAM, you could read/write it all at once. Either making it repr(C) and transmuting, or serializing it with some compact serde flavor such as postcard"
- Tock's [tickv](https://github.com/tock/tock/blob/master/libraries/tickv/SPEC.md)
    - Relies on single-byte writes -> not desirable with ECC on modern NOR flash
- Use something like sequential-storage::Queue, and ALWAYS write ALL values
    - Only a good option if the data fits in 1 sector (or 2 max),
    - Lookups are `O(1)`, after an initial `O(n)` "cursor scan" (see 2025-05-07.md)


## Considerations for Read/Write
- _Presumably_ we want to leverage **ECC** if the chip has it and **program page-wise only** (no individual bit/byte writes!)
    - Avoid writing small chunks of data, but buffer multiple writes (which the intrusive list approach does).
    - CRC is not needed with on-chip ECC, but if the flash chip does not offer ECC, we _likely_ should have checksums (optional feature)
    - If we always write **the entire list**, this is not an issue because we will write one huge chunk, spanning multiple pages. -> **Is there any reason to NOT do it like this?**
- Still there is the question: **how do we find the keys (and values) when hydrating nodes?**

### Approach 1: Store metadata in sector header
- Store header information for the entire sector at its beginning, i.e.,
    ```
    some_metadata: (to be defined if needed)
    key1: u64 (could be combined with offset?)
    offset1: u16 (not all bits used!)
    key2: u64,
    ...
    ```
    ```
          Sector 0
    ┌───────────────────┐
    │     metadata      │
    │┌─────────────────┐│
    ││key0: u64,       ││
    ││offset0: u16,    ││
    ││key1: u64,       ││
    ││offset1: u16,    ││
    ││...              ││
    ││                 ││
    ││                 ││
    │└─────────────────┘│
    │     payload       │
    │┌─────────────────┐│
    ││data0: [u8; len],││
    ││data1: [u8; len],││
    ││...              ││
    ││                 ││
    ││                 ││
    │└─────────────────┘│
    │                   │
    │                   │
    │                   │
    └───────────────────┘
    ```
- **Pros:**
    - no need to deserialize anything that has not been requested (i.e., if node/key not attached to list)
    - easy to cache keys + offsets in RAM
- **Cons:**
    - wastes space for offset values
    - **crossing sectors** not very elegant
        - need to write **"sector header"**, which makes using leftover space on the sector for a new list-revision a bit awkward (how do we invalidate the old list and tell the reader where the new one begins?)
        - **Alternative:** write all metadata in the same sector and if `offset > SECTOR_SIZE` the payload must lie in the next sector. This would also allow payloads to span multiple sectors quite easily. However, if only the **last** payload crosses the sector boundary, this is not immediately obvious before deserializing.
            - Idea: Instead of storing only _offsets_, store the length of the metadata/header and the _length_ for every payload
- If we want to make **incremental changes**, i.e., add/update single nodes, we have to care about page boundaries and how we invalidate old entries when the new ones have been written.
    - What if a page contains **multiple nodes**? -> Both have to be re-written. If the size increases, they might now span **multiple pages**.

### Approach 2: Store as list/log
Start somewhere (details tbd) and append the nodes one by one, and deserialize one by one. That is: deserialize the first node, then the second, and so on. This way, we can easily handle payloads that span multiple sectors.

## R/W procedure
### Read
- Node with key is attached to the StorageList by a _user_
- Worker searches for valid key -> **How?** Where do we start? Should we do a full scan at startup and find all latest/valid keys?

### Write
- Do we want/need CRC? Where should it be stored? -> Might be unnecessary with ECC memory, though. So rather make it an optional feature?
