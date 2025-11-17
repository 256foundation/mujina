# Bitaxe Gamma Board Support

This document describes mujina-miner's support for the Bitaxe Gamma board.

## Overview

The [Bitaxe Gamma](https://github.com/bitaxeorg/bitaxegamma) is an open-source Bitcoin
mining board featuring a single BM1370 ASIC chip (from Antminer S21 Pro) and
an ESP32-S3 microcontroller. The board connects to mujina-miner via USB and
provides on-board power management and thermal control.

## Firmware Requirements

**The Bitaxe Gamma must be running the
[bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw) firmware to work with
mujina-miner.** This firmware exposes a dual-port USB serial interface that
allows direct control of the board's peripherals and ASIC communication.

See the [bitaxe-raw flashing
instructions](https://github.com/bitaxeorg/bitaxe-raw#flashing) to install
the required firmware on your board.

## Board Architecture

The board presents two USB CDC ACM serial ports when connected:
- `/dev/ttyACM0` - Control channel for board management (power, thermal, GPIO)
- `/dev/ttyACM1` - Data channel for direct ASIC communication

The control channel uses the bitaxe-raw protocol to tunnel I2C, GPIO, and ADC
operations over USB, allowing mujina-miner to manage board peripherals without
custom kernel drivers.

## Hardware Components

- **BM1370 ASIC**: Single chip capable of approximately 640 GH/s
- **TPS546D24A**: PMBus-compatible power management IC for core voltage control
- **EMC2101**: PWM fan controller with integrated temperature monitoring

Implementation details for these components are in the board and peripheral
modules.

## References

- [Bitaxe Project](https://bitaxe.org)
- [Bitaxe Gamma Hardware](https://github.com/bitaxeorg/bitaxeGamma)
- [bitaxe-raw Firmware](https://github.com/bitaxeorg/bitaxe-raw)
- [BM1370 Protocol Documentation](../asic/bm13xx/PROTOCOL.md)
