# Maxims:

* Forward compatibility means "we can add fields/variants"
    * it MAYBE means that we can remove fields
    * renames are destructive (remove + add)
* Adding fields means we need a default for every struct
* With respect to schemas:
    * If we have lots of copies of the same struct, then storing the schema once is good
    * If we have few copies of the same struct, then storing the schema "inline" is better
    * Storing the schema inline means that we need to rewrite the schema on updates

We could "just" use a format like protobuf, cbor, that already has in-place schemas.

Forward compatibility requires query-like behavior

We can't necessarily re-use keys to find old versions, since there's no guaranteed
preservation of data for old versions.

## Open questions:

* What to do with path/keys? They need to be owned, which means taking a &str is a pain
    * I don't want to use a heapless::String
    * Hashes have room for collisions
    * 64 bit hashes are often gross on embedded
    * idk
* We should probably do some kind of check that two pieces of code don't write different
  data to the same path.
    * We could try to load + decode on every write? This is not great
    * Now that we're using minicbor, we don't really have anything like postcard-schema
    * types might still mismatch, how far is too far?
