# Changelog

(DD-MM-YY)

## Unreleased

- *Breaking:* Added const generic to list that controls how much redundancy is stored. To keep the same behavior as before, use the value 3.

## 0.5.0 03-09-25

- *Breaking:* Update to embassy-time 0.5

## 0.4.0 31-07-25

- *Breaking:* Updated sequential-storage to 5.0.0 and minicbor to 2.0.0
  - Both are very minor major releases
- Removed 'static trait bounds for NdlDataStorage for Flash and NdlElemIter
- Make it compile on windows
- Expose the extracted kv-pair to the outside world so people can look at the cbor bytes
