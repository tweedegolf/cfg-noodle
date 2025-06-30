# cfg-noodle

[![Crates.io](https://img.shields.io/crates/v/cfg-noodle.svg)](https://crates.io/crates/cfg-noodle)
[![Documentation](https://docs.rs/cfg-noodle/badge.svg)](https://docs.rs/cfg-noodle)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/tweedegolf/cfg-noodle#license)

A persistent configuration management library for embedded Rust applications.

## Overview

`cfg-noodle` provides a type-safe, async-friendly way to manage configuration data in embedded systems with persistent flash storage. It uses an intrusive linked list architecture to efficiently manage multiple configuration items while minimizing memory overhead.

### Key Features

- **Persistent Storage**: Read and write configuration to flash memory with wear leveling
- **Async/Await**: Async API to handle flash access with minimal blocking
- **Memory Efficient**: Uses intrusive linked lists to minimize RAM usage
- **Flexible Flash Support**: The `NdlDataStorage` trait can be implemented for most flash chips
- **Worker Task Pattern**: Background task handles all flash I/O operations (use our default or bring your own!)

## Quick Start

In the [`demos`](https://github.com/tweedegolf/cfg-noodle/tree/main/demos) folder you can find an example for the nRF52840-DK that stores the blinking frequencies of the four
board LEDs on flash and loads them after reboot.

## Architecture

The library is built around a [`StorageList`](https://docs.rs/cfg-noodle/latest/cfg_noodle/struct.StorageList.html) consisting of `StorageListNode`s, where

- **`StorageList`** is the central coordinator that manages all configuration items
- **`StorageListNode`** is the individual configuration item that can be attached to a list
- **`StorageListNodeHandle`** is a handle for reading and writing configuration data

and a background worker task that handles all flash I/O operations including:

- **Loading** configuration data from flash on startup
- **Writing** changed configuration data to flash
- **Garbage collection** to reclaim space from old data

## Documentation

- **[API Documentation](https://docs.rs/cfg-noodle)** - docs.rs API reference
- **[Safety Guide](https://docs.rs/cfg-noodle/latest/cfg_noodle/safety_guide/)** - Important safety considerations for contributors
- **[Data Portability Guide](https://docs.rs/cfg-noodle/latest/cfg_noodle/data_portability/)** - Configuration node data evolution guide
- **[Demos](https://github.com/tweedegolf/cfg-noodle/tree/main/demos)** - Complete working demo example
- **[Worker Task](https://docs.rs/cfg-noodle/latest/cfg_noodle/worker_task/)** - Default worker task implementation that can serve as a template to build your own

## Storage Backends

The library supports different storage backends through the `NdlDataStorage` trait:

- **Sequential Storage**: Built-in support for [`sequential-storage`](https://github.com/tweedegolf/sequential-storage) crate
- **Test Storage**: In-memory storage for testing and development

## Safety

This crate contains lots of `unsafe` code for performance and memory efficiency.
All unsafe code follows strict safety rules documented in the [Safety Guide](https://docs.rs/cfg-noodle/latest/cfg_noodle/safety_guide/).
The public API, however, is safe to use.

## Features

- `std` - Enable standard library support (for testing)
- `defmt` - Enable defmt logging support (efficient logging on embedded)
- Default: no features enabled (no_std embedded use)

## Implementation Overview

The `StorageList` has three functions that need to be executed by a worker task:

- `process_reads`: deserialize data from flash into the nodes
- `process_writes`: serialize data from the nodes into flash
- `process_garbage`: delete old data (but keep a few as a backup)

The worker task is separate to allow users to control exactly when read/write/garbage processing takes place,
enabling control over flash durability consumption, power consumption, efficient batching, and other specific requirements.

However, cfg-noodle provides a basic default worker task implementation that is sufficient for many cases.

### Reading

When attaching a `StorageListNode` to a `StorageList`, the following happens:

1. User calls `attach`
2. Node is pushed to the intrusive linked list (if all safety checks pass and the key does not already exist)
3. Worker task is notified about the node waiting to be hydrated
4. Worker task calls `process_reads` to hydrate the node from flash data or initialize it with a default value otherwise
5. User obtains a `StorageListNodeHandle` that allows loading/writing the underlying value

### Writing

When writing a value via the `StorageListNodeHandle`, the following happens:

1. User calls `write`
2. Value in the `StorageListNode` is replaced
3. Worker task is notified about the pending write to flash
4. Worker task calls `process_writes` to write the entire linked list to flash

### Garbage Collection (GC)

After writing to flash (or at other regular intervals) old data has to be deleted from flash. When the worker task calls `process_garbage` this is what happens:

1. Check whether GC is needed (if not, just return)
2. Iterate over the existing elements in flash
3. Invalidate (i.e. mark as "to be deleted") all elements except for the ones belonging to the current and latest three writes of the list.

### Design Notes

General design notes, vocabulary and data representation in flash can be found in in [DESIGN.md](https://github.com/tweedegolf/cfg-noodle/blob/main/DESIGN.md)

## Contributing

We welcome contributions! Please see our [Contributing Guide](https://github.com/tweedegolf/cfg-noodle/tree/main/CONTRIBUTING.md) for details.
