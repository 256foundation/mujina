use std::collections::BTreeMap;
use std::env;
use std::ops::RangeInclusive;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mujina_miner::asic::bzm2::protocol::{
    DtsVsGeneration, ENGINE_REG_TARGET, ENGINE_REG_TIMESTAMP_COUNT, ENGINE_REG_ZEROS_TO_FIND,
    TdmFrame, TdmFrameParser, default_engine_coordinates, encode_read_register,
    encode_read_result_command, logical_engine_address,
};
use mujina_miner::asic::bzm2::{
    BROADCAST_GROUP_ASIC, Bzm2ClockController, Bzm2Dll, Bzm2Pll, Bzm2UartController, NOTCH_REG,
};
use mujina_miner::transport::{SerialReader, SerialStream, SerialWriter};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, timeout};

const DEFAULT_BAUD: u32 = 5_000_000;
const DEFAULT_WATCH_POLL_MS: u64 = 100;
const DEFAULT_BROADCAST_READ_TIMEOUT_MS: u64 = 2_000;
const LOCAL_REG_UART_TDM_CTL: u8 = 0x07;
const MAX_EFFBST_SUBJOBS: u8 = 4;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "uart-read" => cmd_uart_read(&args[2..]).await,
        "uart-write" => cmd_uart_write(&args[2..]).await,
        "uart-multicast-write" => cmd_uart_multicast_write(&args[2..]).await,
        "uart-noop" => cmd_uart_noop(&args[2..]).await,
        "uart-loopback" => cmd_uart_loopback(&args[2..]).await,
        "uart-read-result" => cmd_uart_read_result(&args[2..]).await,
        "noop-scan" => cmd_noop_scan(&args[2..]).await,
        "loopback-scan" => cmd_loopback_scan(&args[2..]).await,
        "tdm-enable" => cmd_tdm_enable(&args[2..]).await,
        "tdm-disable" => cmd_tdm_disable(&args[2..]).await,
        "tdm-watch" => cmd_tdm_watch(&args[2..]).await,
        "tdm-broadcast-read-watch" => cmd_tdm_broadcast_read_watch(&args[2..]).await,
        "engine-target-all" => cmd_engine_target_all(&args[2..]).await,
        "engine-timestamp-all" => cmd_engine_timestamp_all(&args[2..]).await,
        "engine-zeros-all" => cmd_engine_zeros_all(&args[2..]).await,
        "job-grid" => cmd_job_grid(&args[2..]).await,
        "job-grid-watch" => cmd_job_grid_watch(&args[2..]).await,
        "job-grid-2phase-watch" => cmd_job_grid_2phase_watch(&args[2..]).await,
        "clock-report" => cmd_clock_report(&args[2..]).await,
        "pll-set" => cmd_pll_set(&args[2..]).await,
        "dll-set" => cmd_dll_set(&args[2..]).await,
        "pll-broadcast-lock-check" => cmd_pll_broadcast_lock_check(&args[2..]).await,
        "dll-broadcast-lock-check" => cmd_dll_broadcast_lock_check(&args[2..]).await,
        other => bail!("unknown command: {other}"),
    }
}

fn print_usage() {
    eprintln!("Usage: mujina-bzm2-debug <command> [args]");
    eprintln!();
    eprintln!("Direct UART developer commands:");
    eprintln!("  uart-read <serial> <asic> <engine|notch> <offset> <count> [baud]");
    eprintln!("  uart-write <serial> <asic|broadcast> <engine|notch> <offset> <hex-bytes> [baud]");
    eprintln!(
        "  uart-multicast-write <serial> <asic|broadcast> <group> <offset> <hex-bytes> [baud]"
    );
    eprintln!("  uart-noop <serial> <asic> [baud]");
    eprintln!("  uart-loopback <serial> <asic> <hex-bytes> [baud]");
    eprintln!("  uart-read-result <serial> <asic> <dts-gen> [baud]");
    eprintln!("  noop-scan <serial> <asic-start> <asic-end> [baud]");
    eprintln!("  loopback-scan <serial> <asic-start> <asic-end> <payload-len> [baud]");
    eprintln!();
    eprintln!("TDM control and observation:");
    eprintln!("  tdm-enable <serial> <tdm-prediv-raw> <tdm-counter> [baud]");
    eprintln!("  tdm-disable <serial> <tdm-prediv-raw> <tdm-counter> [baud]");
    eprintln!("  tdm-watch <serial> <dts-gen> <watch-secs> [baud]");
    eprintln!(
        "  tdm-broadcast-read-watch <serial> <dts-gen> <asic-start> <asic-end> <engine|notch> <offset> <count> [baud]"
    );
    eprintln!();
    eprintln!("Engine-wide validation helpers:");
    eprintln!("  engine-target-all <serial> <asic|broadcast> <target-u32> [baud]");
    eprintln!("  engine-timestamp-all <serial> <asic|broadcast> <timestamp-count> [baud]");
    eprintln!("  engine-zeros-all <serial> <asic|broadcast> <zeros-to-find> [baud]");
    eprintln!("  job-grid <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> [baud]");
    eprintln!(
        "  job-grid-watch <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> <watch-secs> <dts-gen> [baud]"
    );
    eprintln!(
        "  job-grid-2phase-watch <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> <watch-secs> <dts-gen> [baud]"
    );
    eprintln!();
    eprintln!("Clock diagnostics:");
    eprintln!("  clock-report <serial> <asic> [baud]");
    eprintln!("  pll-set <serial> <asic|broadcast> <pll0|pll1> <freq-mhz> <post1-divider> [baud]");
    eprintln!("  dll-set <serial> <asic|broadcast> <dll0|dll1> <duty-cycle> [baud]");
    eprintln!(
        "  pll-broadcast-lock-check <serial> <pll0|pll1> <freq-mhz> <post1-divider> <asic-start> <asic-end> [baud]"
    );
    eprintln!(
        "  dll-broadcast-lock-check <serial> <dll0|dll1> <duty-cycle> <asic-start> <asic-end> [baud]"
    );
    eprintln!();
    eprintln!("Addressing examples:");
    eprintln!("  unicast   : uart-write /dev/ttyUSB0 2 notch 0x12 01000000");
    eprintln!("  broadcast : uart-write /dev/ttyUSB0 broadcast notch 0x12 01000000");
    eprintln!("  multicast : uart-multicast-write /dev/ttyUSB0 2 7 0x49 3c");
    eprintln!("  scan      : noop-scan /dev/ttyUSB0 0 15");
    eprintln!("  TDM watch : tdm-watch /dev/ttyUSB0 gen2 5");
    eprintln!("  grid job  : job-grid-watch /dev/ttyUSB0 2 0 1700000000 60 5 gen2");
}

fn parse_baud(raw: Option<&String>) -> Result<u32> {
    match raw {
        Some(raw) => Ok(parse_u32(raw)?),
        None => Ok(DEFAULT_BAUD),
    }
}

fn parse_watch_duration(raw: &str) -> Result<Duration> {
    let seconds = parse_f32(raw)?;
    if seconds <= 0.0 {
        bail!("watch duration must be greater than zero");
    }
    Ok(Duration::from_secs_f32(seconds))
}

fn parse_dts_generation(raw: &str) -> Result<DtsVsGeneration> {
    DtsVsGeneration::from_env_value(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid DTS/VS generation: {raw}"))
}

fn parse_u8(raw: &str) -> Result<u8> {
    Ok(parse_u32(raw)? as u8)
}

fn parse_u16(raw: &str) -> Result<u16> {
    Ok(parse_u32(raw)? as u16)
}

fn parse_u32(raw: &str) -> Result<u32> {
    if let Some(stripped) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Ok(u32::from_str_radix(stripped, 16)?)
    } else {
        Ok(raw.parse()?)
    }
}

fn parse_f32(raw: &str) -> Result<f32> {
    Ok(raw.parse()?)
}

fn parse_hex_bytes(raw: &str) -> Result<Vec<u8>> {
    let sanitized: String = raw
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '_' && *c != ':')
        .collect();
    if sanitized.len() % 2 != 0 {
        bail!("hex payload must have an even number of digits");
    }
    Ok(hex::decode(sanitized)?)
}

fn parse_asic_or_broadcast(raw: &str) -> Result<u8> {
    if raw.eq_ignore_ascii_case("broadcast") {
        Ok(BROADCAST_GROUP_ASIC)
    } else {
        parse_u8(raw)
    }
}

fn parse_asic_range(start: &str, end: &str) -> Result<RangeInclusive<u8>> {
    let start = parse_u8(start)?;
    let end = parse_u8(end)?;
    if start > end {
        bail!("asic-start must be less than or equal to asic-end");
    }
    Ok(start..=end)
}

fn parse_engine_address(raw: &str) -> Result<u16> {
    if raw.eq_ignore_ascii_case("notch") {
        Ok(NOTCH_REG)
    } else {
        parse_u16(raw)
    }
}

fn parse_pll(raw: &str) -> Result<Bzm2Pll> {
    match raw.to_ascii_lowercase().as_str() {
        "pll0" | "0" => Ok(Bzm2Pll::Pll0),
        "pll1" | "1" => Ok(Bzm2Pll::Pll1),
        _ => bail!("invalid PLL selector: {raw}"),
    }
}

fn parse_dll(raw: &str) -> Result<Bzm2Dll> {
    match raw.to_ascii_lowercase().as_str() {
        "dll0" | "0" => Ok(Bzm2Dll::Dll0),
        "dll1" | "1" => Ok(Bzm2Dll::Dll1),
        _ => bail!("invalid DLL selector: {raw}"),
    }
}

fn parse_zeros_to_find(raw: &str) -> Result<u8> {
    let zeros = parse_u8(raw)?;
    if !(32..=64).contains(&zeros) {
        bail!("zeros-to-find must be in the inclusive range 32..=64");
    }
    Ok(zeros)
}

fn open_uart(serial: &str, baud: u32) -> Result<Bzm2UartController> {
    let stream = SerialStream::new(serial, baud)
        .with_context(|| format!("failed to open serial port {serial} at {baud} baud"))?;
    let (reader, writer, _) = stream.split();
    Ok(Bzm2UartController::new(reader, writer))
}

fn open_clock(serial: &str, baud: u32) -> Result<Bzm2ClockController> {
    let stream = SerialStream::new(serial, baud)
        .with_context(|| format!("failed to open serial port {serial} at {baud} baud"))?;
    let (reader, writer, _) = stream.split();
    Ok(Bzm2ClockController::new(reader, writer))
}

fn open_raw(serial: &str, baud: u32) -> Result<(SerialReader, SerialWriter)> {
    let stream = SerialStream::new(serial, baud)
        .with_context(|| format!("failed to open serial port {serial} at {baud} baud"))?;
    let (reader, writer, _) = stream.split();
    Ok((reader, writer))
}

async fn cmd_uart_read(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!("usage: uart-read <serial> <asic> <engine|notch> <offset> <count> [baud]");
    }
    let mut uart = open_uart(&args[0], parse_baud(args.get(5))?)?;
    let asic = parse_u8(&args[1])?;
    let engine = parse_engine_address(&args[2])?;
    let offset = parse_u8(&args[3])?;
    let count = parse_u8(&args[4])?;
    let data = uart.read_register(asic, engine, offset, count).await?;
    println!("{}", hex::encode(data));
    Ok(())
}

async fn cmd_uart_write(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!(
            "usage: uart-write <serial> <asic|broadcast> <engine|notch> <offset> <hex-bytes> [baud]"
        );
    }
    let mut uart = open_uart(&args[0], parse_baud(args.get(5))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let engine = parse_engine_address(&args[2])?;
    let offset = parse_u8(&args[3])?;
    let value = parse_hex_bytes(&args[4])?;
    uart.write_register(asic, engine, offset, &value).await?;
    println!("ok");
    Ok(())
}

async fn cmd_uart_multicast_write(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!(
            "usage: uart-multicast-write <serial> <asic|broadcast> <group> <offset> <hex-bytes> [baud]"
        );
    }
    let mut uart = open_uart(&args[0], parse_baud(args.get(5))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let group = parse_u16(&args[2])?;
    let offset = parse_u8(&args[3])?;
    let value = parse_hex_bytes(&args[4])?;
    uart.multicast_write_register(asic, group, offset, &value)
        .await?;
    println!("ok");
    Ok(())
}

async fn cmd_uart_noop(args: &[String]) -> Result<()> {
    if args.len() < 2 || args.len() > 3 {
        bail!("usage: uart-noop <serial> <asic> [baud]");
    }
    let mut uart = open_uart(&args[0], parse_baud(args.get(2))?)?;
    let asic = parse_u8(&args[1])?;
    let value = uart.noop(asic).await?;
    println!(
        "ascii={}{}{} hex={}",
        value[0] as char,
        value[1] as char,
        value[2] as char,
        hex::encode(value)
    );
    Ok(())
}

async fn cmd_uart_loopback(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: uart-loopback <serial> <asic> <hex-bytes> [baud]");
    }
    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let asic = parse_u8(&args[1])?;
    let payload = parse_hex_bytes(&args[2])?;
    let echoed = uart.loopback(asic, &payload).await?;
    println!("{}", hex::encode(echoed));
    Ok(())
}

async fn cmd_uart_read_result(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: uart-read-result <serial> <asic> <dts-gen> [baud]");
    }

    let asic = parse_u8(&args[1])?;
    let generation = parse_dts_generation(&args[2])?;
    let baud = parse_baud(args.get(3))?;
    let (mut reader, mut writer) = open_raw(&args[0], baud)?;
    writer
        .write_all(&encode_read_result_command(asic))
        .await
        .context("failed to send read-result command")?;
    writer
        .flush()
        .await
        .context("failed to flush serial stream")?;

    let mut parser = TdmFrameParser::new(generation);
    let frames = read_tdm_frames_once(
        &mut reader,
        &mut parser,
        Duration::from_millis(DEFAULT_BROADCAST_READ_TIMEOUT_MS),
    )
    .await?;

    if frames.is_empty() {
        bail!("no TDM frame returned for ASIC {asic}");
    }

    for frame in frames {
        print_tdm_frame(&frame);
    }

    Ok(())
}

async fn cmd_noop_scan(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: noop-scan <serial> <asic-start> <asic-end> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let asics = parse_asic_range(&args[1], &args[2])?;
    let mut found = Vec::new();

    for asic in asics {
        match uart.noop(asic).await {
            Ok(value) => {
                let ascii = String::from_utf8_lossy(&value);
                println!("asic {asic}: ascii={ascii} hex={}", hex::encode(value));
                found.push(asic);
            }
            Err(err) => {
                println!("asic {asic}: no response ({err})");
            }
        }
    }

    println!("responsive_asics={}", found.len());
    if !found.is_empty() {
        println!("asic_ids={}", format_asic_ids(&found));
    }

    Ok(())
}

async fn cmd_loopback_scan(args: &[String]) -> Result<()> {
    if args.len() < 4 || args.len() > 5 {
        bail!("usage: loopback-scan <serial> <asic-start> <asic-end> <payload-len> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(4))?)?;
    let asics = parse_asic_range(&args[1], &args[2])?;
    let payload_len = parse_u8(&args[3])? as usize;
    if payload_len == 0 {
        bail!("payload-len must be greater than zero");
    }

    for asic in asics {
        let payload = synthetic_loopback_payload(asic, payload_len);
        let echoed = uart.loopback(asic, &payload).await?;
        let matched = payload == echoed;
        println!(
            "asic {asic}: matched={} payload={} echoed={}",
            matched,
            hex::encode(&payload),
            hex::encode(&echoed)
        );
        if !matched {
            bail!("loopback mismatch on ASIC {asic}");
        }
    }

    Ok(())
}

async fn cmd_tdm_enable(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: tdm-enable <serial> <tdm-prediv-raw> <tdm-counter> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let prediv = parse_u32(&args[1])?;
    let counter = parse_u8(&args[2])?;
    let control = encode_tdm_control(prediv, counter, true);
    uart.write_local_reg_u32(BROADCAST_GROUP_ASIC, LOCAL_REG_UART_TDM_CTL, control)
        .await?;
    println!(
        "broadcast local_reg=0x{LOCAL_REG_UART_TDM_CTL:02x} control={control:#010x} enabled=true"
    );
    Ok(())
}

async fn cmd_tdm_disable(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: tdm-disable <serial> <tdm-prediv-raw> <tdm-counter> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let prediv = parse_u32(&args[1])?;
    let counter = parse_u8(&args[2])?;
    let control = encode_tdm_control(prediv, counter, false);
    uart.write_local_reg_u32(BROADCAST_GROUP_ASIC, LOCAL_REG_UART_TDM_CTL, control)
        .await?;
    println!(
        "broadcast local_reg=0x{LOCAL_REG_UART_TDM_CTL:02x} control={control:#010x} enabled=false"
    );
    Ok(())
}

async fn cmd_tdm_watch(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: tdm-watch <serial> <dts-gen> <watch-secs> [baud]");
    }

    let generation = parse_dts_generation(&args[1])?;
    let duration = parse_watch_duration(&args[2])?;
    let baud = parse_baud(args.get(3))?;
    let (mut reader, _) = open_raw(&args[0], baud)?;
    let mut parser = TdmFrameParser::new(generation);
    let stats = watch_tdm_frames(&mut reader, &mut parser, duration).await?;
    println!(
        "summary: result={} register={} noop={} dts_vs={}",
        stats.result, stats.register, stats.noop, stats.dts_vs
    );
    Ok(())
}

async fn cmd_tdm_broadcast_read_watch(args: &[String]) -> Result<()> {
    if args.len() < 7 || args.len() > 8 {
        bail!(
            "usage: tdm-broadcast-read-watch <serial> <dts-gen> <asic-start> <asic-end> <engine|notch> <offset> <count> [baud]"
        );
    }

    let generation = parse_dts_generation(&args[1])?;
    let asics = parse_asic_range(&args[2], &args[3])?;
    let engine = parse_engine_address(&args[4])?;
    let offset = parse_u8(&args[5])?;
    let count = parse_u8(&args[6])?;
    let baud = parse_baud(args.get(7))?;
    let (mut reader, mut writer) = open_raw(&args[0], baud)?;
    let mut parser = TdmFrameParser::new(generation);
    let expected = asics.clone().count();

    for asic in asics.clone() {
        parser.expect_read_register_bytes(asic, count as usize);
    }

    writer
        .write_all(&encode_read_register(
            BROADCAST_GROUP_ASIC,
            engine,
            offset,
            count,
        ))
        .await
        .context("failed to send broadcast read-register command")?;
    writer
        .flush()
        .await
        .context("failed to flush serial stream")?;

    let mut received = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_millis(DEFAULT_BROADCAST_READ_TIMEOUT_MS);
    while Instant::now() < deadline && received.len() < expected {
        let frames = read_tdm_frames_once(
            &mut reader,
            &mut parser,
            Duration::from_millis(DEFAULT_WATCH_POLL_MS),
        )
        .await?;
        for frame in frames {
            print_tdm_frame(&frame);
            if let TdmFrame::Register(register) = frame {
                received.insert(register.asic, register.data);
            }
        }
    }

    println!(
        "broadcast-read summary: received={} expected={} missing={}",
        received.len(),
        expected,
        expected.saturating_sub(received.len())
    );

    if received.len() != expected {
        let mut missing = Vec::new();
        for asic in asics {
            if !received.contains_key(&asic) {
                missing.push(asic);
            }
        }
        if !missing.is_empty() {
            println!("missing_asics={}", format_asic_ids(&missing));
        }
    }

    Ok(())
}

async fn cmd_engine_target_all(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: engine-target-all <serial> <asic|broadcast> <target-u32> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let target = parse_u32(&args[2])?;
    write_engine_reg_all_u32(&mut uart, asic, ENGINE_REG_TARGET, target).await?;
    println!("programmed engine target across all rows: asic={asic:#04x} target={target:#010x}");
    Ok(())
}

async fn cmd_engine_timestamp_all(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: engine-timestamp-all <serial> <asic|broadcast> <timestamp-count> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let value = parse_u8(&args[2])?;
    write_engine_reg_all_u8(&mut uart, asic, ENGINE_REG_TIMESTAMP_COUNT, value).await?;
    println!(
        "programmed ENGINE_REG_TIMESTAMP_COUNT across all rows: asic={asic:#04x} value={value}"
    );
    Ok(())
}

async fn cmd_engine_zeros_all(args: &[String]) -> Result<()> {
    if args.len() < 3 || args.len() > 4 {
        bail!("usage: engine-zeros-all <serial> <asic|broadcast> <zeros-to-find> [baud]");
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(3))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let zeros = parse_zeros_to_find(&args[2])?;
    let register_value = zeros - 32;
    write_engine_reg_all_u8(&mut uart, asic, ENGINE_REG_ZEROS_TO_FIND, register_value).await?;
    println!(
        "programmed ENGINE_REG_ZEROS_TO_FIND across all rows: asic={asic:#04x} zeros_to_find={zeros} register_value={register_value}"
    );
    Ok(())
}

async fn cmd_job_grid(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!(
            "usage: job-grid <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> [baud]"
        );
    }

    let mut uart = open_uart(&args[0], parse_baud(args.get(5))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let sequence_base = parse_u8(&args[2])?;
    let ntime = parse_u32(&args[3])?;
    let timestamp_count = parse_u8(&args[4])?;

    dispatch_grid_jobs(
        &mut uart,
        asic,
        sequence_base,
        ntime,
        timestamp_count,
        false,
    )
    .await?;
    println!(
        "dispatched grid job set: asic={asic:#04x} engines={} seq_base={} ntime={ntime:#010x} timestamp_count={timestamp_count}",
        default_engine_coordinates().len(),
        sequence_base
    );
    Ok(())
}

async fn cmd_job_grid_watch(args: &[String]) -> Result<()> {
    if args.len() < 7 || args.len() > 8 {
        bail!(
            "usage: job-grid-watch <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> <watch-secs> <dts-gen> [baud]"
        );
    }

    let asic = parse_asic_or_broadcast(&args[1])?;
    let sequence_base = parse_u8(&args[2])?;
    let ntime = parse_u32(&args[3])?;
    let timestamp_count = parse_u8(&args[4])?;
    let duration = parse_watch_duration(&args[5])?;
    let generation = parse_dts_generation(&args[6])?;
    let baud = parse_baud(args.get(7))?;
    let (mut reader, writer) = open_raw(&args[0], baud)?;
    let mut parser = TdmFrameParser::new(generation);
    let mut uart = Bzm2UartController::new(reader.clone(), writer);

    dispatch_grid_jobs(
        &mut uart,
        asic,
        sequence_base,
        ntime,
        timestamp_count,
        false,
    )
    .await?;
    let stats = watch_tdm_frames(&mut reader, &mut parser, duration).await?;
    println!(
        "summary: result={} register={} noop={} dts_vs={}",
        stats.result, stats.register, stats.noop, stats.dts_vs
    );
    Ok(())
}

async fn cmd_job_grid_2phase_watch(args: &[String]) -> Result<()> {
    if args.len() < 7 || args.len() > 8 {
        bail!(
            "usage: job-grid-2phase-watch <serial> <asic|broadcast> <seq-base> <ntime> <timestamp-count> <watch-secs> <dts-gen> [baud]"
        );
    }

    let asic = parse_asic_or_broadcast(&args[1])?;
    let sequence_base = parse_u8(&args[2])?;
    let ntime = parse_u32(&args[3])?;
    let timestamp_count = parse_u8(&args[4])?;
    let duration = parse_watch_duration(&args[5])?;
    let generation = parse_dts_generation(&args[6])?;
    let baud = parse_baud(args.get(7))?;
    let (mut reader, writer) = open_raw(&args[0], baud)?;
    let mut parser = TdmFrameParser::new(generation);
    let mut uart = Bzm2UartController::new(reader.clone(), writer);

    dispatch_grid_jobs(
        &mut uart,
        asic,
        sequence_base,
        ntime,
        timestamp_count,
        false,
    )
    .await?;
    dispatch_grid_jobs(
        &mut uart,
        asic,
        sequence_base.wrapping_add(MAX_EFFBST_SUBJOBS),
        ntime.wrapping_add(1),
        timestamp_count,
        true,
    )
    .await?;

    let stats = watch_tdm_frames(&mut reader, &mut parser, duration).await?;
    println!(
        "summary: result={} register={} noop={} dts_vs={}",
        stats.result, stats.register, stats.noop, stats.dts_vs
    );
    Ok(())
}

async fn cmd_clock_report(args: &[String]) -> Result<()> {
    if args.len() < 2 || args.len() > 3 {
        bail!("usage: clock-report <serial> <asic> [baud]");
    }
    let mut clock = open_clock(&args[0], parse_baud(args.get(2))?)?;
    let asic = parse_u8(&args[1])?;
    let report = clock.debug_report(asic).await?;
    println!("ASIC {}", report.asic);
    println!(
        "  PLL0: enabled={} locked={} enable={:#010x} misc={:#010x}",
        report.pll0.enabled,
        report.pll0.locked,
        report.pll0.enable_register,
        report.pll0.misc_register
    );
    println!(
        "  PLL1: enabled={} locked={} enable={:#010x} misc={:#010x}",
        report.pll1.enabled,
        report.pll1.locked,
        report.pll1.enable_register,
        report.pll1.misc_register
    );
    println!(
        "  DLL0: locked={} freeze_valid={} coarsecon={} fincon={:#04x} fincon_valid={} control2={:#04x} control5={:#04x}",
        report.dll0.locked,
        report.dll0.freeze_valid,
        report.dll0.coarsecon,
        report.dll0.fincon,
        report.dll0.fincon_valid,
        report.dll0.control2,
        report.dll0.control5
    );
    println!(
        "  DLL1: locked={} freeze_valid={} coarsecon={} fincon={:#04x} fincon_valid={} control2={:#04x} control5={:#04x}",
        report.dll1.locked,
        report.dll1.freeze_valid,
        report.dll1.coarsecon,
        report.dll1.fincon,
        report.dll1.fincon_valid,
        report.dll1.control2,
        report.dll1.control5
    );
    Ok(())
}

async fn cmd_pll_set(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!(
            "usage: pll-set <serial> <asic|broadcast> <pll0|pll1> <freq-mhz> <post1-divider> [baud]"
        );
    }
    let mut clock = open_clock(&args[0], parse_baud(args.get(5))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let pll = parse_pll(&args[2])?;
    let freq = parse_f32(&args[3])?;
    let post1 = parse_u8(&args[4])?;

    if asic == BROADCAST_GROUP_ASIC {
        let config = clock.set_pll_frequency(asic, pll, freq, post1).await?;
        clock.enable_pll(asic, pll).await?;
        println!(
            "broadcast {:?}: freq={}MHz fbdiv={} postdiv={:#x}",
            pll, config.frequency_mhz, config.feedback_divider, config.packed_post_divider
        );
    } else {
        let (config, status) = clock
            .configure_and_lock_pll(asic, pll, freq, post1, Duration::from_secs(3))
            .await?;
        println!(
            "asic {} {:?}: freq={}MHz fbdiv={} postdiv={:#x} locked={} enable={:#010x}",
            asic,
            pll,
            config.frequency_mhz,
            config.feedback_divider,
            config.packed_post_divider,
            status.locked,
            status.enable_register
        );
    }
    Ok(())
}

async fn cmd_dll_set(args: &[String]) -> Result<()> {
    if args.len() < 4 || args.len() > 5 {
        bail!("usage: dll-set <serial> <asic|broadcast> <dll0|dll1> <duty-cycle> [baud]");
    }
    let mut clock = open_clock(&args[0], parse_baud(args.get(4))?)?;
    let asic = parse_asic_or_broadcast(&args[1])?;
    let dll = parse_dll(&args[2])?;
    let duty = parse_u8(&args[3])?;

    if asic == BROADCAST_GROUP_ASIC {
        let config = clock.set_dll_duty_cycle(asic, dll, duty).await?;
        clock.enable_dll(asic, dll).await?;
        println!(
            "broadcast {:?}: duty={} nde_dll={:#x} nde_clk={:#x} npi_clk={:#x}",
            dll, config.duty_cycle, config.nde_dll, config.nde_clk, config.npi_clk
        );
    } else {
        let (config, status) = clock
            .configure_and_lock_dll(asic, dll, duty, Duration::from_secs(2))
            .await?;
        println!(
            "asic {} {:?}: duty={} locked={} coarsecon={} fincon={:#04x} fincon_valid={}",
            asic,
            dll,
            config.duty_cycle,
            status.locked,
            status.coarsecon,
            status.fincon,
            status.fincon_valid
        );
    }
    Ok(())
}

async fn cmd_pll_broadcast_lock_check(args: &[String]) -> Result<()> {
    if args.len() < 6 || args.len() > 7 {
        bail!(
            "usage: pll-broadcast-lock-check <serial> <pll0|pll1> <freq-mhz> <post1-divider> <asic-start> <asic-end> [baud]"
        );
    }

    let mut clock = open_clock(&args[0], parse_baud(args.get(6))?)?;
    let pll = parse_pll(&args[1])?;
    let freq = parse_f32(&args[2])?;
    let post1 = parse_u8(&args[3])?;
    let asics = parse_asic_range(&args[4], &args[5])?;

    let config = clock
        .set_pll_frequency(BROADCAST_GROUP_ASIC, pll, freq, post1)
        .await?;
    clock.enable_pll(BROADCAST_GROUP_ASIC, pll).await?;

    println!(
        "broadcast {:?}: freq={}MHz fbdiv={} postdiv={:#x}",
        pll, config.frequency_mhz, config.feedback_divider, config.packed_post_divider
    );

    for asic in asics {
        let status = clock
            .wait_for_pll_lock(
                asic,
                pll,
                Duration::from_secs(3),
                Duration::from_millis(100),
            )
            .await?;
        println!(
            "asic {} {:?}: locked={} enable={:#010x} misc={:#010x}",
            asic, pll, status.locked, status.enable_register, status.misc_register
        );
    }

    Ok(())
}

async fn cmd_dll_broadcast_lock_check(args: &[String]) -> Result<()> {
    if args.len() < 5 || args.len() > 6 {
        bail!(
            "usage: dll-broadcast-lock-check <serial> <dll0|dll1> <duty-cycle> <asic-start> <asic-end> [baud]"
        );
    }

    let mut clock = open_clock(&args[0], parse_baud(args.get(5))?)?;
    let dll = parse_dll(&args[1])?;
    let duty = parse_u8(&args[2])?;
    let asics = parse_asic_range(&args[3], &args[4])?;

    let config = clock
        .set_dll_duty_cycle(BROADCAST_GROUP_ASIC, dll, duty)
        .await?;
    clock.enable_dll(BROADCAST_GROUP_ASIC, dll).await?;

    println!(
        "broadcast {:?}: duty={} nde_dll={:#x} nde_clk={:#x} npi_clk={:#x}",
        dll, config.duty_cycle, config.nde_dll, config.nde_clk, config.npi_clk
    );

    for asic in asics {
        clock
            .wait_for_dll_lock(asic, dll, Duration::from_secs(2), Duration::from_millis(10))
            .await?;
        let status = clock.ensure_dll_fincon_valid(asic, dll).await?;

        println!(
            "asic {} {:?}: locked={} coarsecon={} fincon={:#04x} fincon_valid={}",
            asic, dll, status.locked, status.coarsecon, status.fincon, status.fincon_valid
        );
    }

    Ok(())
}

async fn write_engine_reg_all_u8(
    uart: &mut Bzm2UartController,
    asic: u8,
    offset: u8,
    value: u8,
) -> Result<()> {
    for row in 0..20u16 {
        uart.multicast_write_reg_u8(asic, row, offset, value)
            .await?;
    }
    Ok(())
}

async fn write_engine_reg_all_u32(
    uart: &mut Bzm2UartController,
    asic: u8,
    offset: u8,
    value: u32,
) -> Result<()> {
    for row in 0..20u16 {
        uart.multicast_write_register(asic, row, offset, &value.to_le_bytes())
            .await?;
    }
    Ok(())
}

async fn dispatch_grid_jobs(
    uart: &mut Bzm2UartController,
    asic: u8,
    sequence_base: u8,
    ntime: u32,
    timestamp_count: u8,
    second_phase: bool,
) -> Result<()> {
    write_engine_reg_all_u8(uart, asic, ENGINE_REG_TIMESTAMP_COUNT, timestamp_count).await?;

    for (index, (row, col)) in default_engine_coordinates().into_iter().enumerate() {
        let engine = logical_engine_address(row, col);
        let sequence = sequence_base.wrapping_add(index as u8);
        let (midstate, merkle_root_residue) =
            synthetic_job_material(asic, engine, sequence, second_phase);
        uart.write_job(
            asic,
            engine,
            &midstate,
            merkle_root_residue,
            ntime.wrapping_add(u32::from(second_phase)),
            sequence,
            0,
        )
        .await?;
    }

    Ok(())
}

async fn watch_tdm_frames(
    reader: &mut SerialReader,
    parser: &mut TdmFrameParser,
    duration: Duration,
) -> Result<TdmStats> {
    let deadline = Instant::now() + duration;
    let mut stats = TdmStats::default();

    while Instant::now() < deadline {
        let frames =
            read_tdm_frames_once(reader, parser, Duration::from_millis(DEFAULT_WATCH_POLL_MS))
                .await?;
        for frame in frames {
            stats.record(&frame);
            print_tdm_frame(&frame);
        }
    }

    Ok(stats)
}

async fn read_tdm_frames_once(
    reader: &mut SerialReader,
    parser: &mut TdmFrameParser,
    wait: Duration,
) -> Result<Vec<TdmFrame>> {
    let mut buf = [0u8; 1024];
    match timeout(wait, reader.read(&mut buf)).await {
        Ok(Ok(0)) => bail!("serial stream closed while waiting for TDM data"),
        Ok(Ok(read)) => Ok(parser.push(&buf[..read])),
        Ok(Err(err)) => Err(err).context("failed to read TDM data"),
        Err(_) => Ok(Vec::new()),
    }
}
fn print_tdm_frame(frame: &TdmFrame) {
    match frame {
        TdmFrame::Result(result) => {
            println!(
                "tdm result: asic={} engine={:#05x} row={} col={} status={:#x} nonce={:#010x} seq={} time={}",
                result.asic,
                result.engine_address,
                result.row(),
                result.col(),
                result.status,
                result.nonce,
                result.sequence_id,
                result.reported_time
            );
        }
        TdmFrame::Register(register) => {
            println!(
                "tdm readreg: asic={} data={}",
                register.asic,
                hex::encode(&register.data)
            );
        }
        TdmFrame::Noop(noop) => {
            let ascii = String::from_utf8_lossy(&noop.data);
            println!(
                "tdm noop: asic={} ascii={} hex={}",
                noop.asic,
                ascii,
                hex::encode(noop.data)
            );
        }
        TdmFrame::DtsVs(dts_vs) => match dts_vs {
            mujina_miner::asic::bzm2::protocol::TdmDtsVsFrame::Gen1(frame) => {
                println!(
                    "tdm dts_vs gen1: asic={} voltage={} thermal_tune_code={} voltage_enabled={} thermal_validity={} thermal_enabled={}",
                    frame.asic,
                    frame.voltage,
                    frame.thermal_tune_code,
                    frame.voltage_enabled,
                    frame.thermal_validity,
                    frame.thermal_enabled
                );
            }
            mujina_miner::asic::bzm2::protocol::TdmDtsVsFrame::Gen2(frame) => {
                println!(
                    "tdm dts_vs gen2: asic={} ch0={} ch1={} ch2={} thermal_code={} thermal_trip={} thermal_fault={} voltage_fault={} pll_lock={} dll0_lock={} dll1_lock={}",
                    frame.asic,
                    frame.ch0_voltage,
                    frame.ch1_voltage,
                    frame.ch2_voltage,
                    frame.thermal_tune_code,
                    frame.thermal_trip_status,
                    frame.thermal_fault,
                    frame.voltage_fault,
                    frame.pll_lock,
                    frame.dll0_lock,
                    frame.dll1_lock
                );
            }
        },
    }
}

fn synthetic_job_material(
    asic: u8,
    engine_address: u16,
    sequence_id: u8,
    second_phase: bool,
) -> ([u8; 32], u32) {
    let phase = if second_phase { 1u8 } else { 0u8 };
    let seed = format!("{asic}:{engine_address}:{sequence_id}:{phase}");
    let digest = Sha256::digest(seed.as_bytes());
    let residue_digest = Sha256::digest(digest);

    let mut midstate = [0u8; 32];
    midstate.copy_from_slice(&digest);

    let mut residue_bytes = [0u8; 4];
    residue_bytes.copy_from_slice(&residue_digest[..4]);
    let merkle_root_residue = u32::from_le_bytes(residue_bytes);

    (midstate, merkle_root_residue)
}

fn synthetic_loopback_payload(asic: u8, payload_len: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(payload_len);
    let mut block = Sha256::digest([asic]);
    while payload.len() < payload_len {
        let take = (payload_len - payload.len()).min(block.len());
        payload.extend_from_slice(&block[..take]);
        block = Sha256::digest(block);
    }
    payload
}

fn encode_tdm_control(prediv_raw: u32, counter: u8, enable: bool) -> u32 {
    (prediv_raw << 9) | ((counter as u32) << 1) | u32::from(enable)
}

fn format_asic_ids(asics: &[u8]) -> String {
    asics
        .iter()
        .map(|asic| asic.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug, Default, Clone, Copy)]
struct TdmStats {
    result: usize,
    register: usize,
    noop: usize,
    dts_vs: usize,
}

impl TdmStats {
    fn record(&mut self, frame: &TdmFrame) {
        match frame {
            TdmFrame::Result(_) => self.result += 1,
            TdmFrame::Register(_) => self.register += 1,
            TdmFrame::Noop(_) => self.noop += 1,
            TdmFrame::DtsVs(_) => self.dts_vs += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_to_find_is_range_checked() {
        assert!(parse_zeros_to_find("32").is_ok());
        assert!(parse_zeros_to_find("64").is_ok());
        assert!(parse_zeros_to_find("31").is_err());
        assert!(parse_zeros_to_find("65").is_err());
    }

    #[test]
    fn tdm_control_word_matches_legacy_layout() {
        let control = encode_tdm_control(0x12, 0x0f, true);
        assert_eq!(control, (0x12 << 9) | (0x0f << 1) | 1);
    }

    #[test]
    fn synthetic_jobs_change_per_engine() {
        let first = synthetic_job_material(2, logical_engine_address(0, 0), 0, false);
        let second = synthetic_job_material(2, logical_engine_address(1, 0), 1, false);
        let third = synthetic_job_material(2, logical_engine_address(0, 0), 0, true);

        assert_ne!(first, second);
        assert_ne!(first, third);
    }
}
