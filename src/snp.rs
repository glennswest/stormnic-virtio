//! `EFI_SIMPLE_NETWORK_PROTOCOL` on a child handle, from the UEFI
//! specification's Simple Network Protocol section. The same glue as
//! stormnic-virtio's (its #4).
//!
//! Start (binding.rs) makes one `Port` per NIC: the protocol interface and
//! its Mode, the `virtio::snp` state machine that owns the queues, and a
//! child handle carrying the SNP and a device path (the controller's, plus a
//! MAC address node). The child opens the controller's PciIo
//! BY_CHILD_CONTROLLER, so DisconnectController on the NIC stops the child
//! first. stormbootx (or the check app) opens the child's SNP EXCLUSIVE.
//!
//! Every call runs at TPL_CALLBACK (the spec's limit for SNP), so a timer
//! poll never enters the driver while another call is in it. WaitForPacket
//! is a NOTIFY_WAIT event that is signalled when a frame is waiting. An
//! ExitBootServices event resets the device, so nothing DMAs into memory
//! the OS owns next.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::protocol::network::snp::{
    InterruptStatus, NetworkMode, NetworkState, NetworkStatistics, ReceiveFlags, SimpleNetworkProtocol,
};
use uefi_raw::table::boot::{BootServices, EventType, Tpl};
use uefi_raw::{Boolean, Event, Guid, Handle, IpAddress, MacAddress, Status};

use crate::binding::{at, mac_str, Bars};
use crate::pci_io::{Location, PciIo};
use crate::virtio::net::Net;
use crate::virtio::snp::{self, Fail, Snp, State, MAX_MCAST};
use crate::virtio::{Caps, Error};

/// `EFI_SIMPLE_NETWORK_PROTOCOL_REVISION`.
const REVISION: u64 = 0x0001_0000;
/// `NET_IFTYPE_ETHERNET` (RFC 1700 hardware type).
const IFTYPE_ETHERNET: u8 = 1;
/// `EFI_OPEN_PROTOCOL_GET_PROTOCOL` and `BY_CHILD_CONTROLLER`.
const GET_PROTOCOL: u32 = 0x02;
const BY_CHILD_CONTROLLER: u32 = 0x08;
const PCI_IO_GUID: Guid = uefi::guid!("4cf5b200-68b8-4ca5-9eec-b23e3f50029a");

pub fn bs() -> &'static BootServices {
    let st = uefi::table::system_table_raw().expect("system table");
    // SAFETY: the system table and its boot services stay valid while this
    // boot-service driver runs (the ExitBootServices handler is the last use).
    unsafe { &*st.as_ref().boot_services }
}

/// One NIC's SNP. `snp` comes first: firmware's `This` is the Port.
#[repr(C)]
pub struct Port {
    snp: SimpleNetworkProtocol,
    mode: NetworkMode,
    core: Snp,
    pci: *const PciIo,
    caps: Caps,
    /// The child handle, while the SNP is installed on it.
    pub child: Option<Handle>,
    path: Vec<u8>,
    exit: Event,
    /// Inside an SNP call; the WaitForPacket check leaves the queues alone.
    busy: bool,
    /// For the console.
    location: Option<Location>,
}

/// The controller's device path with a MAC node (messaging 3, subtype 11,
/// 37 bytes: 32-byte address, IfType) before the end node.
fn mac_path(parent: *const u8, mac: [u8; 6]) -> Vec<u8> {
    let mut path = Vec::new();
    if !parent.is_null() {
        let mut p = parent;
        // SAFETY: a device path from the firmware: nodes with a 4-byte header
        // whose length covers the node, ending in the end-entire node.
        unsafe {
            loop {
                let (kind, sub) = (*p, *p.add(1));
                let len = u16::from_le_bytes([*p.add(2), *p.add(3)]) as usize;
                if (kind == 0x7f && sub == 0xff) || len < 4 { break; }
                path.extend_from_slice(core::slice::from_raw_parts(p, len));
                p = p.add(len);
            }
        }
    }
    path.extend_from_slice(&[3, 11, 37, 0]);
    let mut addr = [0u8; 32];
    addr[..6].copy_from_slice(&mac);
    path.extend_from_slice(&addr);
    path.push(IFTYPE_ETHERNET);
    path.extend_from_slice(&[0x7f, 0xff, 4, 0]);
    path
}

fn status<E>(f: &Fail<E>) -> Status {
    match f {
        Fail::NotStarted => Status::NOT_STARTED,
        Fail::AlreadyStarted => Status::ALREADY_STARTED,
        Fail::NotInitialized | Fail::Device(_) => Status::DEVICE_ERROR,
        Fail::InvalidParameter => Status::INVALID_PARAMETER,
        Fail::Unsupported => Status::UNSUPPORTED,
        Fail::BufferTooSmall(_) => Status::BUFFER_TOO_SMALL,
        Fail::NotReady => Status::NOT_READY,
    }
}

/// TPL_CALLBACK for the length of one SNP call, and the `busy` mark.
struct Call<'a> {
    port: &'a mut Port,
    old: Tpl,
}

impl<'a> Call<'a> {
    /// # Safety
    /// `this` is the `snp` field of a live Port.
    unsafe fn enter(this: *const SimpleNetworkProtocol) -> Call<'a> {
        // SAFETY: raise_tpl from <= TPL_CALLBACK, the SNP calling limit.
        let old = unsafe { (bs().raise_tpl)(Tpl::CALLBACK) };
        // SAFETY: `snp` is the first field of the repr(C) Port (caller).
        let port = unsafe { &mut *(this as *mut Port) };
        port.busy = true;
        Call { port, old }
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        self.port.busy = false;
        sync(self.port);
        // SAFETY: back to the TPL the caller had.
        unsafe { (bs().restore_tpl)(self.old) };
    }
}

/// Mode reflects the state machine after every call.
fn sync(p: &mut Port) {
    let (c, m) = (&p.core, &mut p.mode);
    m.state = match c.state {
        State::Stopped => NetworkState::STOPPED,
        State::Started => NetworkState::STARTED,
        State::Initialized => NetworkState::INITIALIZED,
    };
    m.receive_filter_setting = c.setting;
    m.mcast_filter_count = c.mcast_count as u32;
    for (i, slot) in m.mcast_filter.iter_mut().enumerate() {
        *slot = if i < c.mcast_count { c.mcast[i].into() } else { MacAddress::default() };
    }
    m.current_address = c.current.into();
    m.media_present = c.media.into();
}

impl Port {
    fn io(&self) -> Bars<'static> {
        // SAFETY: the BY_DRIVER PciIo open outlives the Port (binding.rs
        // frees the Port before it drops the open).
        Bars { pci: unsafe { &*self.pci }, caps: self.caps }
    }

    fn log_fail<E: core::fmt::Debug>(&self, call: &str, f: &Fail<E>) {
        if let Fail::Device(e) = f {
            say!("stormnic-virtio: {}: SNP {call} failed: {e:x?}", at(self.location));
        }
    }
}

// The SNP functions. Each is entered through `Call` (TPL_CALLBACK).

unsafe extern "efiapi" fn start(this: *const SimpleNetworkProtocol) -> Status {
    let c = unsafe { Call::enter(this) };
    match c.port.core.start::<Status>() {
        Ok(()) => Status::SUCCESS,
        Err(f) => status(&f),
    }
}

unsafe extern "efiapi" fn stop(this: *const SimpleNetworkProtocol) -> Status {
    let c = unsafe { Call::enter(this) };
    let mut io = c.port.io();
    match c.port.core.stop(&mut io) {
        Ok(()) => Status::SUCCESS,
        Err(f) => { c.port.log_fail("Stop", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn initialize(this: *const SimpleNetworkProtocol, _extra_rx: usize, _extra_tx: usize) -> Status {
    let c = unsafe { Call::enter(this) };
    let mut io = c.port.io();
    match c.port.core.initialize(&mut io) {
        Ok(()) => {
            trace!(
                "stormnic-virtio: {}: SNP initialized, MAC {}, media {}",
                at(c.port.location), mac_str(c.port.core.current), if c.port.core.media { "present" } else { "absent" }
            );
            Status::SUCCESS
        }
        Err(f) => { c.port.log_fail("Initialize", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn reset(this: *const SimpleNetworkProtocol, _extended: Boolean) -> Status {
    let c = unsafe { Call::enter(this) };
    let mut io = c.port.io();
    match c.port.core.reset(&mut io) {
        Ok(()) => Status::SUCCESS,
        Err(f) => { c.port.log_fail("Reset", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn shutdown(this: *const SimpleNetworkProtocol) -> Status {
    let c = unsafe { Call::enter(this) };
    let mut io = c.port.io();
    match c.port.core.shutdown(&mut io) {
        Ok(()) => { trace!("stormnic-virtio: {}: SNP shut down", at(c.port.location)); Status::SUCCESS }
        Err(f) => { c.port.log_fail("Shutdown", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn receive_filters(this: *const SimpleNetworkProtocol, enable: ReceiveFlags,
    disable: ReceiveFlags, reset_mcast: Boolean, count: usize, list: *const MacAddress) -> Status {
    let c = unsafe { Call::enter(this) };
    let reset_mcast = reset_mcast.0 != 0;
    let mut addrs = [[0u8; 6]; MAX_MCAST];
    let n = if reset_mcast { 0 } else { count };
    if n > MAX_MCAST || (n > 0 && list.is_null()) { return Status::INVALID_PARAMETER; }
    for (i, a) in addrs.iter_mut().enumerate().take(n) {
        // SAFETY: the caller passes `count` addresses at `list`.
        *a = unsafe { *list.add(i) }.into();
    }
    let before = c.port.core.setting;
    match c.port.core.receive_filters::<Status>(enable.bits(), disable.bits(), reset_mcast, &addrs[..n]) {
        Ok(()) => {
            if c.port.core.setting != before || n > 0 {
                trace!(
                    "stormnic-virtio: {}: SNP receive filters {:#04x}, {} multicast address(es)",
                    at(c.port.location), c.port.core.setting, c.port.core.mcast_count
                );
            }
            Status::SUCCESS
        }
        Err(f) => { c.port.log_fail("ReceiveFilters", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn station_address(this: *const SimpleNetworkProtocol, reset: Boolean,
    new: *const MacAddress) -> Status {
    let c = unsafe { Call::enter(this) };
    // SAFETY: non-null `new` points to an EFI_MAC_ADDRESS.
    let new = (!new.is_null()).then(|| <[u8; 6]>::from(unsafe { *new }));
    match c.port.core.station_address::<Status>(reset.0 != 0, new) {
        Ok(()) => {
            trace!("stormnic-virtio: {}: SNP station address {}", at(c.port.location), mac_str(c.port.core.current));
            Status::SUCCESS
        }
        Err(f) => { c.port.log_fail("StationAddress", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn statistics(this: *const SimpleNetworkProtocol, _reset: Boolean,
    _size: *mut usize, _table: *mut NetworkStatistics) -> Status {
    let c = unsafe { Call::enter(this) };
    if c.port.core.state == State::Stopped { Status::NOT_STARTED } else { Status::UNSUPPORTED }
}

unsafe extern "efiapi" fn mcast_ip_to_mac(this: *const SimpleNetworkProtocol, ipv6: Boolean,
    ip: *const IpAddress, mac: *mut MacAddress) -> Status {
    let c = unsafe { Call::enter(this) };
    if c.port.core.state == State::Stopped { return Status::NOT_STARTED; }
    if ip.is_null() || mac.is_null() { return Status::INVALID_PARAMETER; }
    // SAFETY: EFI_IP_ADDRESS is 16 bytes (IPv4 in the first 4).
    let bytes = unsafe { *(ip as *const [u8; 16]) };
    match snp::mcast_ip_to_mac(ipv6.0 != 0, &bytes) {
        // SAFETY: `mac` is a valid EFI_MAC_ADDRESS out-pointer.
        Some(m) => { unsafe { *mac = m.into() }; Status::SUCCESS }
        None => Status::INVALID_PARAMETER,
    }
}

unsafe extern "efiapi" fn nv_data(this: *const SimpleNetworkProtocol, _read: Boolean, _offset: usize,
    _size: usize, _buffer: *mut c_void) -> Status {
    let c = unsafe { Call::enter(this) };
    if c.port.core.state == State::Stopped { Status::NOT_STARTED } else { Status::UNSUPPORTED }
}

unsafe extern "efiapi" fn get_status(this: *const SimpleNetworkProtocol, interrupts: *mut InterruptStatus,
    tx_buf: *mut *mut c_void) -> Status {
    let c = unsafe { Call::enter(this) };
    let mut io = c.port.io();
    match c.port.core.get_status(&mut io, !tx_buf.is_null()) {
        Ok((bits, token)) => {
            // SAFETY: non-null out-pointers from the caller.
            unsafe {
                if !interrupts.is_null() { *interrupts = InterruptStatus::from_bits_truncate(bits); }
                if !tx_buf.is_null() { *tx_buf = token.map_or(ptr::null_mut(), |t| t as *mut c_void); }
            }
            Status::SUCCESS
        }
        Err(f) => { c.port.log_fail("GetStatus", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn transmit(this: *const SimpleNetworkProtocol, header_size: usize, size: usize,
    buffer: *const c_void, source: *const MacAddress, destination: *const MacAddress,
    protocol: *const u16) -> Status {
    let c = unsafe { Call::enter(this) };
    if buffer.is_null() { return Status::INVALID_PARAMETER; }
    if size > crate::virtio::net::MAX_FRAME { return Status::INVALID_PARAMETER; }
    // SAFETY: the caller's `size`-byte buffer; the spec has the driver fill
    // in its media header when `header_size` is not 0.
    let buf = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, size) };
    // SAFETY: non-null pointers from the caller.
    let mac = |p: *const MacAddress| (!p.is_null()).then(|| <[u8; 6]>::from(unsafe { *p }));
    let protocol = (!protocol.is_null()).then(|| unsafe { *protocol });
    let mut io = c.port.io();
    match c.port.core.transmit(&mut io, header_size, buf, mac(source), mac(destination), protocol, buffer as usize) {
        Ok(()) => Status::SUCCESS,
        Err(f) => { c.port.log_fail("Transmit", &f); status(&f) }
    }
}

unsafe extern "efiapi" fn receive(this: *const SimpleNetworkProtocol, header_size: *mut usize,
    size: *mut usize, buffer: *mut c_void, source: *mut MacAddress, destination: *mut MacAddress,
    protocol: *mut u16) -> Status {
    let c = unsafe { Call::enter(this) };
    if size.is_null() || buffer.is_null() { return Status::INVALID_PARAMETER; }
    // SAFETY: the caller's buffer of `*size` bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, *size) };
    let mut io = c.port.io();
    match c.port.core.receive(&mut io, out) {
        Ok(r) => {
            // SAFETY: non-null out-pointers from the caller.
            unsafe {
                *size = r.len;
                if !header_size.is_null() { *header_size = snp::HEADER; }
                if !source.is_null() { *source = r.source.into(); }
                if !destination.is_null() { *destination = r.destination.into(); }
                if !protocol.is_null() { *protocol = r.protocol; }
            }
            Status::SUCCESS
        }
        Err(f) => {
            // SAFETY: as above; the frame stays queued for a bigger buffer.
            if let Fail::BufferTooSmall(len) = f { unsafe { *size = len; } }
            c.port.log_fail("Receive", &f);
            status(&f)
        }
    }
}

/// WaitForPacket's check (NOTIFY_WAIT): signal when a frame is waiting.
unsafe extern "efiapi" fn wait_notify(event: Event, context: *mut c_void) {
    // SAFETY: the context is the live Port (the event is closed before the
    // Port is freed).
    let port = unsafe { &mut *(context as *mut Port) };
    if port.busy { return; }
    let mut io = port.io();
    if port.core.frame_waiting(&mut io) {
        // SAFETY: our own event.
        unsafe { let _ = (bs().signal_event)(event); }
    }
}

/// ExitBootServices: stop the queues. No console output: the OS may have
/// the console by now.
unsafe extern "efiapi" fn exit_notify(_event: Event, context: *mut c_void) {
    // SAFETY: as `wait_notify`.
    let port = unsafe { &mut *(context as *mut Port) };
    let mut io = port.io();
    let _ = port.core.halt(&mut io);
}

/// Make the Port and its child handle. The device is reset; `pci` is the
/// controller's BY_DRIVER PciIo open, kept until `destroy`. Err: the status,
/// and whether the Port (and so the DMA region) had to be left in place.
#[allow(clippy::too_many_arguments)]
pub fn create(agent: Handle, controller: Handle, pci: &PciIo, caps: Caps, queues: Net,
    mac: [u8; 6], media: bool, location: Option<Location>) -> Result<*mut Port, (Status, bool)> {
    let bs = bs();
    let mut parent: *mut c_void = ptr::null_mut();
    // SAFETY: GET_PROTOCOL needs no close; a missing path leaves `parent` null.
    let _ = unsafe {
        (bs.open_protocol)(controller, &DevicePathProtocol::GUID, &mut parent, agent, controller, GET_PROTOCOL)
    };
    let port = Box::into_raw(Box::new(Port {
        snp: SimpleNetworkProtocol {
            revision: REVISION,
            start, stop, initialize, reset, shutdown, receive_filters, station_address, statistics,
            multicast_ip_to_mac: mcast_ip_to_mac, non_volatile_data: nv_data, get_status, transmit, receive,
            wait_for_packet: ptr::null_mut(),
            mode: ptr::null_mut(),
        },
        mode: NetworkMode {
            state: NetworkState::STOPPED,
            hw_address_size: 6,
            media_header_size: snp::HEADER as u32,
            max_packet_size: snp::MAX_PACKET as u32,
            nv_ram_size: 0,
            nv_ram_access_size: 0,
            receive_filter_mask: snp::FILTER_MASK,
            receive_filter_setting: 0,
            max_mcast_filter_count: MAX_MCAST as u32,
            mcast_filter_count: 0,
            mcast_filter: [MacAddress::default(); 16],
            current_address: mac.into(),
            broadcast_address: [0xff; 6].into(),
            permanent_address: mac.into(),
            if_type: IFTYPE_ETHERNET,
            // No VIRTIO_NET_F_CTRL_MAC_ADDR: the device's address is fixed.
            mac_address_changeable: Boolean::FALSE,
            multiple_tx_supported: Boolean::TRUE,
            media_present_supported: Boolean::TRUE,
            media_present: media.into(),
        },
        core: Snp::new(queues, caps.notify_multiplier, mac, media),
        pci: pci as *const PciIo,
        caps,
        child: None,
        path: mac_path(parent as *const u8, mac),
        exit: ptr::null_mut(),
        busy: false,
        location,
    }));
    // SAFETY: `port` is live until `destroy`, which undoes each step below.
    unsafe {
        let p = &mut *port;
        p.snp.mode = &raw mut p.mode;
        let ctx = port as *mut c_void;
        let r = (bs.create_event)(EventType::NOTIFY_WAIT, Tpl::NOTIFY, Some(wait_notify), ctx, &mut p.snp.wait_for_packet);
        if r.is_error() { let _ = destroy(port); return Err((r, false)); }
        let r = (bs.create_event)(EventType::SIGNAL_EXIT_BOOT_SERVICES, Tpl::CALLBACK, Some(exit_notify), ctx, &mut p.exit);
        if r.is_error() { p.exit = ptr::null_mut(); let _ = destroy(port); return Err((r, false)); }
        let mut child: Handle = ptr::null_mut();
        let r = (bs.install_multiple_protocol_interfaces)(
            &mut child,
            &SimpleNetworkProtocol::GUID as *const Guid, &raw const p.snp as *const c_void,
            &DevicePathProtocol::GUID as *const Guid, p.path.as_ptr() as *const c_void,
            ptr::null::<c_void>(),
        );
        if r.is_error() { let _ = destroy(port); return Err((r, false)); }
        p.child = Some(child);
        let mut iface: *mut c_void = ptr::null_mut();
        let r = (bs.open_protocol)(controller, &PCI_IO_GUID, &mut iface, agent, child, BY_CHILD_CONTROLLER);
        if r.is_error() {
            // A child that will not go keeps pointing at the Port: leak both.
            if remove_child(agent, controller, port).is_err() { return Err((r, true)); }
            let _ = destroy(port);
            return Err((r, false));
        }
    }
    Ok(port)
}

/// Stop with children: close the child's PciIo open and uninstall its
/// protocols; the device is reset. Err: the SNP is still in use.
///
/// # Safety
/// `port` came from `create` and was not destroyed.
pub unsafe fn remove_child(agent: Handle, controller: Handle, port: *mut Port) -> Result<(), Status> {
    let bs = bs();
    let p = unsafe { &mut *port };
    let Some(child) = p.child else { return Ok(()) };
    unsafe {
        let _ = (bs.close_protocol)(controller, &PCI_IO_GUID, agent, child);
        let r = (bs.uninstall_multiple_protocol_interfaces)(
            child,
            &SimpleNetworkProtocol::GUID as *const Guid, &raw const p.snp as *const c_void,
            &DevicePathProtocol::GUID as *const Guid, p.path.as_ptr() as *const c_void,
            ptr::null::<c_void>(),
        );
        if r.is_error() {
            let mut iface: *mut c_void = ptr::null_mut();
            let _ = (bs.open_protocol)(controller, &PCI_IO_GUID, &mut iface, agent, child, BY_CHILD_CONTROLLER);
            return Err(r);
        }
    }
    p.child = None;
    let mut io = p.io();
    let _ = p.core.halt(&mut io);
    Ok(())
}

/// Stop with no children: reset the device, close the events and free the
/// Port. Returns the reset's result: on Err the device may still DMA.
///
/// # Safety
/// `port` came from `create`, its child is removed, and it is not used after.
pub unsafe fn destroy(port: *mut Port) -> Result<(), Error<Status>> {
    let bs = bs();
    let mut p = unsafe { Box::from_raw(port) };
    unsafe {
        if !p.snp.wait_for_packet.is_null() { let _ = (bs.close_event)(p.snp.wait_for_packet); }
        if !p.exit.is_null() { let _ = (bs.close_event)(p.exit); }
    }
    let mut io = p.io();
    p.core.state = State::Stopped;
    p.core.net.stop(&mut io).map(|_| ())
}
