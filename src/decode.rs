//! Memory decode and bus mastering for Start, kept independent of UEFI so
//! the firmware's failure modes can be tested. Taken from stormnic-ixgbe
//! (its #19), unchanged.
//!
//! The UEFI way is `PciIo.Attributes(Enable, MEMORY | BUS_MASTER)`. AMI
//! Aptio 4 (the X9 blades) answered UNSUPPORTED somewhere in that path where
//! EDK2 does not, so every attribute call here is advisory:
//!
//! 1. `Get` (the attributes to put back) and `Supported` are read; a failure
//!    is recorded, not fatal.
//! 2. `Enable` the wanted bits (masked by Supported when it is known); if
//!    that fails, each bit alone.
//! 3. The PCI command register is the ground truth: if Memory Space Enable
//!    and Bus Master Enable are not both set after step 2, they are set with a
//!    16-bit config write (never 32-bit: the status register above it has
//!    write-1-to-clear bits) and read back. Only if they are still clear
//!    does Start fail.
//!
//! `release` undoes exactly what `enable` did.

/// `EFI_PCI_IO_ATTRIBUTE_MEMORY`.
pub const ATTRIBUTE_MEMORY: u64 = 0x0002;
/// `EFI_PCI_IO_ATTRIBUTE_BUS_MASTER`.
pub const ATTRIBUTE_BUS_MASTER: u64 = 0x0004;
const WANTED: u64 = ATTRIBUTE_MEMORY | ATTRIBUTE_BUS_MASTER;

/// The PCI command register (config offset 4, 16 bits).
pub const COMMAND: u32 = 0x04;
/// Command register: Memory Space Enable.
pub const COMMAND_MEMORY: u16 = 1 << 1;
/// Command register: Bus Master Enable.
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
const COMMAND_WANTED: u16 = COMMAND_MEMORY | COMMAND_BUS_MASTER;

/// What `enable` needs from the function: the PciIo attribute operations
/// and 16-bit config access.
pub trait Pci {
    type Status: Copy + core::fmt::Debug;
    fn get(&self) -> Result<u64, Self::Status>;
    fn supported(&self) -> Result<u64, Self::Status>;
    fn enable(&self, attributes: u64) -> Result<(), Self::Status>;
    fn disable(&self, attributes: u64) -> Result<(), Self::Status>;
    fn set(&self, attributes: u64) -> Result<(), Self::Status>;
    fn config_read_u16(&self, offset: u32) -> Result<u16, Self::Status>;
    fn config_write_u16(&self, offset: u32, value: u16) -> Result<(), Self::Status>;
}

/// What `enable` found and did, for the console and for `release`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enabled<S> {
    /// `Get` before anything changed.
    pub original: Result<u64, S>,
    pub supported: Result<u64, S>,
    /// The `Enable` calls: the combined one, then each bit alone if it failed.
    pub enable: Result<(), S>,
    /// Attribute bits that `Enable` accepted.
    pub by_attributes: u64,
    /// Command register before the config write, if one was needed.
    pub command_before: Option<u16>,
    /// Command register at the end.
    pub command: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<S> {
    /// The command register could not be read or written.
    Config(S),
    /// MSE and BME still not both set after every attempt (the register's
    /// value); the attempts are in `Enabled`.
    NotSet(u16, Enabled<S>),
}

pub fn enable<P: Pci>(pci: &P) -> Result<Enabled<P::Status>, Error<P::Status>> {
    let original = pci.get();
    let supported = pci.supported();
    let mask = match supported {
        Ok(s) => WANTED & s,
        Err(_) => WANTED,
    };
    let mut by_attributes = 0;
    let mut enable = Ok(());
    if mask != 0 {
        enable = pci.enable(mask);
        if enable.is_ok() {
            by_attributes = mask;
        } else {
            for bit in [ATTRIBUTE_MEMORY, ATTRIBUTE_BUS_MASTER] {
                if mask & bit != 0 && pci.enable(bit).is_ok() {
                    by_attributes |= bit;
                }
            }
        }
    }
    let mut command = pci.config_read_u16(COMMAND).map_err(Error::Config)?;
    let mut command_before = None;
    if command & COMMAND_WANTED != COMMAND_WANTED {
        command_before = Some(command);
        pci.config_write_u16(COMMAND, command | COMMAND_WANTED).map_err(Error::Config)?;
        command = pci.config_read_u16(COMMAND).map_err(Error::Config)?;
    }
    let e = Enabled { original, supported, enable, by_attributes, command_before, command };
    if command & COMMAND_WANTED != COMMAND_WANTED {
        return Err(Error::NotSet(command, e));
    }
    Ok(e)
}

/// Put the function back as `enable` found it, in reverse order: the
/// command register to its state after `Enable`, then the attributes.
/// Returns the first failure; the other step runs anyway.
pub fn release<P: Pci>(pci: &P, e: &Enabled<P::Status>) -> Result<(), P::Status> {
    let mut r = Ok(());
    if let Some(before) = e.command_before {
        r = pci.config_read_u16(COMMAND).and_then(|now| {
            pci.config_write_u16(COMMAND, (now & !COMMAND_WANTED) | (before & COMMAND_WANTED))
        });
    }
    if e.by_attributes != 0 {
        let a = match e.original {
            Ok(original) => pci.set(original),
            Err(_) => pci.disable(e.by_attributes),
        };
        if r.is_ok() {
            r = a;
        }
    }
    r
}

/// Turn bus mastering off (a NIC whose queues would not stop): `Disable`,
/// and BME cleared in the command register whatever that returned.
/// Ok if BME reads back clear.
pub fn stop_bus_master<P: Pci>(pci: &P) -> Result<(), Option<P::Status>> {
    let _ = pci.disable(ATTRIBUTE_BUS_MASTER);
    let command = pci.config_read_u16(COMMAND).map_err(Some)?;
    if command & COMMAND_BUS_MASTER != 0 {
        pci.config_write_u16(COMMAND, command & !COMMAND_BUS_MASTER).map_err(Some)?;
        if pci.config_read_u16(COMMAND).map_err(Some)? & COMMAND_BUS_MASTER != 0 {
            return Err(None);
        }
    }
    Ok(())
}
