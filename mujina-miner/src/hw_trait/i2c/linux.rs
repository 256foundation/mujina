//! Linux `/dev/i2c-N` backend for the [`I2c`] trait.
//!
//! Every transfer goes through the `I2C_RDWR` ioctl, which hands the adapter a
//! list of messages and gets back one bus transaction. That is what makes
//! [`LinuxI2c::write_read`] a genuine repeated start rather than a write
//! followed, some microseconds later, by an unrelated read: both messages are
//! submitted together and the adapter does not release the bus between them.
//! A device that latches a selector on the write and answers on the read
//! cannot be addressed correctly any other way, because anything else sharing
//! the bus may interleave in the gap.
//!
//! The adapter node is a constructor argument, so a caller can point this at
//! any bus and a test can point it at something that is not an adapter at all.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nix::errno::Errno;
use rustix::fs::{Mode, OFlags, open as open_fd};

use super::{I2c, I2cError};
use crate::hw_trait::{HwError, Result};

/// `I2C_RDWR`: submit a message list as one bus transaction.
const IOCTL_I2C_RDWR: u16 = 0x0707;

/// `I2C_FUNCS`: ask an adapter what it is capable of.
const IOCTL_I2C_FUNCS: u16 = 0x0705;

/// `I2C_FUNC_I2C` in the `I2C_FUNCS` reply: the adapter can issue plain I2C
/// messages, which is the capability a repeated start needs. An adapter that
/// only emulates SMBus clears this bit and cannot do a combined transaction at
/// all, so checking it up front turns a class of silent data corruption into
/// an open-time failure.
const FUNC_PLAIN_I2C: nix::libc::c_ulong = 0x0000_0001;

/// `I2C_M_RD` in a message's flags: the kernel fills this message instead of
/// sending it.
const MSG_FLAG_READ: u16 = 0x0001;

/// One past the largest 7-bit address. Ten-bit addressing needs a second flag
/// bit and a different device model; this backend does not offer it.
const ADDRESS_LIMIT: u8 = 0x80;

/// Payload ceiling the kernel enforces on a single message.
const MAX_MSG_LEN: usize = 8192;

/// An I2C bus reached through a Linux `/dev/i2c-N` adapter node.
///
/// Cloning shares the open descriptor. Concurrent transfers are serialised by
/// the adapter's own lock in the kernel, so clones may be handed to different
/// device drivers on the same bus.
#[derive(Clone, Debug)]
pub struct LinuxI2c {
    path: PathBuf,
    fd: Arc<OwnedFd>,
}

impl LinuxI2c {
    /// Open an adapter node.
    ///
    /// Fails if the path cannot be opened, if it is not an I2C adapter, or if
    /// the adapter cannot issue plain I2C messages.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let fd = open_fd(&path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()).map_err(|err| {
            HwError::Other(format!("cannot open I2C adapter {}: {err}", path.display()))
        })?;

        let mut funcs: nix::libc::c_ulong = 0;
        // SAFETY: `funcs` is a live, correctly typed out-parameter for the
        // duration of the call, and `fd` is open.
        unsafe { ioctl::funcs(fd.as_raw_fd(), &mut funcs) }.map_err(|errno| {
            HwError::Other(format!(
                "{} does not answer as an I2C adapter: {errno}",
                path.display()
            ))
        })?;

        if funcs & FUNC_PLAIN_I2C == 0 {
            return Err(HwError::NotSupported(format!(
                "{} cannot issue plain I2C messages, so it cannot do a repeated start",
                path.display()
            )));
        }

        Ok(Self {
            path,
            fd: Arc::new(fd),
        })
    }

    /// The adapter node this bus was opened on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run one transaction and return the bytes read.
    ///
    /// The three trait methods differ only in which halves they ask for, so
    /// they all land here: there is one place that talks to the kernel, and
    /// therefore one place where the message list can be wrong.
    async fn transfer(&self, addr: u8, write: &[u8], read_len: usize) -> Result<Vec<u8>> {
        check_transfer(&self.path, addr, write.len(), read_len)?;

        let fd = Arc::clone(&self.fd);
        let write = write.to_vec();

        // The ioctl blocks for the whole transaction -- at 100 kHz a short
        // register read is a few hundred microseconds, and a multi-byte walk
        // is milliseconds -- so it does not belong on an async worker thread.
        tokio::task::spawn_blocking(move || {
            let mut read = vec![0u8; read_len];
            transfer_blocking(&fd, addr, &write, &mut read).map(|()| read)
        })
        .await
        .map_err(|err| HwError::Other(format!("I2C transfer task did not finish: {err}")))?
        .map_err(|errno| HwError::I2c(map_errno(addr, errno)))
    }
}

#[async_trait]
impl I2c for LinuxI2c {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        self.transfer(addr, data, 0).await?;
        Ok(())
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
        let bytes = self.transfer(addr, &[], buffer.len()).await?;
        buffer.copy_from_slice(&bytes);
        Ok(())
    }

    /// Refuses a half-empty transaction rather than quietly doing something
    /// else.
    ///
    /// With either half empty the message list collapses to one message and
    /// the adapter issues a plain write or a plain read -- no repeated start,
    /// and under this method's name. On a device that decides what to do when
    /// a write *completes*, those are not interchangeable with a combined
    /// transaction, so a caller that wants one of them has [`write`] and
    /// [`read`] and should say so.
    ///
    /// [`write`]: Self::write
    /// [`read`]: Self::read
    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
        if write.is_empty() || read.is_empty() {
            return Err(HwError::InvalidParameter(format!(
                "{}: a write-read needs both halves, got a {}-byte write \
                 and a {}-byte read",
                self.path.display(),
                write.len(),
                read.len()
            )));
        }

        let bytes = self.transfer(addr, write, read.len()).await?;
        read.copy_from_slice(&bytes);
        Ok(())
    }

    /// Always an error, deliberately.
    ///
    /// The i2c-dev interface exposes no clock control: an adapter's bus rate
    /// comes from its own driver configuration and is already fixed by the
    /// time the `/dev` node exists. A no-op would let a caller believe it had
    /// set a rate it had not, and a wrong bus rate does not announce itself --
    /// it shows up later as corrupt data on an unrelated read. Refusing here
    /// puts the failure at the call that was wrong.
    async fn set_frequency(&mut self, hz: u32) -> Result<()> {
        Err(HwError::NotSupported(format!(
            "{} runs at the rate its adapter driver was configured for; \
             /dev/i2c has no clock control (asked for {hz} Hz)",
            self.path.display()
        )))
    }
}

/// One entry of an `I2C_RDWR` message list.
#[repr(C)]
#[derive(Clone, Copy)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

impl I2cMsg {
    const fn empty() -> Self {
        Self {
            addr: 0,
            flags: 0,
            len: 0,
            buf: std::ptr::null_mut(),
        }
    }
}

/// The `I2C_RDWR` argument: a message list and its length.
#[repr(C)]
struct I2cRdwrData {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

mod ioctl {
    use super::{I2cRdwrData, IOCTL_I2C_FUNCS, IOCTL_I2C_RDWR};

    nix::ioctl_readwrite_bad!(rdwr, IOCTL_I2C_RDWR, I2cRdwrData);
    nix::ioctl_read_bad!(funcs, IOCTL_I2C_FUNCS, nix::libc::c_ulong);
}

/// Validate a transfer before it reaches the kernel.
///
/// These are the failures that cost nothing to catch and are miserable to
/// diagnose from the other side of an ioctl.
fn check_transfer(path: &Path, addr: u8, write_len: usize, read_len: usize) -> Result<()> {
    if addr >= ADDRESS_LIMIT {
        return Err(HwError::InvalidParameter(format!(
            "I2C address 0x{addr:02x} is not a 7-bit address"
        )));
    }
    if write_len == 0 && read_len == 0 {
        return Err(HwError::InvalidParameter(format!(
            "{}: a transaction with neither a write nor a read has nothing to do",
            path.display()
        )));
    }
    for len in [write_len, read_len] {
        if len > MAX_MSG_LEN {
            return Err(HwError::InvalidParameter(format!(
                "{len} bytes exceeds the {MAX_MSG_LEN}-byte limit on one I2C message"
            )));
        }
    }
    Ok(())
}

/// Lay out the message list for one transaction.
///
/// A non-empty `write` becomes the first message and a non-empty `read` the
/// second, flagged as a read. Both present is the combined write-then-read the
/// adapter turns into a repeated start; exactly one present is a plain write
/// or a plain read.
fn build_messages(addr: u8, write: &[u8], read: &mut [u8]) -> ([I2cMsg; 2], u32) {
    let mut msgs = [I2cMsg::empty(); 2];
    let mut count = 0usize;

    if !write.is_empty() {
        msgs[count] = I2cMsg {
            addr: u16::from(addr),
            flags: 0,
            len: write.len() as u16,
            // The kernel only reads through this pointer: the message carries
            // no read flag. The cast away from const is the ABI's shape, not a
            // mutable alias.
            buf: write.as_ptr().cast_mut(),
        };
        count += 1;
    }
    if !read.is_empty() {
        msgs[count] = I2cMsg {
            addr: u16::from(addr),
            flags: MSG_FLAG_READ,
            len: read.len() as u16,
            buf: read.as_mut_ptr(),
        };
        count += 1;
    }

    (msgs, count as u32)
}

fn transfer_blocking(
    fd: &OwnedFd,
    addr: u8,
    write: &[u8],
    read: &mut [u8],
) -> std::result::Result<(), Errno> {
    let (mut msgs, nmsgs) = build_messages(addr, write, read);
    let mut data = I2cRdwrData {
        msgs: msgs.as_mut_ptr(),
        nmsgs,
    };

    // SAFETY: `data` points at `msgs`, which outlives the call; each message
    // points at a buffer at least as long as the `len` it declares. The read
    // message is the only one the kernel writes through, and it points into
    // `read`, borrowed mutably for the whole call.
    unsafe { ioctl::rdwr(fd.as_raw_fd(), &mut data) }?;
    Ok(())
}

/// Map the adapter's errno onto the trait's error vocabulary.
fn map_errno(addr: u8, errno: Errno) -> I2cError {
    match errno {
        // The address went out and nothing pulled SDA low for the ack.
        Errno::ENXIO | Errno::EREMOTEIO => I2cError::NoAck(addr),
        // The adapter gave up after losing the bus or finding it busy.
        Errno::EAGAIN | Errno::EBUSY => I2cError::ArbitrationLost,
        Errno::ETIMEDOUT | Errno::EIO => I2cError::BusError,
        other => I2cError::Other(format!("I2C_RDWR failed: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn a_write_and_a_read_become_one_two_message_transaction() {
        let write = [0x17u8, 0x00];
        let mut read = [0u8; 2];

        let (msgs, nmsgs) = build_messages(0x76, &write, &mut read);

        assert_eq!(nmsgs, 2, "a repeated start is two messages, not two ioctls");
        assert_eq!(msgs[0].addr, 0x76);
        assert_eq!(msgs[0].flags, 0);
        assert_eq!(msgs[0].len, 2);
        assert_eq!(msgs[1].addr, 0x76);
        assert_eq!(msgs[1].flags, 1, "the second message carries the read flag");
        assert_eq!(msgs[1].len, 2);
        assert_eq!(msgs[0].buf.cast_const(), write.as_ptr());
        assert_eq!(msgs[1].buf.cast_const(), read.as_ptr());
    }

    #[test]
    fn a_write_only_transaction_has_one_message() {
        let write = [0xaau8];
        let mut read: [u8; 0] = [];

        let (msgs, nmsgs) = build_messages(0x40, &write, &mut read);

        assert_eq!(nmsgs, 1);
        assert_eq!(msgs[0].flags, 0);
        assert_eq!(msgs[0].len, 1);
    }

    #[test]
    fn a_read_only_transaction_has_one_read_message() {
        let mut read = [0u8; 4];

        let (msgs, nmsgs) = build_messages(0x40, &[], &mut read);

        assert_eq!(nmsgs, 1);
        assert_eq!(msgs[0].flags, 1);
        assert_eq!(msgs[0].len, 4);
    }

    #[test]
    fn ten_bit_addresses_are_refused() {
        let err = check_transfer(Path::new("/dev/i2c-2"), 0x80, 2, 2).unwrap_err();
        assert!(matches!(err, HwError::InvalidParameter(_)), "got {err:?}");
    }

    #[test]
    fn an_empty_transaction_is_refused() {
        let err = check_transfer(Path::new("/dev/i2c-2"), 0x76, 0, 0).unwrap_err();
        assert!(matches!(err, HwError::InvalidParameter(_)), "got {err:?}");
    }

    #[test]
    fn an_oversized_message_is_refused() {
        let err = check_transfer(Path::new("/dev/i2c-2"), 0x76, 0, 8193).unwrap_err();
        assert!(matches!(err, HwError::InvalidParameter(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_half_empty_write_read_is_refused_rather_than_downgraded() {
        // No adapter is needed: the guard runs before anything is opened, and
        // a transaction that would have gone out as a plain write is exactly
        // what must never leave under this name.
        let mut bus = LinuxI2c {
            path: PathBuf::from("/dev/i2c-2"),
            fd: Arc::new(std::fs::File::open("/dev/null").unwrap().into()),
        };

        let err = bus
            .write_read(0x76, &[0x11, 0x22, 0x33, 0x44], &mut [])
            .await
            .unwrap_err();

        assert!(matches!(err, HwError::InvalidParameter(_)), "got {err:?}");
    }

    #[test]
    fn opening_a_missing_node_names_the_path() {
        let err = LinuxI2c::open("/dev/i2c-does-not-exist").unwrap_err();
        assert!(
            err.to_string().contains("/dev/i2c-does-not-exist"),
            "error should name the path it tried: {err}"
        );
    }

    #[test]
    fn opening_something_that_is_not_an_adapter_fails_at_open() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("not-an-i2c-adapter-{unique}"));
        fs::write(&path, b"").unwrap();

        let err = LinuxI2c::open(&path).unwrap_err();

        assert!(
            err.to_string().contains("I2C adapter"),
            "a regular file is not an adapter: {err}"
        );
        let _ = fs::remove_file(path);
    }
}
