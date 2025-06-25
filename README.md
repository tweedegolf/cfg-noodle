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

TODO: Add a code example? Or link the noodle-demo

## Architecture

The library is built around three main components:

- **[`StorageList`]**: The central coordinator that manages all configuration items
- **[`StorageListNode`]**: Individual configuration items that can be attached to a list  
- **[`StorageListNodeHandle`]**: A handle for reading and writing configuration data

A background worker task handles all flash I/O operations including:
- Loading configuration data from flash on startup
- Writing changed configuration data to flash
- Garbage collection to reclaim space from old data

## Documentation

- **[API Documentation](https://docs.rs/cfg-noodle)** - docs.rs API reference
- **[Safety Guide](https://docs.rs/cfg-noodle/latest/cfg_noodle/safety_guide/)** - Important safety considerations for contributors
- **[Examples](examples/)** - Complete working examples
- **[Worker Task](https://docs.rs/cfg-noodle/latest/cfg_noodle/worker_task/)** - Default worker task implementation that can serve as a template to build your own

## Storage Backends

The library supports different storage backends through the [`NdlDataStorage`] trait:

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

## Contributing

We welcome contributions! Please see our [Contributing Guide](CONTRIBUTING.md) for details.