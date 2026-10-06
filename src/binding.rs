//! `EFI_DRIVER_BINDING_PROTOCOL`: which controllers this driver takes.
//! The same glue as stormnic-ixgbe's, for virtio-net.
//!
//! Supported looks at every PCI function the firmware offers. It reads the
//! vendor/device ID with a non-exclusive (GET_PROTOCOL) open and, for one of
//! ours, walks the capability list for the virtio 1.x structures; only then
//! does it try the BY_DRIVER open that Start will hold. A platform driver that
//! already owns the NIC (OVMF's VirtioNetDxe, through its Virtio10Dxe) holds
//! that open, so ours fails and the platform's driver wins.
//!
//! Start holds `EFI_PCI_IO_PROTOCOL` BY_DRIVER, enables memory decode and
//! bus mastering (`decode`), negotiates with the device and reads its MAC
//! and link (`virtio::identify`), maps one DMA region for both queues and
//! their buffers, starts the queues for the DMA check and resets the device
//! again. Last it makes the SNP child handle (`snp`): nothing DMAs until the
//! SNP's user initializes it. Stop with children removes the child; Stop
//! without resets the device, unmaps and frees the region, undoes the PCI
//! changes and releases PciIo.
//!
//! Every virtio network function Supported sees is logged (trace), matched
//! or not, so a verbose boot names what the machine really has.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::time::Duration;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::mem::memory_map::MemoryType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::{Handle, Result, Status};
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::protocol::driver::DriverBindingProtocol;

use crate::console;
use crate::decode::{self, Enabled};
use crate::ids::{self, Nic};
use crate::pci_io::{Location, PciIo, Width};
use crate::snp;
use crate::virtio::net::{self, Dma, Net};
use crate::virtio::{self, Bus, CapError, Caps, Identity, Region};

/// The device's virtio structures, reached through PciIo memory BARs.
pub struct Bars<'a> {
    pub pci: &'a PciIo,
    pub caps: Caps,
}

impl Bus for Bars<'_> {
    type Error = Status;
    fn read(&mut self, region: Region, offset: u32, width: u8) -> core::result::Result<u32, Status> {
        let (bar, at, w) = self.place(region, offset, width)?;
        self.pci.mem_read(w, bar, at).map_err(|e| e.status())
    }
    fn write(&mut self, region: Region, offset: u32, width: u8, value: u32) -> core::result::Result<(), Status> {
        let (bar, at, w) = self.place(region, offset, width)?;
        self.pci.mem_write(w, bar, at, value).map_err(|e| e.status())
    }
    fn delay_us(&mut self, micros: usize) {
        boot::stall(Duration::from_micros(micros as u64));
    }
}

impl Bars<'_> {
    /// BAR, offset in it and PciIo width for an access inside `region`'s
    /// window; INVALID_PARAMETER for one outside it.
    fn place(&self, region: Region, offset: u32, width: u8) -> core::result::Result<(u8, u32, Width), Status> {
        let w = self.caps.window(region).ok_or(Status::UNSUPPORTED)?;
        let end = offset.checked_add(width.into()).ok_or(Status::INVALID_PARAMETER)?;
        if end > w.length {
            return Err(Status::INVALID_PARAMETER);
        }
        let width = match width {
            1 => Width::U8,
            2 => Width::U16,
            4 => Width::U32,
            _ => return Err(Status::INVALID_PARAMETER),
        };
        Ok((w.bar, w.offset.wrapping_add(offset), width))
    }
}

/// A controller this driver has started.
struct Bound {
    controller: Handle,
    location: Option<Location>,
    /// How Start enabled memory decode and bus mastering; Stop undoes it.
    decode: Enabled<Status>,
    /// The queues and buffers, mapped for DMA.
    dma: DmaRegion,
    /// The SNP, its child handle and the queues (`snp::create`).
    port: *mut snp::Port,
    /// The BY_DRIVER open; dropping it closes the protocol.
    pci: ScopedProtocol<PciIo>,
}

/// `net::DMA_PAGES` from AllocateBuffer, mapped as one common buffer.
struct DmaRegion {
    host: *mut u8,
    device: u64,
    mapping: *mut c_void,
}

impl DmaRegion {
    fn new(pci: &PciIo) -> Result<Self> {
        let host = pci.allocate_buffer(net::DMA_PAGES)?;
        // SAFETY: `host` is DMA_PAGES pages from AllocateBuffer.
        match unsafe { pci.map_common(host, net::DMA_PAGES * 4096) } {
            Ok((device, mapping)) => Ok(DmaRegion { host, device, mapping }),
            Err(e) => {
                // SAFETY: allocated above, never mapped or used.
                let _ = unsafe { pci.free_buffer(net::DMA_PAGES, host) };
                Err(e)
            }
        }
    }

    /// Unmap and free. Only once the device is reset.
    fn release(self, pci: &PciIo) {
        // SAFETY: the caller reset the device, so it no longer reads or
        // writes the region; nothing else holds the pointer after this.
        unsafe {
            if let Err(e) = pci.unmap(self.mapping) {
                say!("stormnic-virtio: could not unmap the DMA region: {:?}", e.status());
            }
            if let Err(e) = pci.free_buffer(net::DMA_PAGES, self.host) {
                say!("stormnic-virtio: could not free the DMA region: {:?}", e.status());
            }
        }
    }
}

pub struct VirtioDriver {
    bound: Vec<Bound>,
}

impl VirtioDriver {
    pub const fn new() -> Self {
        VirtioDriver { bound: Vec::new() }
    }
}

/// What config space says about a function.
struct Ident {
    vendor: u16,
    device: u16,
    class: u8,
}

fn ident(pci: &PciIo) -> Result<Ident> {
    let id = pci.config_read_u32(0x00)?;
    let class = pci.config_read_u32(0x08)?;
    Ok(Ident { vendor: id as u16, device: (id >> 16) as u16, class: (class >> 24) as u8 })
}

fn open(agent: Handle, controller: Handle, attrs: OpenProtocolAttributes) -> Result<ScopedProtocol<PciIo>> {
    let params = OpenProtocolParams { handle: controller, agent, controller: Some(controller) };
    // SAFETY: GET_PROTOCOL opens are dropped before Supported/Start return;
    // a BY_DRIVER open is tracked by the firmware, which calls Stop before
    // it removes the interface.
    unsafe { boot::open_protocol::<PciIo>(params, attrs) }
}

pub fn at(location: Option<Location>) -> impl core::fmt::Display {
    struct At(Option<Location>);
    impl core::fmt::Display for At {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self.0 {
                Some(l) => write!(f, "{l}"),
                None => f.write_str("(location unknown)"),
            }
        }
    }
    At(location)
}

pub fn mac_str(m: [u8; 6]) -> impl core::fmt::Display {
    struct M([u8; 6]);
    impl core::fmt::Display for M {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let m = self.0;
            write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
        }
    }
    M(m)
}

fn cap_error(e: &CapError<Status>) -> impl core::fmt::Display + '_ {
    struct C<'a>(&'a CapError<Status>);
    impl core::fmt::Display for C<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self.0 {
                CapError::Io(s) => write!(f, "config space read failed: {s:?}"),
                CapError::NoList => f.write_str("no PCI capability list"),
                CapError::Missing(1) => f.write_str("no virtio common configuration capability"),
                CapError::Missing(2) => f.write_str("no virtio notification capability"),
                CapError::Missing(t) => write!(f, "no virtio capability of type {t}"),
                CapError::Loop => f.write_str("the capability list does not end"),
            }
        }
    }
    C(e)
}

impl VirtioDriver {
    fn is_bound(&self, controller: Handle) -> bool {
        self.bound.iter().any(|b| b.controller == controller)
    }

    /// The controller's entry and virtio structures, if it is one of ours.
    fn ours(&self, agent: Handle, controller: Handle) -> Result<(&'static Nic, Option<Location>, Caps)> {
        // No PciIo: not a PCI function; the common case, and silent.
        let pci = open(agent, controller, OpenProtocolAttributes::GetProtocol)?;
        let id = ident(&pci)?;
        if id.vendor != virtio::VENDOR || id.class != ids::CLASS_NETWORK {
            return Err(Status::UNSUPPORTED.into());
        }
        let location = pci.location().ok();
        let Some(nic) = ids::lookup(id.vendor, id.device) else {
            trace!("stormnic-virtio: {} 1af4:{:04x}: virtio network function, not 1041 or 1000; not binding", at(location), id.device);
            return Err(Status::UNSUPPORTED.into());
        };
        match virtio::capabilities(|off| pci.config_read_u32(off).map_err(|e| e.status())) {
            Ok(caps) => Ok((nic, location, caps)),
            Err(e) => {
                trace!(
                    "stormnic-virtio: {} 1af4:{:04x}: {} (legacy-only virtio); not binding",
                    at(location), nic.device, cap_error(&e)
                );
                Err(Status::UNSUPPORTED.into())
            }
        }
    }

    fn supported(&mut self, agent: Handle, controller: Handle) -> Result {
        if self.is_bound(controller) {
            return Err(Status::ALREADY_STARTED.into());
        }
        let (nic, location, _) = self.ours(agent, controller)?;
        match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(_pci) => {
                trace!("stormnic-virtio: {} 1af4:{:04x} {}: Supported", at(location), nic.device, nic.name);
                Ok(())
            }
            Err(e) => {
                trace!(
                    "stormnic-virtio: {} 1af4:{:04x} {}: already driven by another driver ({:?}); leaving it",
                    at(location), nic.device, nic.name, e.status()
                );
                Err(e)
            }
        }
    }

    fn start(&mut self, agent: Handle, controller: Handle) -> Result {
        console::begin();
        let (nic, location, caps) = self.ours(agent, controller)?;
        let (at, dev) = (at(location), nic.device);
        let pci = match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(pci) => pci,
            Err(e) => {
                fail!("stormnic-virtio: {at} 1af4:{dev:04x}: Start could not open PciIo BY_DRIVER: {:?}", e.status());
                return Err(e);
            }
        };
        trace!(
            "stormnic-virtio: {at} 1af4:{dev:04x}: common cfg BAR {} +{:#x}, notify BAR {} +{:#x} (x{}), device cfg BAR {} +{:#x}",
            caps.common.bar, caps.common.offset, caps.notify.bar, caps.notify.offset, caps.notify_multiplier,
            caps.device.bar, caps.device.offset
        );
        let attributes = match decode::enable(&*pci) {
            Ok(e) => e,
            Err(decode::Error::Config(status)) => {
                fail!("stormnic-virtio: {at} 1af4:{dev:04x}: Start could not enable memory decode and bus mastering: command register access failed: {status:?}");
                return Err(Status::UNSUPPORTED.into());
            }
            Err(decode::Error::NotSet(command, _)) => {
                fail!("stormnic-virtio: {at} 1af4:{dev:04x}: Start could not enable memory decode and bus mastering: command {command:#06x}");
                return Err(Status::UNSUPPORTED.into());
            }
        };
        let mut bars = Bars { pci: &pci, caps };
        let id = match virtio::identify(&mut bars) {
            Ok(id) => id,
            Err(e) => {
                fail!("stormnic-virtio: {at} 1af4:{dev:04x} {}: device not usable: {e:x?}; releasing", nic.name);
                let _ = virtio::reset(&mut bars);
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        trace!(
            "stormnic-virtio: {at} 1af4:{dev:04x}: features {:#x} negotiated, MAC {}, link {}, {} queue(s)",
            id.features, mac_str(id.mac), if id.link { "up" } else { "down" }, id.queues
        );
        let (dma, queues) = match dma_up(&pci, caps, &at, dev, &id) {
            Ok(r) => r,
            Err(()) => {
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        let sizes = queues.sizes();
        let port = match snp::create(agent.as_ptr(), controller.as_ptr(), &pci, caps, queues, id.mac, id.link, location) {
            Ok(p) => p,
            Err((status, in_use)) => {
                fail!("stormnic-virtio: {at} 1af4:{dev:04x}: SNP not installed: {status:?}; releasing");
                if in_use {
                    keep_dma(&pci, &at);
                } else {
                    dma.release(&pci);
                }
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        self.bound.push(Bound { controller, location, decode: attributes, dma, port, pci });
        // The one default line per NIC (stormnic-ixgbe#22).
        say!(
            "stormnic-virtio {}: {at} 1af4:{dev:04x} {}: MAC {}, link {}, queues {}/{}, SNP installed",
            env!("CARGO_PKG_VERSION"), nic.name, mac_str(id.mac), if id.link { "up" } else { "down" }, sizes.0, sizes.1
        );
        console::begin();
        Ok(())
    }

    /// With `children`, remove the SNP child; without, release the NIC.
    fn stop(&mut self, agent: Handle, controller: Handle, children: &[uefi_raw::Handle]) -> Result {
        let Some(i) = self.bound.iter().position(|b| b.controller == controller) else {
            say!("stormnic-virtio: Stop for a controller this driver never started");
            return Err(Status::DEVICE_ERROR.into());
        };
        let (port, location) = (self.bound[i].port, self.bound[i].location);
        // SAFETY: `port` is live until `snp::destroy` below.
        let child = unsafe { (*port).child };
        if !children.is_empty() || child.is_some() {
            if child.is_some_and(|c| children.is_empty() || children.contains(&c)) {
                // SAFETY: as above.
                if let Err(s) = unsafe { snp::remove_child(agent.as_ptr(), controller.as_ptr(), port) } {
                    say!("stormnic-virtio: {}: Stop: SNP child still in use ({s:?}); kept", at(location));
                    return Err(Status::DEVICE_ERROR.into());
                }
                trace!("stormnic-virtio: {}: Stop: SNP child removed", at(location));
            }
            if !children.is_empty() {
                return Ok(());
            }
        }
        let b = self.bound.swap_remove(i);
        // SAFETY: the child is gone, so nothing else reaches the Port.
        match unsafe { snp::destroy(b.port) } {
            Ok(()) => b.dma.release(&b.pci),
            Err(_) => keep_dma(&b.pci, &at(b.location)),
        }
        restore(&b.pci, &b.decode);
        trace!("stormnic-virtio: {}: Stop: released", at(b.location));
        Ok(())
    }
}

/// The device would not reset: it might still DMA into the region, so it is
/// never freed. Bus mastering is turned off at the PCI level instead; the
/// pages stay allocated until reboot.
fn keep_dma(pci: &PciIo, at: &impl core::fmt::Display) {
    let off = decode::stop_bus_master(pci);
    fail!(
        "stormnic-virtio: {at}: could not reset the device; bus mastering {}, DMA region kept allocated",
        if off.is_ok() { "disabled" } else { "could not be disabled" }
    );
}

/// Map the region, start the queues, run the DMA check, and reset the
/// device. Everything is logged; Err(()) means Start fails (the region is
/// released, or kept if the device would not reset).
fn dma_up(pci: &PciIo, caps: Caps, at: &impl core::fmt::Display, dev: u16, id: &Identity)
    -> core::result::Result<(DmaRegion, Net), ()> {
    let region = match DmaRegion::new(pci) {
        Ok(r) => r,
        Err(e) => {
            fail!("stormnic-virtio: {at} 1af4:{dev:04x}: DMA region not mapped: {:?}; releasing", e.status());
            return Err(());
        }
    };
    // SAFETY: the region is DMA_BYTES (rounded to pages) from AllocateBuffer,
    // mapped at `device`, and only these queues use it until it is released.
    let mut queues = unsafe { Net::new(Dma { host: region.host, device: region.device }) };
    let mut bars = Bars { pci, caps };
    let checked = queues.start(&mut bars, caps.notify_multiplier).and_then(|()| {
        trace!(
            "stormnic-virtio: {at} 1af4:{dev:04x}: DMA: {} pages at device {:#x}, queues RX {} / TX {} x {} B",
            net::DMA_PAGES, region.device, queues.sizes().0, queues.sizes().1, net::BUF_SIZE
        );
        net::check(&mut bars, &mut queues, id.mac)
    });
    match &checked {
        Ok(Some(ms)) => trace!("stormnic-virtio: {at} 1af4:{dev:04x}: DMA check: broadcast frame sent, descriptor back after {ms} ms"),
        Ok(None) => say!("stormnic-virtio: {at} 1af4:{dev:04x}: DMA check: the device did not return the frame's descriptor within 100 ms"),
        Err(e) => fail!("stormnic-virtio: {at} 1af4:{dev:04x}: queues failed: {e:x?}"),
    }
    match queues.stop(&mut bars) {
        Ok(_) if checked.is_ok() => Ok((region, queues)),
        Ok(_) => {
            region.release(pci);
            say!("stormnic-virtio: {at} 1af4:{dev:04x}: DMA region released; releasing");
            Err(())
        }
        Err(_) => {
            keep_dma(pci, at);
            Err(())
        }
    }
}

fn restore(pci: &PciIo, e: &Enabled<Status>) {
    if let Err(s) = decode::release(pci, e) {
        say!("stormnic-virtio: could not undo the PCI decode and bus-master changes: {s:?}");
    }
}

/// The driver binding interface and the driver behind it. `protocol` first:
/// firmware's `This` is the Binding.
#[repr(C)]
struct Binding {
    protocol: DriverBindingProtocol,
    driver: VirtioDriver,
}

/// # Safety
/// `this` is the `protocol` of the leaked Binding; the firmware serializes
/// binding calls.
unsafe fn binding<'a>(this: *const DriverBindingProtocol) -> &'a mut Binding {
    unsafe { &mut *(this as *mut Binding) }
}

fn handles(this: *const DriverBindingProtocol, controller: uefi_raw::Handle) -> Option<(Handle, Handle)> {
    if this.is_null() { return None; }
    // SAFETY: the firmware passes our interface and a controller handle.
    unsafe {
        let agent = Handle::from_ptr((*this).driver_binding_handle)?;
        Some((agent, Handle::from_ptr(controller)?))
    }
}

unsafe extern "efiapi" fn binding_supported(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.supported(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_start(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.start(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_stop(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    count: usize, children: *const uefi_raw::Handle) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    if count > 0 && children.is_null() { return Status::INVALID_PARAMETER; }
    let children = if count == 0 { &[][..] } else {
        // SAFETY: the firmware passes `count` child handles.
        unsafe { core::slice::from_raw_parts(children, count) }
    };
    match unsafe { binding(this) }.driver.stop(agent, controller, children) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

/// Install `EFI_DRIVER_BINDING_PROTOCOL` on the image handle. The image
/// must have been loaded as a boot-service driver (its code and data stay
/// after the entry point returns).
pub fn install() -> Result {
    let image = boot::image_handle();
    {
        let loaded = boot::open_protocol_exclusive::<LoadedImage>(image)?;
        if loaded.code_type() != MemoryType::BOOT_SERVICES_CODE || loaded.data_type() != MemoryType::BOOT_SERVICES_DATA {
            return Err(Status::UNSUPPORTED.into());
        }
    }
    let b = Box::into_raw(Box::new(Binding {
        protocol: DriverBindingProtocol {
            supported: binding_supported,
            start: binding_start,
            stop: binding_stop,
            version: 1,
            image_handle: image.as_ptr(),
            driver_binding_handle: image.as_ptr(),
        },
        driver: VirtioDriver::new(),
    }));
    // SAFETY: the Binding is leaked, so the interface lives as long as the
    // image; the GUID matches the interface.
    let r = unsafe {
        boot::install_protocol_interface(Some(image), &DriverBindingProtocol::GUID, (&raw const (*b).protocol).cast())
    };
    if r.is_err() {
        // SAFETY: not installed, so nothing else holds it.
        drop(unsafe { Box::from_raw(b) });
    }
    r.map(|_| ())
}
