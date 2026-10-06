use std::time::Instant;

use super::super::Bzm2ThreadConfig;

/// Default cap on a wire recording. Generous enough for a full power-on at a
/// high telemetry rate, small enough that it cannot threaten a gigabyte of RAM
/// even on tmpfs.
pub(super) const DEFAULT_WIRE_RECORD_LIMIT: usize = 64 * 1024 * 1024;

/// Records the bytes the driver read from the chain, for offline replay.
///
/// Driver-side rather than a tap on the line, deliberately. A tap sees the
/// wire; this sees the wire **as the driver read it**, including how the reads
/// were chunked — which is exactly where framing defects live. Our own
/// mis-framing bug would have been reproducible from a recording like this and
/// invisible in an idealised one.
///
/// Bounded and off by default. This control board has a gigabyte, no swap, and
/// flash for both writable filesystems, and we have already filled its memory
/// once with a capture that watched its own size while something else filled
/// the same filesystem underneath it. A recorder must never become the thing
/// that breaks the run it is observing.
///
/// Format is a header line of `key=value` pairs, then one line per read:
/// microseconds since the first read, the byte count, and the bytes in hex.
/// The header carries enough to reconstruct the configuration without asking
/// anybody, because a recording that cannot be cited will be deleted rather
/// than trusted.
pub(super) struct WireRecorder {
    file: std::fs::File,
    started: Instant,
    written: usize,
    limit: usize,
    stopped: bool,
}

/// Where one chain's recording goes: the configured path with the port's name
/// before the extension, so `wire.log` on `/dev/tty9bit10` is
/// `wire-tty9bit10.log`.
///
/// Every chain thread reads the same `MUJINA_BZM2_RECORD`, and each opens its
/// recording with `File::create`. On one path, the last thread to start
/// truncated every earlier one's file and then shared the descriptor's offset
/// with nothing: one chain's recording survived, headed by whichever port
/// opened last. One file per port, named by the port, and always -- a
/// one-chain run gets the same shape as a three-chain one.
pub(super) fn per_port_record_path(configured: &str, serial_path: &str) -> String {
    let port = std::path::Path::new(serial_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("port");
    let p = std::path::Path::new(configured);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("wire");
    let name = match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{port}.{ext}"),
        None => format!("{stem}-{port}"),
    };
    p.with_file_name(name).to_string_lossy().into_owned()
}

impl WireRecorder {
    pub(super) fn open(
        path: &str,
        config: &Bzm2ThreadConfig,
        limit: usize,
    ) -> std::io::Result<Self> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        writeln!(
            file,
            "# bzm2-wire/v1 port={} baud={} generation={:?} asics={} \
             timestamp_count={} nonce_gap={:#x} limit_bytes={} recorded_at={}",
            config.serial_path,
            config.baud_rate,
            config.dts_vs_generation,
            config.asic_ids.len(),
            config.timestamp_count,
            config.nonce_gap,
            limit,
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".into()),
        )?;
        Ok(Self {
            file,
            started: Instant::now(),
            written: 0,
            limit,
            stopped: false,
        })
    }

    pub(super) fn record(&mut self, bytes: &[u8]) {
        use std::io::Write;
        if self.stopped {
            return;
        }
        if self.written + bytes.len() > self.limit {
            let _ = writeln!(self.file, "# stopped: reached {} byte limit", self.limit);
            self.stopped = true;
            return;
        }
        let us = self.started.elapsed().as_micros();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if writeln!(self.file, "{us} {} {hex}", bytes.len()).is_err() {
            self.stopped = true;
            return;
        }
        self.written += bytes.len();
    }
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    #[test]
    fn every_chain_keeps_its_own_recording() {
        // Two chains, one configured path, opened in the order two actors
        // would open them. Each file must be whole and name its own port.
        let dir = std::env::temp_dir().join(format!("bzm2-rec-ports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let configured = dir.join("wire.log");
        let configured = configured.to_str().unwrap();
        let mut recorders = Vec::new();
        for (port, byte) in [("/dev/tty9bit00", 0xa0u8), ("/dev/tty9bit10", 0xb1)] {
            let mut config = Bzm2ThreadConfig::new(port.into(), 5_000_000, 55.0);
            config.asic_ids = (0..100).collect();
            let path = per_port_record_path(configured, port);
            let mut rec = WireRecorder::open(&path, &config, 1 << 20).unwrap();
            rec.record(&[byte; 3]);
            recorders.push((path, rec));
        }
        // The first chain keeps recording after the second has opened.
        recorders[0].1.record(&[0xa0; 2]);
        let paths: Vec<String> = recorders.into_iter().map(|(p, _)| p).collect();
        assert_ne!(paths[0], paths[1], "two chains must not share a recording");
        for (path, port, hex) in [
            (&paths[0], "tty9bit00", "a0a0a0"),
            (&paths[1], "tty9bit10", "b1b1b1"),
        ] {
            let text = std::fs::read_to_string(path).unwrap();
            let header = text.lines().next().unwrap();
            assert!(
                header.contains(&format!("port=/dev/{port}")),
                "{path}: {header}"
            );
            assert!(
                text.lines().skip(1).any(|l| l.ends_with(hex)),
                "{path} lost its own chain's bytes:\n{text}"
            );
            assert!(path.ends_with(&format!("wire-{port}.log")), "{path}");
        }
        let first = std::fs::read_to_string(&paths[0]).unwrap();
        assert_eq!(
            first.lines().count(),
            3,
            "header and both of chain 0's reads:\n{first}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_recording_is_replayable_and_bounded() {
        // Two properties, and the second is the one that protects the unit.
        let dir = std::env::temp_dir().join(format!("bzm2-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wire.log");
        let mut config = Bzm2ThreadConfig::new("/dev/tty9bit00".into(), 5_000_000, 55.0);
        config.asic_ids = (0..4).collect();

        let mut rec = WireRecorder::open(path.to_str().unwrap(), &config, 16).unwrap();
        rec.record(&[0xde, 0xad]);
        rec.record(&[0xbe, 0xef, 0x00]);
        // Past the limit: must stop cleanly rather than write a half line.
        rec.record(&vec![0x11; 64]);
        rec.record(&[0x22]);
        drop(rec);

        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();

        // The header must reconstruct the configuration without asking anyone.
        let header = lines.next().unwrap();
        for needed in [
            "bzm2-wire/v1",
            "port=/dev/tty9bit00",
            "baud=5000000",
            "asics=4",
        ] {
            assert!(header.contains(needed), "header lacks {needed}: {header}");
        }

        // Every data line is parseable: micros, count, hex of that length.
        let mut recorded = 0usize;
        for l in lines {
            if l.starts_with('#') {
                assert!(
                    l.contains("limit"),
                    "the only note allowed is why it stopped: {l}"
                );
                continue;
            }
            let mut f = l.split_whitespace();
            let _us: u128 = f.next().unwrap().parse().unwrap();
            let n: usize = f.next().unwrap().parse().unwrap();
            let hex = f.next().unwrap();
            assert_eq!(hex.len(), n * 2, "count and payload must agree: {l}");
            recorded += n;
        }
        assert_eq!(recorded, 5, "only the reads inside the limit are kept");
        assert!(
            text.contains("stopped"),
            "hitting the limit must be recorded, not silent -- a truncated capture that \
                 does not say it was truncated is worse than no capture"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
