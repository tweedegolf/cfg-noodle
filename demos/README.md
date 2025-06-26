# Noodle Demo

This is a minimal example application using the `cfg-noodle` crate (`/crates/cfg-noodle`).

For a video of operation, [see here](https://www.youtube.com/watch?v=GUDZICfzQWM).

In this example, we blink each of the four LEDs at a regular rate, starting at 25ms between on/off times.

Pressing the button doubles the time between on/off, or halves the blinking speed. If the time between on/offs exceeds 1500ms, we reset to 25ms. Essentially we cycle between:

* 20hz (25ms on, 25ms off)
* 10hz (50ms on, 50ms off)
* 5hz (100ms on, 100ms off)
* 2.5hz (200ms on, 200ms off)
* 1.25hz (400ms on, 400ms off)
* 0.625hz (800ms on, 800ms off)

Each time the speed is changed via button press, the value is stored back to the node. These changes are then flushed back to the external flash. If you power cycle the device, the LEDs should resume blinking at the same speed they were at power-off. (Unless you power-cycled before the flash-write has completed.)

The I/O worker performs basic debouncing of writes: if you press the button multiple times, a write to flash does NOT occur for every press. Only when a timeout has been exceeded between write events, is the data actually written back to flash.

See [`main.rs`](./nrf52840/src/main.rs) for initialization and setup code, and [`noodle.rs`](./nrf52840/src/noodle.rs) for usage of the `cfg-noodle` library API.
