use std::env;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mujina_miner::asic::bzm2::{
    BROADCAST_GROUP_ASIC, Bzm2ClockController, Bzm2Dll, Bzm2Pll, Bzm2UartController, NOTCH_REG,
};
use mujina_miner::transport::SerialStream;

const DEFAULT_BAUD: u32 = 5_000_000;

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
        "clock-report" => cmd_clock_report(&args[2..]).await,
        "pll-set" => cmd_pll_set(&args[2..]).await,
        "dll-set" => cmd_dll_set(&args[2..]).await,
        other => {
            bail!("unknown command: {other}");
        }
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
    eprintln!();
    eprintln!("Clock diagnostics:");
    eprintln!("  clock-report <serial> <asic> [baud]");
    eprintln!("  pll-set <serial> <asic|broadcast> <pll0|pll1> <freq-mhz> <post1-divider> [baud]");
    eprintln!("  dll-set <serial> <asic|broadcast> <dll0|dll1> <duty-cycle> [baud]");
    eprintln!();
    eprintln!("Addressing examples:");
    eprintln!("  unicast   : uart-write /dev/ttyUSB0 2 notch 0x12 01000000");
    eprintln!("  broadcast : uart-write /dev/ttyUSB0 broadcast notch 0x12 01000000");
    eprintln!("  multicast : uart-multicast-write /dev/ttyUSB0 2 7 0x49 3c");
}

fn parse_baud(raw: Option<&String>) -> Result<u32> {
    match raw {
        Some(raw) => Ok(parse_u32(raw)?),
        None => Ok(DEFAULT_BAUD),
    }
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
