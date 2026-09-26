# Bitaxe Gamma Board Support

This document describes mujina-miner's support for the Bitaxe Gamma board.

## Overview

The [Bitaxe Gamma](https://github.com/bitaxeorg/bitaxegamma) is an open-source Bitcoin
mining board featuring a single BM1370 ASIC chip (from Antminer S21 Pro) and
an ESP32-S3 microcontroller. The board connects to mujina-miner via USB and
provides on-board power management and thermal control.

## Firmware Requirements

**Flash the Bitaxe Gamma with the [rhapd-bitaxe-gamma] firmware to
use it with mujina-miner.** This firmware implements the Raw Hardware
Access Protocol (RHAP). It exposes a dual-port USB serial interface
that allows direct control of the board's peripherals and ASIC
communication.

The [rhapd-bitaxe-gamma README][rhapd-flash] gives the steps to
build and flash the firmware.

rhapd-bitaxe-gamma replaces [bitaxe-raw], which is deprecated.
mujina-miner still recognizes and drives a board that runs
bitaxe-raw.

[rhapd-bitaxe-gamma]: https://github.com/256foundation/rhapd-bitaxe-gamma
[rhapd-flash]: https://github.com/256foundation/rhapd-bitaxe-gamma#developing
[bitaxe-raw]: https://github.com/bitaxeorg/bitaxe-raw

## Board Architecture

The board presents two USB CDC ACM serial ports when connected:
- `/dev/ttyACM0` - Control channel for board management (power, thermal, GPIO)
- `/dev/ttyACM1` - Data channel for direct ASIC communication

The control channel uses RHAP to tunnel I2C, GPIO, and ADC
operations over USB, allowing mujina-miner to manage board peripherals without
custom kernel drivers.

## Hardware Components

- **BM1370 ASIC**: Single chip capable of approximately 1 TH/s at stock
  settings (525 MHz, 1.15V)
- **TPS546D24A**: PMBus-compatible power management IC for core voltage control
- **EMC2101**: PWM fan controller with integrated temperature monitoring

Implementation details for these components are in the board and peripheral
modules.

## References

- [Bitaxe Project](https://bitaxe.org)
- [Bitaxe Gamma Hardware](https://github.com/bitaxeorg/bitaxeGamma)
- [rhapd-bitaxe-gamma Firmware](https://github.com/256foundation/rhapd-bitaxe-gamma)
- [BM13xx Chip Reference](../asic/bm13xx/REFERENCE.md)
