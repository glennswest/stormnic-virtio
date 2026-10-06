//! `EFI_PCI_IO_PROTOCOL`, from the UEFI specification (section 14.4).
//!
//! The `uefi` crate has the root-bridge protocol but not the per-device one,
//! which is what a PCI driver binds to. The layout here is the spec's, field
//! for field; only the calls the driver uses have safe wrappers.

use core::ffi::c_void;
use uefi::proto::unsafe_protocol;
use uefi::{Result, Status, StatusExt};

/// `EFI_PCI_IO_PROTOCOL_WIDTH`.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Width {
    U8 = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
}

/// `EFI_PCI_IO_PROTOCOL_ATTRIBUTE_OPERATION`.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum AttributeOp {
    Get = 0,
    Set = 1,
    Enable = 2,
    Disable = 3,
    Supported = 4,
}

/// `AllocateAnyPages` and `EfiBootServicesData`, the only allocation type
/// and one of the two memory types `AllocateBuffer` accepts.
const ALLOCATE_ANY_PAGES: u32 = 0;
const BOOT_SERVICES_DATA: u32 = 4;

/// `EFI_PCI_IO_PROTOCOL_OPERATION` (for `Map`).
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum MapOp {
    BusMasterRead = 0,
    BusMasterWrite = 1,
    BusMasterCommonBuffer = 2,
}

type PollFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    bar_index: u8,
    offset: u64,
    mask: u64,
    value: u64,
    delay: u64,
    result: *mut u64,
) -> Status;

type IoMemFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    bar_index: u8,
    offset: u64,
    count: usize,
    buffer: *mut c_void,
) -> Status;

type ConfigFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    offset: u32,
    count: usize,
    buffer: *mut c_void,
) -> Status;

/// `EFI_PCI_IO_PROTOCOL_ACCESS`.
#[repr(C)]
pub struct Access {
    pub read: IoMemFn,
    pub write: IoMemFn,
}

/// `EFI_PCI_IO_PROTOCOL_CONFIG_ACCESS`.
#[repr(C)]
pub struct ConfigAccess {
    pub read: ConfigFn,
    pub write: ConfigFn,
}

/// `EFI_PCI_IO_PROTOCOL`.
#[repr(C)]
#[unsafe_protocol("4cf5b200-68b8-4ca5-9eec-b23e3f50029a")]
pub struct PciIo {
    pub poll_mem: PollFn,
    pub poll_io: PollFn,
    pub mem: Access,
    pub io: Access,
    pub pci: ConfigAccess,
    pub copy_mem: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        width: Width,
        dest_bar_index: u8,
        dest_offset: u64,
        src_bar_index: u8,
        src_offset: u64,
        count: usize,
    ) -> Status,
    pub map: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        operation: MapOp,
        host_address: *mut c_void,
        number_of_bytes: *mut usize,
        device_address: *mut u64,
        mapping: *mut *mut c_void,
    ) -> Status,
    pub unmap: unsafe extern "efiapi" fn(this: *mut PciIo, mapping: *mut c_void) -> Status,
    pub allocate_buffer: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        alloc_type: u32,
        memory_type: u32,
        pages: usize,
        host_address: *mut *mut c_void,
        attributes: u64,
    ) -> Status,
    pub free_buffer:
        unsafe extern "efiapi" fn(this: *mut PciIo, pages: usize, host_address: *mut c_void) -> Status,
    pub flush: unsafe extern "efiapi" fn(this: *mut PciIo) -> Status,
    pub get_location: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        segment: *mut usize,
        bus: *mut usize,
        device: *mut usize,
        function: *mut usize,
    ) -> Status,
    pub attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        operation: AttributeOp,
        attributes: u64,
        result: *mut u64,
    ) -> Status,
    pub get_bar_attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        bar_index: u8,
        supports: *mut u64,
        resources: *mut *mut c_void,
    ) -> Status,
    pub set_bar_attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        attributes: u64,
        bar_index: u8,
        offset: *mut u64,
        length: *mut u64,
    ) -> Status,
    pub rom_size: u64,
    pub rom_image: *mut c_void,
}

/// Where a function sits on the PCI bus, as `seg:bus:dev.fn`.
#[derive(Clone, Copy)]
pub struct Location {
    pub segment: usize,
    pub bus: usize,
    pub device: usize,
    pub function: usize,
}

impl core::fmt::Display for Location {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.segment, self.bus, self.device, self.function
        )
    }
}

impl PciIo {
    fn this(&self) -> *mut PciIo {
        self as *const PciIo as *mut PciIo
    }

    /// One dword of the function's configuration space.
    pub fn config_read_u32(&self, offset: u32) -> Result<u32> {
        let mut v: u32 = 0;
        // SAFETY: `self` is a live PciIo interface opened by the caller, and
        // the buffer holds exactly one U32.
        unsafe {
            (self.pci.read)(self.this(), Width::U32, offset, 1, (&raw mut v).cast())
        }
        .to_result_with_val(|| v)
    }

    /// One word of the function's configuration space.
    pub fn config_read_u16(&self, offset: u32) -> Result<u16> {
        let mut v: u16 = 0;
        // SAFETY: live PciIo interface; the buffer holds exactly one U16.
        unsafe {
            (self.pci.read)(self.this(), Width::U16, offset, 1, (&raw mut v).cast())
        }
        .to_result_with_val(|| v)
    }

    pub fn config_write_u16(&self, offset: u32, value: u16) -> Result {
        let mut v = value;
        // SAFETY: live PciIo interface; the buffer holds exactly one U16,
        // which the firmware only reads.
        unsafe {
            (self.pci.write)(self.this(), Width::U16, offset, 1, (&raw mut v).cast())
        }
        .to_result()
    }

    pub fn location(&self) -> Result<Location> {
        let mut l = Location { segment: 0, bus: 0, device: 0, function: 0 };
        // SAFETY: `self` is a live PciIo interface; the four out-pointers are
        // valid usizes.
        unsafe {
            (self.get_location)(
                self.this(),
                &raw mut l.segment,
                &raw mut l.bus,
                &raw mut l.device,
                &raw mut l.function,
            )
        }
        .to_result_with_val(|| l)
    }

    /// One value of `width` (1, 2 or 4 bytes) at `offset` in memory BAR
    /// `bar`, zero-extended.
    pub fn mem_read(&self, width: Width, bar: u8, offset: u32) -> Result<u32> {
        let mut v: u32 = 0;
        // SAFETY: live PciIo interface; one element of `width` <= 4 bytes is
        // written to the start of `v` (little-endian, so the value is the
        // zero-extended element).
        unsafe {
            (self.mem.read)(self.this(), width, bar, offset.into(), 1, (&raw mut v).cast())
        }
        .to_result_with_val(|| v)
    }

    /// One value of `width` (1, 2 or 4 bytes; the low bytes of `value`) at
    /// `offset` in memory BAR `bar`.
    pub fn mem_write(&self, width: Width, bar: u8, offset: u32, value: u32) -> Result {
        let mut v = value;
        // SAFETY: live PciIo interface; the firmware reads one element of
        // `width` <= 4 bytes from the start of `v` (little-endian).
        unsafe {
            (self.mem.write)(self.this(), width, bar, offset.into(), 1, (&raw mut v).cast())
        }
        .to_result()
    }

    /// `AllocateBuffer`: `pages` of boot-services data below 4 GB, suitable
    /// for a common-buffer mapping.
    pub fn allocate_buffer(&self, pages: usize) -> Result<*mut u8> {
        let mut host: *mut c_void = core::ptr::null_mut();
        // SAFETY: live PciIo interface; `host` is a valid out-pointer.
        unsafe {
            (self.allocate_buffer)(self.this(), ALLOCATE_ANY_PAGES, BOOT_SERVICES_DATA, pages, &raw mut host, 0)
        }
        .to_result_with_val(|| host.cast())
    }

    /// `FreeBuffer` for memory from `allocate_buffer`.
    ///
    /// # Safety
    /// `host` came from `allocate_buffer(pages)`, is unmapped, and is not
    /// used again.
    pub unsafe fn free_buffer(&self, pages: usize, host: *mut u8) -> Result {
        (self.free_buffer)(self.this(), pages, host.cast()).to_result()
    }

    /// `Map(BusMasterCommonBuffer)` of `bytes` at `host`: the device
    /// address and the mapping token. Fails unless all `bytes` are mapped.
    ///
    /// # Safety
    /// `host` came from `allocate_buffer` and spans at least `bytes`.
    pub unsafe fn map_common(&self, host: *mut u8, bytes: usize) -> Result<(u64, *mut c_void)> {
        let mut len = bytes;
        let mut device: u64 = 0;
        let mut mapping: *mut c_void = core::ptr::null_mut();
        (self.map)(self.this(), MapOp::BusMasterCommonBuffer, host.cast(), &raw mut len, &raw mut device, &raw mut mapping)
            .to_result()?;
        if len != bytes {
            let _ = (self.unmap)(self.this(), mapping);
            return Err(Status::OUT_OF_RESOURCES.into());
        }
        Ok((device, mapping))
    }

    /// `Unmap`.
    ///
    /// # Safety
    /// `mapping` came from `map_common` and the device no longer uses it.
    pub unsafe fn unmap(&self, mapping: *mut c_void) -> Result {
        (self.unmap)(self.this(), mapping).to_result()
    }

    /// `Attributes(op, attributes)`, returning the result word (meaningful
    /// for Get and Supported).
    pub fn attributes(&self, op: AttributeOp, attributes: u64) -> Result<u64> {
        let mut r: u64 = 0;
        // SAFETY: live PciIo interface; `r` is a valid out-pointer.
        unsafe { (self.attributes)(self.this(), op, attributes, &raw mut r) }.to_result_with_val(|| r)
    }
}

/// Start's memory decode and bus mastering (`decode`) through this PciIo.
impl crate::decode::Pci for PciIo {
    type Status = Status;
    fn get(&self) -> core::result::Result<u64, Status> {
        self.attributes(AttributeOp::Get, 0).map_err(|e| e.status())
    }
    fn supported(&self) -> core::result::Result<u64, Status> {
        self.attributes(AttributeOp::Supported, 0).map_err(|e| e.status())
    }
    fn enable(&self, attributes: u64) -> core::result::Result<(), Status> {
        self.attributes(AttributeOp::Enable, attributes).map(|_| ()).map_err(|e| e.status())
    }
    fn disable(&self, attributes: u64) -> core::result::Result<(), Status> {
        self.attributes(AttributeOp::Disable, attributes).map(|_| ()).map_err(|e| e.status())
    }
    fn set(&self, attributes: u64) -> core::result::Result<(), Status> {
        self.attributes(AttributeOp::Set, attributes).map(|_| ()).map_err(|e| e.status())
    }
    fn config_read_u16(&self, offset: u32) -> core::result::Result<u16, Status> {
        PciIo::config_read_u16(self, offset).map_err(|e| e.status())
    }
    fn config_write_u16(&self, offset: u32, value: u16) -> core::result::Result<(), Status> {
        PciIo::config_write_u16(self, offset, value).map_err(|e| e.status())
    }
}
