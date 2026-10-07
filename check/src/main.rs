//! stormnic-virtio-check: an EFI application that proves the driver end to
//! end on a machine whose firmware has its own virtio-net driver (OVMF's
//! VirtioNetDxe, which claims the NIC before any media driver runs).
//!
//! 1. Find the virtio-net PCI functions (1af4:1041 or 1af4:1000).
//! 2. Connect it with the firmware's drivers (stormbootx's first pass), then
//!    take it from whatever drives it: DisconnectController on its driver
//!    tree, children first (pve's OVMF stacks MNP/IP4/PXE/HTTP boot on it).
//! 3. Load `\stormboot\drivers\stormnic-virtio.efi` from the volume this app
//!    booted from, start it, and ConnectController the function with that
//!    driver named, so ours binds it.
//! 4. Open the SNP child it made EXCLUSIVE and run smoltcp on it: DHCP, five
//!    pings to the router, a UDP echo (1200 bytes) to the router's port 7
//!    (the stormcentral #310 test peer echoes it; `udp_port=` in
//!    `\stormnic-check.conf` changes the port, 0 skips it).
//! 5. Print `STORMNIC-VIRTIO CHECK PASS: …` or `STORMNIC-VIRTIO CHECK FAIL: …`,
//!    keep answering pings for 15 s (the peer pings the guest), then power off.
//!
//! What it does to the firmware is what stormbootx would do to prefer this
//! driver over the platform's: disconnect, then connect with ours named.
#![no_main]
#![no_std]

extern crate alloc;

#[path = "../../src/decode.rs"]
#[allow(dead_code)]
mod decode;
#[path = "../../src/pci_io.rs"]
#[allow(dead_code)]
mod pci_io;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;
use core::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, icmp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::prelude::*;
use uefi::{cstr16, println, Identify};
use uefi_raw::protocol::network::snp::{ReceiveFlags, SimpleNetworkProtocol};
use uefi_raw::table::boot::BootServices;
use uefi_raw::Boolean;

use pci_io::PciIo;

const PASS: &str = "STORMNIC-VIRTIO CHECK PASS";
const FAIL: &str = "STORMNIC-VIRTIO CHECK FAIL";
const EXCLUSIVE: u32 = 0x20;
const BY_DRIVER: u32 = 0x10;
const BY_CHILD_CONTROLLER: u32 = 0x08;

fn bs() -> &'static BootServices {
    let st = uefi::table::system_table_raw().expect("system table");
    // SAFETY: boot services stay valid while this application runs.
    unsafe { &*st.as_ref().boot_services }
}

/// `EFI_OPEN_PROTOCOL_INFORMATION_ENTRY`.
#[repr(C)]
struct OpenInfo {
    agent: uefi_raw::Handle,
    controller: uefi_raw::Handle,
    attributes: u32,
    count: u32,
}

/// Who has `guid` open on `handle`.
fn open_info(handle: Handle, guid: &uefi::Guid) -> Vec<(uefi_raw::Handle, uefi_raw::Handle, u32)> {
    let mut buf: *const OpenInfo = ptr::null();
    let mut n = 0usize;
    // SAFETY: the firmware fills a pool buffer of `n` entries, freed below.
    unsafe {
        let f: unsafe extern "efiapi" fn(uefi_raw::Handle, *const uefi::Guid, *mut *const OpenInfo, *mut usize) -> Status =
            core::mem::transmute(bs().open_protocol_information);
        if f(handle.as_ptr(), guid, &mut buf, &mut n).is_error() || buf.is_null() {
            return Vec::new();
        }
        let v = core::slice::from_raw_parts(buf, n).iter().map(|e| (e.agent, e.controller, e.attributes)).collect();
        let _ = (bs().free_pool)(buf as *mut _);
        v
    }
}

/// The protocols installed on `handle`.
fn protocols(handle: uefi_raw::Handle) -> Vec<uefi::Guid> {
    let mut buf: *mut *const uefi::Guid = ptr::null_mut();
    let mut n = 0usize;
    // SAFETY: the firmware fills a pool array of `n` GUID pointers, freed below.
    unsafe {
        let f: unsafe extern "efiapi" fn(uefi_raw::Handle, *mut *mut *const uefi::Guid, *mut usize) -> Status =
            core::mem::transmute(bs().protocols_per_handle);
        if f(handle, &mut buf, &mut n).is_error() || buf.is_null() {
            return Vec::new();
        }
        let v = core::slice::from_raw_parts(buf, n).iter().map(|g| **g).collect();
        let _ = (bs().free_pool)(buf as *mut _);
        v
    }
}

/// Handles opened BY_CHILD_CONTROLLER on any protocol of `handle`: its
/// children in the driver tree.
fn children(handle: uefi_raw::Handle) -> Vec<uefi_raw::Handle> {
    let mut out: Vec<uefi_raw::Handle> = Vec::new();
    for g in protocols(handle) {
        // SAFETY: a live handle from the firmware.
        let h = unsafe { Handle::from_ptr(handle) }.expect("handle");
        for e in open_info(h, &g) {
            if e.2 & BY_CHILD_CONTROLLER != 0 && e.1 != handle && !out.contains(&e.1) {
                out.push(e.1);
            }
        }
    }
    out
}

/// Disconnect the drivers on `handle`'s subtree, children first; print each
/// handle that refuses, with its protocols.
fn disconnect_tree(handle: uefi_raw::Handle, depth: usize, failed: &mut usize) {
    if depth > 8 { return; }
    for c in children(handle) {
        disconnect_tree(c, depth + 1, failed);
    }
    // SAFETY: disconnect every driver from a live handle.
    let st = unsafe { (bs().disconnect_controller)(handle, ptr::null_mut(), ptr::null_mut()) };
    if st.is_error() {
        *failed += 1;
        let guids: Vec<String> = protocols(handle).iter().map(|g| format!("{g}")).collect();
        println!("check:   depth {depth} handle {handle:p}: DisconnectController {st:?}; protocols {}", guids.join(" "));
    }
}

/// A millisecond clock: advanced by the poll loop's own stalls.
struct Clock(i64);
impl Clock {
    fn now(&self) -> Instant { Instant::from_millis(self.0) }
    fn tick(&mut self) {
        boot::stall(Duration::from_millis(1));
        self.0 += 1;
    }
}

struct SnpDev {
    snp: *const SimpleNetworkProtocol,
    rx: Vec<u8>,
    frames_in: u32,
    frames_out: u32,
    /// Echo requests addressed to us (smoltcp answers them).
    pings_in: u32,
}

struct Rx<'a>(&'a [u8]);
struct Tx<'a> {
    snp: *const SimpleNetworkProtocol,
    out: &'a mut u32,
}

impl RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R { f(self.0) }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let r = f(&mut frame);
        for _ in 0..100 {
            // SAFETY: the SNP is open EXCLUSIVE; the frame stays alive until
            // GetStatus hands it back below (the driver copies it at once).
            let st = unsafe {
                ((*self.snp).transmit)(self.snp, 0, len, frame.as_ptr() as *const c_void, ptr::null(), ptr::null(), ptr::null())
            };
            if st == Status::SUCCESS {
                *self.out += 1;
                break;
            }
            if st != Status::NOT_READY { break; }
            reclaim(self.snp, None);
        }
        reclaim(self.snp, Some(frame.as_ptr()));
        r
    }
}

/// GetStatus until `mine` is handed back (or nothing more is), so its
/// memory can go.
fn reclaim(snp: *const SimpleNetworkProtocol, mine: Option<*const u8>) {
    for _ in 0..64 {
        let mut buf: *mut c_void = ptr::null_mut();
        // SAFETY: an open SNP; out-pointers valid.
        let st = unsafe { ((*snp).get_status)(snp, ptr::null_mut(), &mut buf) };
        if st.is_error() || buf.is_null() || Some(buf as *const u8) == mine { return; }
    }
}

fn is_ping_to_us(f: &[u8]) -> bool {
    f.len() >= 14 + 20 + 8 && f[12..14] == [0x08, 0x00] && f[23] == 1 && f.get(14 + (f[14] & 0x0f) as usize * 4) == Some(&8)
}

impl Device for SnpDev {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let mut size = self.rx.len();
        // SAFETY: an open SNP; the buffer holds `size` bytes.
        let st = unsafe {
            ((*self.snp).receive)(self.snp, ptr::null_mut(), &mut size, self.rx.as_mut_ptr() as *mut c_void,
                ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
        };
        if st != Status::SUCCESS { return None; }
        self.frames_in += 1;
        if is_ping_to_us(&self.rx[..size]) { self.pings_in += 1; }
        Some((Rx(&self.rx[..size]), Tx { snp: self.snp, out: &mut self.frames_out }))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx { snp: self.snp, out: &mut self.frames_out })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    println!("stormnic-virtio check {}", env!("CARGO_PKG_VERSION"));
    let mut net = None;
    match run(&mut net) {
        Ok(summary) => println!("{PASS}: {summary}"),
        Err(e) => println!("{FAIL}: {e}"),
    }
    if let Some(n) = net.as_mut() {
        n.serve_loud(15_000);
        println!("check: done; {} frames in, {} out, {} ping request(s) to us answered", n.dev.frames_in, n.dev.frames_out, n.dev.pings_in);
    }
    boot::stall(Duration::from_millis(500));
    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None);
}

struct Net {
    dev: SnpDev,
    iface: Interface,
    sockets: SocketSet<'static>,
    clock: Clock,
}

impl Net {
    fn poll(&mut self) {
        self.iface.poll(self.clock.now(), &mut self.dev, &mut self.sockets);
        self.clock.tick();
    }
    fn serve(&mut self, ms: i64) {
        let end = self.clock.0 + ms;
        while self.clock.0 < end { self.poll(); }
    }
    /// `serve`, printing the counts every 3 s.
    fn serve_loud(&mut self, ms: i64) {
        let end = self.clock.0 + ms;
        while self.clock.0 < end {
            self.serve(3000.min(end - self.clock.0));
            println!("check: serving: {} frames in, {} out, {} ping request(s) to us", self.dev.frames_in, self.dev.frames_out, self.dev.pings_in);
        }
    }
}

/// `udp_port=N` from `\stormnic-check.conf`, if there is one; else 7.
fn udp_port(fs: &mut uefi::fs::FileSystem) -> u16 {
    let Ok(text) = fs.read(cstr16!("\\stormnic-check.conf")) else { return 7 };
    String::from_utf8_lossy(&text)
        .lines()
        .find_map(|l| l.trim().strip_prefix("udp_port=").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(7)
}

fn run(out: &mut Option<Net>) -> Result<String, String> {
    let image = boot::image_handle();

    // 1. The virtio-net functions.
    let handles = boot::locate_handle_buffer(SearchType::ByProtocol(&PciIo::GUID)).map_err(|e| format!("no PCI I/O handles: {:?}", e.status()))?;
    let mut found = None;
    for &h in handles.iter() {
        let params = OpenProtocolParams { handle: h, agent: image, controller: None };
        // SAFETY: a GET_PROTOCOL open, dropped at the end of the iteration.
        let Ok(pci) = (unsafe { boot::open_protocol::<PciIo>(params, OpenProtocolAttributes::GetProtocol) }) else { continue };
        let (Ok(id), Ok(class)) = (pci.config_read_u32(0), pci.config_read_u32(8)) else { continue };
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == 0x1af4 && (device == 0x1041 || device == 0x1000) && class >> 24 == 2 {
            let loc = pci.location().map(|l| l.to_string()).unwrap_or_default();
            println!("check: virtio-net 1af4:{device:04x} at {loc}");
            if found.is_none() { found = Some((h, device, loc)); }
        }
    }
    let (ctrl, device, loc) = found.ok_or("no virtio-net PCI function (1af4:1041 or 1af4:1000)")?;

    // 2. Let the firmware's drivers bind it first, as stormbootx's first
    //    ConnectController pass does (OVMF connects only boot devices), then
    //    take it from them.
    // SAFETY: connect the function with whatever drivers the firmware has.
    let st = unsafe { (bs().connect_controller)(ctrl.as_ptr(), ptr::null_mut(), ptr::null_mut(), Boolean::TRUE) };
    println!("check: {loc}: ConnectController with the firmware's drivers: {st:?}");
    let held = |c: Handle| open_info(c, &PciIo::GUID).iter().filter(|e| e.2 & BY_DRIVER != 0).count();
    let holders = held(ctrl);
    println!("check: {loc}: held BY_DRIVER by {holders} firmware driver(s); {} handle(s) below it", children(ctrl.as_ptr()).len());
    // Leaf first: a network stack bound recursively (MNP, IP4, PXE, HTTP
    // boot, …) is taken down from the top, in up to three passes.
    let mut pass = 0;
    while held(ctrl) > 0 && pass < 3 {
        pass += 1;
        let mut failed = 0;
        disconnect_tree(ctrl.as_ptr(), 0, &mut failed);
        println!("check: {loc}: disconnect pass {pass}: {failed} handle(s) refused, {} driver(s) still hold it", held(ctrl));
    }
    if holders > 0 {
        println!("check: {loc}: firmware driver(s) {}", if held(ctrl) == 0 { "disconnected" } else { "still bound" });
    }
    if held(ctrl) > 0 {
        return Err(format!("{loc}: could not disconnect the firmware's driver"));
    }

    // 3. Load and start our driver, then connect the function with it named.
    let sfs = boot::get_image_file_system(image).map_err(|e| format!("boot volume: {:?}", e.status()))?;
    let mut fs = uefi::fs::FileSystem::new(sfs);
    let path = cstr16!("\\stormboot\\drivers\\stormnic-virtio.efi");
    let data = fs.read(path).map_err(|e| format!("reading {path}: {e:?}"))?;
    let port = udp_port(&mut fs);
    drop(fs);
    let mut driver: uefi_raw::Handle = ptr::null_mut();
    // SAFETY: a PE image in memory; the firmware copies it.
    let st = unsafe { (bs().load_image)(Boolean::FALSE, image.as_ptr(), ptr::null(), data.as_ptr() as *const _, data.len(), &mut driver) };
    if st.is_error() { return Err(format!("LoadImage {path}: {st:?}")); }
    // SAFETY: the image just loaded; a driver returns from its entry point.
    let st = unsafe { (bs().start_image)(driver, ptr::null_mut(), ptr::null_mut()) };
    println!("check: {path} ({} bytes) started: {st:?}", data.len());
    if st.is_error() { return Err(format!("StartImage: {st:?}")); }
    let mut list = [driver, ptr::null_mut()];
    // SAFETY: a null-terminated driver list; not recursive (no firmware
    // network stack on top: this app opens the SNP itself).
    let st = unsafe { (bs().connect_controller)(ctrl.as_ptr(), list.as_mut_ptr(), ptr::null_mut(), Boolean::FALSE) };
    println!("check: {loc}: ConnectController with stormnic-virtio: {st:?}");
    if st.is_error() { return Err(format!("{loc}: stormnic-virtio did not bind: {st:?}")); }
    let entries = open_info(ctrl, &PciIo::GUID);
    for e in &entries {
        println!("check: {loc}: PciIo open by agent {:p} for {:p}, attributes {:#x}{}", e.0, e.1, e.2, if e.0 == driver { " (stormnic-virtio)" } else { "" });
    }
    let child = match entries.iter().find(|e| e.0 == driver && e.2 & BY_CHILD_CONTROLLER != 0) {
        Some(e) => e.1,
        // Else: the SNP whose code lives in the driver's image.
        None => ours_by_code(driver).ok_or_else(|| format!("{loc}: no SNP child from stormnic-virtio"))?,
    };

    // 4. Its SNP, EXCLUSIVE.
    let mut iface: *mut c_void = ptr::null_mut();
    // SAFETY: open by this image; never closed (the app powers off).
    let st = unsafe { (bs().open_protocol)(child, &SimpleNetworkProtocol::GUID, &mut iface, image.as_ptr(), ptr::null_mut(), EXCLUSIVE) };
    if st.is_error() { return Err(format!("SNP EXCLUSIVE open: {st:?}")); }
    let snp = iface as *const SimpleNetworkProtocol;
    // SAFETY: the open SNP and its Mode.
    let mac = unsafe {
        let s = ((*snp).start)(snp);
        if s.is_error() && s != Status::ALREADY_STARTED { return Err(format!("SNP Start: {s:?}")); }
        let s = ((*snp).initialize)(snp, 0, 0);
        if s.is_error() { return Err(format!("SNP Initialize: {s:?}")); }
        let s = ((*snp).receive_filters)(snp, ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST, ReceiveFlags::empty(), Boolean::FALSE, 0, ptr::null());
        if s.is_error() { return Err(format!("SNP ReceiveFilters: {s:?}")); }
        let m = &*(*snp).mode;
        println!("check: SNP on {loc}: MAC {}, media {}", mac_text(&m.current_address.0), if bool::from(m.media_present) { "present" } else { "absent" });
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&m.current_address.0[..6]);
        mac
    };

    let mut dev = SnpDev { snp, rx: vec![0u8; 2048], frames_in: 0, frames_out: 0, pings_in: 0 };
    let clock = Clock(0);
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    // SAFETY: RDTSC exists on every x86_64.
    cfg.random_seed = unsafe { core::arch::x86_64::_rdtsc() };
    let iface = Interface::new(cfg, &mut dev, clock.now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let n = out.insert(Net { dev, iface, sockets, clock });

    // DHCP, up to 30 s.
    let mut lease = None;
    while lease.is_none() && n.clock.0 < 30_000 {
        n.poll();
        if let Some(dhcpv4::Event::Configured(c)) = n.sockets.get_mut::<dhcpv4::Socket>(dhcp).poll() {
            n.iface.update_ip_addrs(|a| {
                a.clear();
                let _ = a.push(IpCidr::Ipv4(c.address));
            });
            if let Some(r) = c.router { let _ = n.iface.routes_mut().add_default_ipv4_route(r); }
            println!("check: leased {} gw {} after {} ms", c.address, c.router.map(|r| r.to_string()).unwrap_or("none".into()), n.clock.0);
            lease = Some((c.address, c.router));
        }
    }
    let Some((address, router)) = lease else {
        return Err(format!("no DHCP lease in 30 s ({} frames in, {} out)", n.dev.frames_in, n.dev.frames_out));
    };
    let gw = router.ok_or("the lease has no router to ping")?;

    // Five pings to the router.
    let replies = ping(n, gw, 5)?;
    println!("check: ping {gw}: {}/5 replies", replies.len());
    if replies.len() < 3 {
        return Err(format!("ping {gw}: {}/5 replies", replies.len()));
    }

    // UDP echo.
    let udp = if port == 0 { "UDP echo skipped".to_string() } else {
        let ms = udp_echo(n, gw, port)?;
        println!("check: UDP echo {gw}:{port}: 1200 bytes back in {ms} ms");
        format!("UDP echo {gw}:{port} 1200 B")
    };
    let min = replies.iter().min().copied().unwrap_or(0);
    let max = replies.iter().max().copied().unwrap_or(0);
    Ok(format!(
        "stormnic-virtio on {loc} (1af4:{device:04x}) MAC {}: leased {address} gw {gw}, ping {}/5 (rtt {min}-{max} ms), {udp}",
        mac_text(&mac), replies.len()
    ))
}

/// The SNP handle whose functions are inside `driver`'s loaded image.
fn ours_by_code(driver: uefi_raw::Handle) -> Option<uefi_raw::Handle> {
    let image = boot::image_handle();
    // SAFETY: a live image handle from LoadImage.
    let dh = unsafe { Handle::from_ptr(driver) }?;
    let params = OpenProtocolParams { handle: dh, agent: image, controller: None };
    // SAFETY: GET_PROTOCOL, dropped at once.
    let li = unsafe { boot::open_protocol::<uefi::proto::loaded_image::LoadedImage>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
    let (base, size) = li.info();
    let (lo, hi) = (base as usize, base as usize + size as usize);
    drop(li);
    let snps = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleNetworkProtocol::GUID)).ok()?;
    for &h in snps.iter() {
        let mut iface: *mut c_void = ptr::null_mut();
        // SAFETY: GET_PROTOCOL needs no close.
        let st = unsafe { (bs().open_protocol)(h.as_ptr(), &SimpleNetworkProtocol::GUID, &mut iface, image.as_ptr(), ptr::null_mut(), 0x02) };
        if st.is_error() || iface.is_null() { continue; }
        // SAFETY: an SNP interface.
        let start = unsafe { (*(iface as *const SimpleNetworkProtocol)).start } as usize;
        println!("check: SNP handle {:p}: Start at {start:#x}{}", h.as_ptr(), if (lo..hi).contains(&start) { " (in stormnic-virtio)" } else { "" });
        if (lo..hi).contains(&start) { return Some(h.as_ptr()); }
    }
    None
}

fn mac_text(m: &[u8]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// Echo requests to `to`, one at a time, 2 s each; the round-trip times of
/// the replies.
fn ping(n: &mut Net, to: Ipv4Address, count: u16) -> Result<Vec<i64>, String> {
    let rx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 2048]);
    let tx = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 2048]);
    let mut s = icmp::Socket::new(rx, tx);
    const IDENT: u16 = 0x5376;
    s.bind(icmp::Endpoint::Ident(IDENT)).map_err(|e| format!("ICMP bind: {e:?}"))?;
    let h = n.sockets.add(s);
    let caps = ChecksumCapabilities::default();
    let mut rtts = Vec::new();
    for seq in 0..count {
        let data = b"stormnic-virtio check ping";
        let req = Icmpv4Repr::EchoRequest { ident: IDENT, seq_no: seq, data: &data[..] };
        let sent = n.clock.0;
        {
            let s = n.sockets.get_mut::<icmp::Socket>(h);
            let buf = s.send(req.buffer_len(), IpAddress::Ipv4(to)).map_err(|e| format!("ICMP send: {e:?}"))?;
            req.emit(&mut Icmpv4Packet::new_unchecked(buf), &caps);
        }
        while n.clock.0 < sent + 2000 {
            n.poll();
            let s = n.sockets.get_mut::<icmp::Socket>(h);
            let mut got = false;
            while s.can_recv() {
                let Ok((payload, _)) = s.recv() else { break };
                let Ok(p) = Icmpv4Packet::new_checked(payload) else { continue };
                if let Ok(Icmpv4Repr::EchoReply { seq_no, .. }) = Icmpv4Repr::parse(&p, &caps) {
                    if seq_no == seq { got = true; }
                }
            }
            if got { rtts.push(n.clock.0 - sent); break; }
        }
        n.serve(200);
    }
    n.sockets.remove(h);
    Ok(rtts)
}

/// 1200 bytes to `to`:`port`, three tries of 2 s; the milliseconds until
/// the same bytes came back.
fn udp_echo(n: &mut Net, to: Ipv4Address, port: u16) -> Result<i64, String> {
    let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
    let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
    let mut s = udp::Socket::new(rx, tx);
    s.bind(40007).map_err(|e| format!("UDP bind: {e:?}"))?;
    let h = n.sockets.add(s);
    let payload: Vec<u8> = (0..1200u32).map(|i| (i * 7 + 3) as u8).collect();
    let mut buf = vec![0u8; 2048];
    for _ in 0..3 {
        let sent = n.clock.0;
        n.sockets.get_mut::<udp::Socket>(h).send_slice(&payload, (IpAddress::Ipv4(to), port))
            .map_err(|e| format!("UDP send: {e:?}"))?;
        while n.clock.0 < sent + 2000 {
            n.poll();
            let s = n.sockets.get_mut::<udp::Socket>(h);
            while let Ok((len, _)) = s.recv_slice(&mut buf) {
                if buf[..len] == payload[..] {
                    n.sockets.remove(h);
                    return Ok(n.clock.0 - sent);
                }
            }
        }
    }
    n.sockets.remove(h);
    Err(format!("UDP echo {to}:{port}: nothing back in 3 tries"))
}
