# Requirements for NVM Configuration Service

NOTE: See [embedded-services#267] for more discussion.

[embedded-services#267]: https://github.com/OpenDevicePartnership/embedded-services/issues/267

We will need the ability to store non-volatile data for use with configuration or similar data.

## Hard Requirements

This service will need the following qualities ("Hard Requirements"):

* Compatible with external NOR flash, for example SPI/QSPI flash parts
  * Should be compatible with other storage technologies
* Support for basic create/read/update/delete operations
* Ability to perform write/update operations in a power-loss-safe method without corruption
  and minimal data loss (e.g. only unsynced changes at power loss are lost, no older/existing data is lost)
* Support for concurrent/shared firmware access
* Support for "addressable" storage, e.g. a file path, storage key, etc.
* Ability to mitigate impact of memory wear
* Compatible with low-power operation
* Ability to support "roll forward" and "roll backwards" operations associated with a firmware update or rollback.
    * This may entail a "schema change" or "migration" of stored data
    * In the event of a firmware rollback, it should be possible to use data stored prior to the "migration"
    * In the event of a firmware update, it should be possible to use data stored prior to the "migration"
* Handling of Serialization/Deserialization steps, e.g. provide a "rust data" interface, translating
  to/from the "wire format" stored at rest

## Potential requirements

The following are items that have been discussed, but may not be hard requirements, or may require
additional scoping. This is to be discussed.

* Ability to "secure erase" individual files/records
    * This would be to ensure certain data could not be retrieved after deletion,
      but before the physical storage page has been re-used and fully erased
    * How would this feature interact with roll forward or roll back abilities?
    * Is this feature also power-loss-safe? e.g. loss of power AFTER "soft"
      delete occurs, but before "secure" delete occurs?
* Ability to handle "bad block" detection, vs "general wear leveling"
    * NOR flash is less susceptible than other mediums like NAND flash for random bad blocks
    * Do we need to handle resiliency in the face of individual bad blocks, or only adopt a
      general wear-leveling pattern to avoid changes to common locations?
    * littlefs handles this by reading-back after every write, and picking a new location if
      the write fails. This is less efficient than keeping a running table of bad blocks,
      but also less complex)

## Unknown Qualities

The following are unknown items that may help guide decisions made for implementation.

### How broad of a scope is "Configuration Storage"?

Configuration data is often relatively small (e.g. 10s-100s of bytes), and is often "write once"
(e.g. not appended to, like logs).

This means that there may be significant space efficiencies by allowing files to be packed into a
single page/sector (typical: 4K/64K for NOR flash), instead of allocating a minimum file size.

However this also means that there might be significantly diminishing returns on supporting any kind
of "delta" changes, where only a small portion of a file is updated, and we could avoid re-writing
the whole file.

However, it would be good to know if this is "just" for configuration, or if scope may expand to
other "just one more thing" sort of use cases where we need some kind of non-volatile storage.

This may impact whether we want a more-specialized solution optimized for configuration and tightly
integrated, or a more-generalized file/kv store that may miss some potential room for optimization,
but which has a more general API that is easier to use in different use cases.

### What kind of access patterns are we expecting?

Particularly with respect to the SIZE of files, how often MINOR changes are written (where it would
be worth avoiding full-rewrites of a file), and how often TOTAL changes are written. Also validating
that in general, we expect to READ configuration (much) more often than we WRITE configuration.

This relates back to the previous question re: "scope broadness", but could still be a concern even
for "just" configuration data.

If we have a strong idea of what our access patterns are, we may be able to better optimize or
simplify our storage service, however these optimizations or simplifications are mismatched to our
actual access patterns, they may work against us instead of for us.

### How to implement storage, and what "layers" do we expect?

Depending on answers to previous questions, I can see three main ways to implement the configuration
storage service:

1. A specific implementation that handles things from the high level API down to the low level flash
   operations. It will need to handle metadata, performing reads/erases of specific physical flash
   locations, etc.
2. Separate the "configuration API" from the "disk storage layer". This means that the configuration
   subsystem doesn't need to know "how" data is stored, and instead thinks in terms of "files" or
   "records. It would be built on top of some kind of storage layer, like:
    * a) A general flash filesystem ("ffs") that provides "files" and potentially "folders", e.g.
      littlefs, spiffs, pebblefs, etc.
    * b) A general key:value store ("kvs" that provides "records", acting similar to a
      `HashMap<Vec<u8>, Vec<u8>>`, e.g. sequential-storage::Map, ekv, etc.

If we ONLY need configuration storage (in some scope-able usage pattern), it's likely most
reasonable to go with option 1: we can tightly integrate the operation of the storage API with the
on-disk format, allowing us to have a smaller code footprint, and ideally fewer "integration seams"
that can lead to bugs.

If we are likely to need storage for other purposes, it might make sense to extract the common "disk
storage layer", and use a common ffs/kvs layer, to reduce total code size and "moving parts", and
implement configuration as a layer on top of that.

However this "sharing" is to be taken with a grain of salt: if our use cases are different enough,
e.g. a "large, append-only log store" and a "small, write-once-read-many config store", then
"sharing" the ffs/kvs layer may actually end up worse off than have two smaller bespoke
implementations. This "false sharing" case should be avoided, as it will make the ffs/kvs layer more
complicated than it would otherwise need to be.
