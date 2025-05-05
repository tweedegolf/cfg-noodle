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

