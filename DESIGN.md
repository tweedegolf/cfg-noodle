# Design Notes

## Decided Vocabulary

The atomic data item of **noodle** is an **element**, which is one of the following three items:

1. A **start element**, which notes a sequence number, and begins a "write record"
2. A **data element**, which includes a single k:v pair, mirrored in an in-memory node
3. An **end element**, which includes a sequence number, and a CRC of all data elements, and ends a **write record**

Each **element** is **push**ed as a discrete item into the sequential storage queue. **element**s can only be **push**ed to the FRONT of the list. Each **element** contains its own integrity check.

An **element** may be **pop**ped by invalidating an element, ensuring it is not visible in future read passes. Only the OLDEST elements can be **pop**ped.

A **write record** is a linear, uninterrupted sequence of "elements", that specifically starts with a **start element**, contains `1..N` **data elements**, and ends with an **end element**. A **write record** is only considered if ALL items described above are present, in the correct order (e.g. start element, N data elements, stop element), with valid contents. It (and all **element**s that make it up) is otherwise entirely ignored.

The **data storage backend** of **noodle** is made up of a first-in, first-out (FIFO) queue of **element**s, organized as **write record**s.

## In-Flash Data Representation
This document describes how cfg-noodle data is represented in flash in different situations.

### "Happy path writing", including initial boot

```text
┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │             │             │             │             │             ││
 │             │             │             │             │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The flash starts with all empty sectors. The system will use and write-back default
│ values for all nodes.                                                               │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││1-------1    │             │             │             │             │             ││
 │SabcdefgE    │             │             │             │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The initial write record occurs. A write is started with a START (S) element and seq
│ no. Nodes a-g are written as data elements, then an END (E) element is written.     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││1-------12-------2         │             │             │             │             ││
 │SabcdefgESabcefghE         │             │             │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 At a later point, a second write record occurs. We have two valid write records.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││1-------12------23--------34--------4    │             │             │             ││
 │xxxxxxxxxSabcefgESabcdefghESabcdefghE    │             │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 More writes occur. We reach a point where we have >3 full valid write records. We
│ begin popping elements older than 3 by corrupting the CRCs of these elements,       │
  but NOT doing an erase until all elements of a sector have been popped.
└ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││1-------12------23--------34--------45-------5         │             │             ││
 │xxxxxxxxxxxxxxxxxSabcdefghESabcdefghESabcdefgE         │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 More writes and element pops occur. We reach a point where the last element of a
│ 4k sector has been popped. We now schedule an erasure of the empty sector after     │
  the write has been completed.
└ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------5         │             │             ││
 │             │xxxSabcdefghESabcdefghESabcdefgE         │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The erase occurs, we are left with a few popped elements, and three valid write
│ records.                                                                            │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
```

### Subsequent boots and reloading stored data

```text
┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------5         │             │             ││
 │             │xxxSabcdefghESabcdefghESabcdefgE         │             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We boot, and begin linearly scanning the range. After one full pass of all valid
│ elements occurs, we determine that "5" is the newest write record with valid data.  │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------5         │             │             ││
 │             │xxxSabcdefghESabcdefghESabcdefgE         │             │             │
││             │             │         ^-------^         │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We iterate a second time, skipping until we reach write 5. We then use the contents
│ of write 5 to deserialize and hydrate the in-memory nodes.                          │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghE             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 Writes resume as normal, continuing on after 5.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
```

### Write Record failure due to bad flash block

```text
┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghE      ~      │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We start with all valid data. Unknown to us, the next sector contains a bad byte that
│ will fail to write.                                                                 │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------67--------7   │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghESabcde~ghE   │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We write and perform a readback to validate. The CRC does not match due to the bad
│ byte in the region. We attempt to start over and do a write again.                  │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------67--------78--------8       ││
 │             │xxxxxxxxxxxxxxxxxxxxxxxSabcdefgESabcdefghESabcde~ghESabcdefghE       │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 This write succeeds. We now have three good writes: 5, 6, and 8. All items earlier
│ than 5 are popped. 7 is left as-is. An erase is scheduled as usual.                 │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │             │--------45-------56--------67--------78--------8       ││
 │             │             │xxxxxxxxxSabcdefgESabcdefghESabcde~ghESabcdefghE       │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The erase completes as expected, usage continues.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
```

### Write Record failure due to interruption

```text
┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghE             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We start with all valid data.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------67----        │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghESabcd        │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 We begin a write, but are interrupted by a power-off event, panic, or some other
│ failure that causes an interruption of the record write after pushing some elements.│
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------67----        │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghESabcd        │             │
││             │             │             │    ^--------^             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 On next boot, we scan the flash, and determine that 6 is the latest, complete + valid
│ write record. "6" is used to hydrate the system.                                    │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------67----        │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghESabcd        │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 After hydration, 7 is left as-is.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │             │--------45-------56--------67----8--------8            ││
 │             │             │xxxxxxxxxSabcdefgESabcdefghESabcdSabcdefghE            │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 Writing continues as normal.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
```

### Recovery from hardware corruption between successful write and next boot

```text
┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdefghE             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The system shuts down with good data.
│                                                                                     │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdef~hE             │             │
││             │             │             │             │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 At some point between the completion and read-back of the write, and the next
│ subsequent boot, the latest record is invalidated due to a hardware failure.        │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─

┌ nor flash ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
 ┌─────────────┬─────────────┬─────────────┬─────────────┬─────────────┬─────────────┐
││             │--23--------34--------45-------56--------6             │             ││
 │             │xxxxxxxxxxxxxSabcdefghESabcdefgESabcdef~hE             │             │
││             │             │         ^-------^         │             │             ││
 └─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┴─4k sector───┘
├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
 The system boots, and determines that write record "5" is the newest valid record.
│ Record 5 is used to hydrate the system data                                         │
 ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
```