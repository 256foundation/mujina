use std::io;

use futures::{SinkExt, sink::Sink, stream::Stream};
use tokio::time::{self, Duration, Instant};
use tokio_stream::StreamExt;

use crate::{
    asic::hash_thread::{BoardPeripherals, HashThreadError},
    tracing::prelude::*,
};

use super::{
    BZM2_END_NONCE, BZM2_START_NONCE, DRIVE_STRENGTH_STRONG, ENGINE_COLS,
    ENGINE_CONFIG_ENHANCED_MODE_BIT, ENGINE_ROWS, INIT_NOOP_TIMEOUT, INIT_READREG_TIMEOUT,
    PLL_LOCK_MASK, PLL_LOCK_TIMEOUT, PLL_POLL_DELAY, POST1_DIVIDER, POST2_DIVIDER, REF_CLK_MHZ,
    REF_DIVIDER, SENSOR_REPORT_INTERVAL, SOFT_RESET_DELAY, TARGET_FREQ_MHZ, THERMAL_TRIP_C,
    VOLTAGE_TRIP_MV, protocol, work::engine_id, work::is_invalid_engine,
};

fn init_failed(msg: impl Into<String>) -> HashThreadError {
    HashThreadError::InitializationFailed(msg.into())
}

async fn send_command<W>(
    chip_commands: &mut W,
    command: protocol::Command,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    chip_commands
        .send(command)
        .await
        .map_err(|e| init_failed(format!("{context}: {e:?}")))
}

async fn drain_input<R>(chip_responses: &mut R)
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
{
    while let Ok(Some(_)) = time::timeout(Duration::from_millis(20), chip_responses.next()).await {}
}

async fn wait_for_noop<R>(
    chip_responses: &mut R,
    expected_asic_id: u8,
    timeout: Duration,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(init_failed(format!(
                "timeout waiting for NOOP response from ASIC 0x{expected_asic_id:02x}"
            )));
        }

        match time::timeout(remaining, chip_responses.next()).await {
            Ok(Some(Ok(protocol::Response::Noop { asic_hw_id, .. })))
                if asic_hw_id == expected_asic_id =>
            {
                return Ok(());
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(init_failed(format!("failed while waiting for NOOP: {e}")));
            }
            Ok(None) => {
                return Err(init_failed("response stream closed while waiting for NOOP"));
            }
            Err(_) => {
                return Err(init_failed(format!(
                    "timeout waiting for NOOP response from ASIC 0x{expected_asic_id:02x}"
                )));
            }
        }
    }
}

async fn read_reg_u32<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    timeout: Duration,
    context: &str,
) -> Result<u32, HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::read_reg_u32(asic_id, engine, offset),
        context,
    )
    .await?;

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(init_failed(format!(
                "{context}: timeout waiting for READREG response"
            )));
        }

        match time::timeout(remaining, chip_responses.next()).await {
            Ok(Some(Ok(protocol::Response::ReadReg { asic_hw_id, data })))
                if asic_hw_id == asic_id =>
            {
                return match data {
                    protocol::ReadRegData::U32(value) => Ok(value),
                    protocol::ReadRegData::U16(value) => Ok(value as u32),
                    protocol::ReadRegData::U8(value) => Ok(value as u32),
                };
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(init_failed(format!("{context}: stream read error: {e}")));
            }
            Ok(None) => {
                return Err(init_failed(format!("{context}: response stream closed")));
            }
            Err(_) => {
                return Err(init_failed(format!(
                    "{context}: timeout waiting for response"
                )));
            }
        }
    }
}

pub(super) async fn write_reg_u32<W>(
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    value: u32,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::write_reg_u32_le(asic_id, engine, offset, value),
        context,
    )
    .await
}

pub(super) async fn write_reg_u8<W>(
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    value: u8,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::write_reg_u8(asic_id, engine, offset, value),
        context,
    )
    .await
}

async fn group_write_u8<W>(
    chip_commands: &mut W,
    asic_id: u8,
    group: u16,
    offset: u16,
    value: u8,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::multicast_write_u8(asic_id, group, offset, value),
        context,
    )
    .await
}

fn thermal_c_to_tune_code(thermal_c: f32) -> u32 {
    let tune_code = (2048.0 / 4096.0) + (4096.0 * (thermal_c + 293.8) / 631.8);
    tune_code.max(0.0) as u32
}

fn voltage_mv_to_tune_code(voltage_mv: f32) -> u32 {
    let tune_code = (16384.0 / 6.0) * (2.5 * voltage_mv / 706.7 + 3.0 / 16384.0 + 1.0);
    tune_code.max(0.0) as u32
}

fn calc_pll_dividers(freq_mhz: f32, post1_divider: u8) -> (u32, u32) {
    let fb =
        REF_DIVIDER as f32 * (post1_divider as f32 + 1.0) * (POST2_DIVIDER as f32 + 1.0) * freq_mhz
            / REF_CLK_MHZ;
    let mut fb_div = fb as u32;
    if fb - fb_div as f32 > 0.5 {
        fb_div += 1;
    }

    let post_div = (1 << 12) | (POST2_DIVIDER << 9) | ((post1_divider as u32) << 6) | REF_DIVIDER;
    (post_div, fb_div)
}

async fn configure_sensors<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    read_asic_id: u8,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let thermal_trip_code = thermal_c_to_tune_code(THERMAL_TRIP_C);
    let voltage_trip_code = voltage_mv_to_tune_code(VOLTAGE_TRIP_MV);

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::UART_TX,
        0xF,
        "enable sensors: UART_TX",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SLOW_CLK_DIV,
        2,
        "enable sensors: SLOW_CLK_DIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENSOR_CLK_DIV,
        (8 << 5) | 8,
        "enable sensors: SENSOR_CLK_DIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::DTS_SRST_PD,
        1 << 8,
        "enable sensors: DTS_SRST_PD",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENS_TDM_GAP_CNT,
        SENSOR_REPORT_INTERVAL,
        "enable sensors: SENS_TDM_GAP_CNT",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::DTS_CFG,
        0,
        "enable sensors: DTS_CFG",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENSOR_THRS_CNT,
        (10 << 16) | 10,
        "enable sensors: SENSOR_THRS_CNT",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::TEMPSENSOR_TUNE_CODE,
        0x8001 | (thermal_trip_code << 1),
        "enable sensors: TEMPSENSOR_TUNE_CODE",
    )
    .await?;

    let bandgap = read_reg_u32(
        chip_responses,
        chip_commands,
        read_asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::BANDGAP,
        INIT_READREG_TIMEOUT,
        "enable sensors: read BANDGAP",
    )
    .await?;
    let bandgap_updated = (bandgap & !0xF) | 0x3;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::BANDGAP,
        bandgap_updated,
        "enable sensors: write BANDGAP",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VSENSOR_SRST_PD,
        1 << 8,
        "enable sensors: VSENSOR_SRST_PD",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VSENSOR_CFG,
        (8 << 28) | (1 << 24),
        "enable sensors: VSENSOR_CFG",
    )
    .await?;

    let vs_enable = (voltage_trip_code << 16) | (voltage_trip_code << 1) | 1;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VOLTAGE_SENSOR_ENABLE,
        vs_enable,
        "enable sensors: VOLTAGE_SENSOR_ENABLE",
    )
    .await?;

    Ok(())
}

async fn set_frequency<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    read_asic_id: u8,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let (post_div, fb_div) = calc_pll_dividers(TARGET_FREQ_MHZ, POST1_DIVIDER);

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_FBDIV,
        fb_div,
        "set frequency: PLL_FBDIV",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_POSTDIV,
        post_div,
        "set frequency: PLL_POSTDIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_FBDIV,
        fb_div,
        "set frequency: PLL1_FBDIV",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_POSTDIV,
        post_div,
        "set frequency: PLL1_POSTDIV",
    )
    .await?;

    time::sleep(Duration::from_millis(1)).await;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_ENABLE,
        1,
        "set frequency: PLL_ENABLE",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_ENABLE,
        1,
        "set frequency: PLL1_ENABLE",
    )
    .await?;

    let deadline = Instant::now() + PLL_LOCK_TIMEOUT;
    for pll_enable_offset in [
        protocol::local_reg::PLL_ENABLE,
        protocol::local_reg::PLL1_ENABLE,
    ] {
        loop {
            let lock = read_reg_u32(
                chip_responses,
                chip_commands,
                read_asic_id,
                protocol::NOTCH_REG,
                pll_enable_offset,
                INIT_READREG_TIMEOUT,
                "set frequency: wait PLL lock",
            )
            .await?;
            if (lock & PLL_LOCK_MASK) != 0 {
                break;
            }

            if Instant::now() >= deadline {
                return Err(init_failed(format!(
                    "set frequency: PLL at offset 0x{pll_enable_offset:02x} failed to lock"
                )));
            }

            time::sleep(PLL_POLL_DELAY).await;
        }
    }

    Ok(())
}

async fn soft_reset<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    write_reg_u32(
        chip_commands,
        asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::ENG_SOFT_RESET,
        0,
        "soft reset assert",
    )
    .await?;
    time::sleep(SOFT_RESET_DELAY).await;
    write_reg_u32(
        chip_commands,
        asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::ENG_SOFT_RESET,
        1,
        "soft reset release",
    )
    .await?;
    time::sleep(SOFT_RESET_DELAY).await;
    Ok(())
}

async fn set_all_clock_gates<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for group_id in 0..ENGINE_ROWS {
        group_write_u8(
            chip_commands,
            asic_id,
            group_id,
            protocol::engine_reg::CONFIG,
            ENGINE_CONFIG_ENHANCED_MODE_BIT,
            "set all clock gates",
        )
        .await?;
    }
    Ok(())
}

async fn set_asic_nonce_range<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            if is_invalid_engine(row, col) {
                continue;
            }
            let engine = engine_id(row, col);
            write_reg_u32(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::START_NONCE,
                BZM2_START_NONCE,
                "set nonce range: START_NONCE",
            )
            .await?;
            write_reg_u32(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::END_NONCE,
                BZM2_END_NONCE,
                "set nonce range: END_NONCE",
            )
            .await?;
        }
    }

    Ok(())
}

async fn start_warm_up_jobs<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            if is_invalid_engine(row, col) {
                continue;
            }
            let engine = engine_id(row, col);

            write_reg_u8(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::TIMESTAMP_COUNT,
                0xff,
                "warm-up: TIMESTAMP_COUNT",
            )
            .await?;

            for seq in [0xfc, 0xfd, 0xfe, 0xff] {
                write_reg_u8(
                    chip_commands,
                    asic_id,
                    engine,
                    protocol::engine_reg::SEQUENCE_ID,
                    seq,
                    "warm-up: SEQUENCE_ID",
                )
                .await?;
            }

            write_reg_u8(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::JOB_CONTROL,
                1,
                "warm-up: JOB_CONTROL",
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn initialize_chip<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
    asic_count: u8,
) -> Result<Vec<u8>, HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if asic_count == 0 {
        return Err(init_failed("asic_count must be > 0"));
    }

    if let Some(ref mut asic_enable) = peripherals.asic_enable {
        asic_enable
            .enable()
            .await
            .map_err(|e| init_failed(format!("failed to release reset for BZM2 bring-up: {e}")))?;
    }
    time::sleep(Duration::from_millis(200)).await;

    drain_input(chip_responses).await;

    send_command(
        chip_commands,
        protocol::Command::Noop {
            asic_hw_id: protocol::DEFAULT_ASIC_ID,
        },
        "default ping",
    )
    .await?;
    wait_for_noop(chip_responses, protocol::DEFAULT_ASIC_ID, INIT_NOOP_TIMEOUT).await?;
    debug!("BZM2 default ASIC ID ping succeeded");

    let mut asic_ids = Vec::with_capacity(asic_count as usize);
    for index in 0..asic_count {
        let asic_id = protocol::logical_to_hw_asic_id(index);
        if protocol::hw_to_logical_asic_id(asic_id) != Some(index) {
            return Err(init_failed(format!(
                "invalid ASIC ID mapping for logical index {} -> 0x{:02x}",
                index, asic_id
            )));
        }

        write_reg_u32(
            chip_commands,
            protocol::DEFAULT_ASIC_ID,
            protocol::NOTCH_REG,
            protocol::local_reg::ASIC_ID,
            asic_id as u32,
            "program chain IDs",
        )
        .await?;
        time::sleep(Duration::from_millis(50)).await;

        let readback = read_reg_u32(
            chip_responses,
            chip_commands,
            asic_id,
            protocol::NOTCH_REG,
            protocol::local_reg::ASIC_ID,
            INIT_READREG_TIMEOUT,
            "verify programmed ASIC ID",
        )
        .await?;

        if (readback & 0xff) as u8 != asic_id {
            return Err(init_failed(format!(
                "ASIC ID verify mismatch for 0x{asic_id:02x}: read 0x{readback:08x}"
            )));
        }

        asic_ids.push(asic_id);
    }
    debug!(asic_ids = ?asic_ids, "BZM2 chain IDs programmed");

    drain_input(chip_responses).await;
    for &asic_id in &asic_ids {
        send_command(
            chip_commands,
            protocol::Command::Noop {
                asic_hw_id: asic_id,
            },
            "per-ASIC ping",
        )
        .await?;
        wait_for_noop(chip_responses, asic_id, INIT_NOOP_TIMEOUT).await?;
    }
    debug!("BZM2 per-ASIC ping succeeded");

    let first_asic = *asic_ids
        .first()
        .ok_or_else(|| init_failed("no ASIC IDs programmed"))?;

    debug!("Configuring BZM2 sensors");
    configure_sensors(chip_responses, chip_commands, first_asic).await?;
    debug!("Configuring BZM2 PLL");
    set_frequency(chip_responses, chip_commands, first_asic).await?;

    write_reg_u8(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::CKDCCR_5_0,
        0x00,
        "disable DLL0",
    )
    .await?;
    write_reg_u8(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::CKDCCR_5_1,
        0x00,
        "disable DLL1",
    )
    .await?;

    let uart_tdm_control = (0x7f << 9) | (100 << 1) | 1;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::UART_TDM_CTL,
        uart_tdm_control,
        "enable UART TDM mode",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        first_asic,
        protocol::NOTCH_REG,
        protocol::local_reg::IO_PEPS_DS,
        DRIVE_STRENGTH_STRONG,
        "set drive strength",
    )
    .await?;

    for &asic_id in &asic_ids {
        debug!(asic_id, "BZM2 soft reset + clock gate + warm-up start");
        soft_reset(chip_commands, asic_id).await?;
        set_all_clock_gates(chip_commands, asic_id).await?;
        set_asic_nonce_range(chip_commands, asic_id).await?;
        start_warm_up_jobs(chip_commands, asic_id).await?;
        debug!(asic_id, "BZM2 warm-up complete");
    }

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::RESULT_STS_CTL,
        0x10,
        "enable TDM results",
    )
    .await?;

    Ok(asic_ids)
}
