//! The SNP state machine and data path against the simulated device in
//! `sim.rs`: the calls a firmware MNP or stormbootx's smoltcp makes, in its
//! order, and each call's failure statuses from the UEFI specification.
#[path = "../src/virtio.rs"]
#[allow(dead_code)]
mod virtio;
#[allow(dead_code)]
mod sim;
use sim::*;
use virtio::net::BUF_SIZE;
use virtio::snp::{self, Fail, Received, Snp, State};
use virtio::Error;

const OTHER: [u8; 6] = [0x02, 9, 9, 9, 9, 9];
const MCAST: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];
const MCAST2: [u8; 6] = [0x33, 0x33, 0xff, 0x44, 0x55, 0x66];
type R<T> = Result<T, Fail<&'static str>>;

fn snp() -> (Dev, Snp) {
    let (d, net) = setup();
    (d, Snp::new(net, MULTIPLIER, MAC, true))
}

/// Start, Initialize and the filters MNP asks for: unicast + broadcast.
fn up() -> (Dev, Snp) {
    let (mut d, mut s) = snp();
    s.start::<&str>().unwrap();
    s.initialize(&mut d).unwrap();
    s.receive_filters::<&str>(snp::UNICAST | snp::BROADCAST, 0, false, &[]).unwrap();
    (d, s)
}

fn rx(s: &mut Snp, d: &mut Dev) -> R<Received> {
    let mut buf = [0u8; BUF_SIZE];
    s.receive(d, &mut buf)
}

fn frame(destination: [u8; 6], mark: u8, len: usize) -> Vec<u8> { frame_to(destination, len, mark) }

#[test]
fn state_machine_follows_the_spec() {
    let (mut d, mut s) = snp();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(s.initialize(&mut d), Err(Fail::NotStarted));
    assert_eq!(s.stop(&mut d), Err(Fail::NotStarted));
    assert_eq!(s.shutdown(&mut d), Err(Fail::NotStarted));
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotStarted));
    s.start::<&str>().unwrap();
    assert_eq!(s.start::<&str>(), Err(Fail::AlreadyStarted));
    // Started, not initialized: DEVICE_ERROR for the data path.
    assert_eq!(s.get_status(&mut d, true), Err(Fail::NotInitialized));
    assert_eq!(s.reset(&mut d), Err(Fail::NotInitialized));
    assert_eq!(s.transmit(&mut d, 0, &mut [0; 60], None, None, None, 1), Err(Fail::NotInitialized));
    assert!(!d.running(), "nothing runs before Initialize");
    s.initialize(&mut d).unwrap();
    assert_eq!(s.state, State::Initialized);
    assert!(d.running());
    assert!(s.media);
    s.shutdown(&mut d).unwrap();
    assert_eq!(s.state, State::Started);
    assert_eq!(d.status, 0, "shutdown resets the device");
    s.initialize(&mut d).unwrap();
    // Stop from initialized shuts down first.
    s.stop(&mut d).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(d.status, 0);
}

#[test]
fn initialize_clears_filters() {
    let (mut d, mut s) = snp();
    s.start::<&str>().unwrap();
    s.initialize(&mut d).unwrap();
    assert_eq!(s.setting, 0);
    // Nothing enabled: even a unicast to the station is dropped.
    d.inject(&frame(MAC, 1, 60));
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotReady));
}

#[test]
fn mnp_style_transmit_with_header_fill_and_recycling() {
    let (mut d, mut s) = up();
    let mut buf = vec![0u8; 100];
    buf[14..18].copy_from_slice(b"ping");
    let token = buf.as_ptr() as usize;
    s.transmit(&mut d, 14, &mut buf, None, Some(OTHER), Some(0x0800), token).unwrap();
    // The header was filled in the caller's buffer, source = station.
    assert_eq!(&buf[..6], &OTHER);
    assert_eq!(&buf[6..12], &MAC);
    assert_eq!(&buf[12..14], &[0x08, 0x00]);
    assert_eq!(d.wire, vec![buf.clone()]);
    let (bits, got) = s.get_status(&mut d, true).unwrap();
    assert_eq!(bits & snp::TRANSMIT_INTERRUPT, snp::TRANSMIT_INTERRUPT);
    assert_eq!(got, Some(token));
    // Once only.
    assert_eq!(s.get_status(&mut d, true).unwrap(), (0, None));
}

#[test]
fn transmit_checks_its_parameters() {
    let (mut d, mut s) = up();
    let t = 7;
    assert_eq!(s.transmit(&mut d, 14, &mut [0; 60], None, None, Some(0x800), t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut d, 14, &mut [0; 60], None, Some(OTHER), None, t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut d, 12, &mut [0; 60], None, Some(OTHER), Some(0x800), t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut d, 14, &mut [0; 10], None, Some(OTHER), Some(0x800), t), Err(Fail::BufferTooSmall(14)));
    assert_eq!(s.transmit(&mut d, 0, &mut [0; 10], None, None, None, t), Err(Fail::BufferTooSmall(14)));
    assert_eq!(s.transmit(&mut d, 0, &mut [0; 1519], None, None, None, t), Err(Fail::InvalidParameter));
    assert!(d.wire.is_empty());
    // A complete frame (header 0) goes as given; a short one is padded to 60.
    let mut f = frame(OTHER, 1, 60);
    s.transmit(&mut d, 0, &mut f, None, None, None, t).unwrap();
    assert_eq!(d.wire, vec![frame(OTHER, 1, 60)]);
}

#[test]
fn full_queue_or_recycle_list_is_not_ready() {
    let (mut d, mut s) = up();
    d.hold_tx = true;
    let mut n = 0;
    while s.transmit(&mut d, 0, &mut frame(OTHER, n, 60), None, None, None, n as usize).is_ok() { n += 1; }
    assert_eq!(n, 32);
    assert_eq!(s.transmit(&mut d, 0, &mut frame(OTHER, 0, 60), None, None, None, 0), Err(Fail::NotReady));
    d.release_tx();
    // The queue drains, but nobody recycled: the list fills at RECYCLE.
    let mut m = n as usize;
    while s.transmit(&mut d, 0, &mut frame(OTHER, 0, 60), None, None, None, m).is_ok() { m += 1; }
    assert_eq!(m, snp::RECYCLE);
    // GetStatus hands them back oldest first, then transmit works again.
    for want in 0..3 { assert_eq!(s.get_status(&mut d, true).unwrap().1, Some(want)); }
    s.transmit(&mut d, 0, &mut frame(OTHER, 0, 60), None, None, None, 99).unwrap();
}

#[test]
fn receive_fills_header_fields_and_keeps_a_frame_too_big_for_the_buffer() {
    let (mut d, mut s) = up();
    d.inject(&frame(MAC, 5, 300));
    let (bits, _) = s.get_status(&mut d, false).unwrap();
    assert_eq!(bits & snp::RECEIVE_INTERRUPT, snp::RECEIVE_INTERRUPT);
    assert!(s.frame_waiting(&mut d));
    let mut small = [0u8; 100];
    assert_eq!(s.receive(&mut d, &mut small), Err(Fail::BufferTooSmall(300)));
    let mut buf = [0u8; BUF_SIZE];
    let r = s.receive(&mut d, &mut buf).unwrap();
    assert_eq!(r, Received { len: 300, destination: MAC, source: [0x02, 0, 0, 0, 0, 9], protocol: 0x0800 });
    assert_eq!(buf[20], 5);
    assert_eq!(s.receive(&mut d, &mut buf), Err(Fail::NotReady));
    assert!(!s.frame_waiting(&mut d));
}

#[test]
fn round_trip_through_loopback() {
    let (mut d, mut s) = up();
    d.loopback = true;
    let mut out = vec![0u8; 64];
    s.transmit(&mut d, 14, &mut out, None, Some(MAC), Some(0x88b5), 1).unwrap();
    let r = rx(&mut s, &mut d).unwrap();
    assert_eq!((r.len, r.destination, r.source, r.protocol), (64, MAC, MAC, 0x88b5));
}

#[test]
fn filters_are_exact_in_software() {
    let (mut d, mut s) = up();
    // Broadcast and unicast on; multicast and other stations off.
    for dst in [[0xff; 6], OTHER, MAC, MCAST] { d.inject(&frame(dst, 0, 60)); }
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, [0xff; 6]);
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, MAC);
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotReady));
    // Multicast list: exact match.
    s.receive_filters::<&str>(snp::MULTICAST, 0, false, &[MCAST]).unwrap();
    for dst in [MCAST2, MCAST] { d.inject(&frame(dst, 0, 60)); }
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, MCAST);
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotReady));
    // Unicast off: the station's own frames go.
    s.receive_filters::<&str>(0, snp::UNICAST, false, &[]).unwrap();
    d.inject(&frame(MAC, 0, 60));
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotReady));
    // Promiscuous passes everything.
    s.receive_filters::<&str>(snp::PROMISCUOUS, 0, false, &[]).unwrap();
    for dst in [OTHER, MCAST2] { d.inject(&frame(dst, 0, 60)); }
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, OTHER);
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, MCAST2);
    // All-multicast, list reset.
    s.receive_filters::<&str>(snp::PROMISCUOUS_MULTICAST, snp::PROMISCUOUS, true, &[]).unwrap();
    assert_eq!(s.mcast_count, 0);
    for dst in [OTHER, MCAST2] { d.inject(&frame(dst, 0, 60)); }
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, MCAST2);
    assert_eq!(rx(&mut s, &mut d), Err(Fail::NotReady));
    assert_eq!(d.dropped, 0, "every buffer went back to the device");
}

#[test]
fn receive_filters_checks_its_parameters() {
    let (_d, mut s) = up();
    assert_eq!(s.receive_filters::<&str>(0x20, 0, false, &[]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters::<&str>(0, 0x40, false, &[]), Err(Fail::InvalidParameter));
    // A list needs MULTICAST, multicast addresses, and at most 16.
    assert_eq!(s.receive_filters::<&str>(0, 0, false, &[MCAST]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters::<&str>(snp::MULTICAST, 0, false, &[OTHER]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters::<&str>(snp::MULTICAST, 0, false, &[MCAST; 17]), Err(Fail::InvalidParameter));
    assert_eq!(s.setting, snp::UNICAST | snp::BROADCAST, "unchanged by a refused call");
    s.receive_filters::<&str>(snp::MULTICAST, 0, false, &[MCAST; 16]).unwrap();
    assert_eq!(s.mcast_count, 16);
}

#[test]
fn station_address_is_the_devices_own() {
    let (_d, mut s) = up();
    assert_eq!(s.station_address::<&str>(false, None), Err(Fail::InvalidParameter));
    assert_eq!(s.station_address::<&str>(false, Some(MCAST)), Err(Fail::InvalidParameter));
    assert_eq!(s.station_address::<&str>(false, Some(OTHER)), Err(Fail::Unsupported));
    assert_eq!(s.current, MAC);
    s.station_address::<&str>(false, Some(MAC)).unwrap();
    s.station_address::<&str>(true, None).unwrap();
    assert_eq!(s.current, MAC);
}

#[test]
fn reset_restarts_the_queues_keeping_filters() {
    let (mut d, mut s) = up();
    s.receive_filters::<&str>(snp::MULTICAST, 0, false, &[MCAST]).unwrap();
    s.reset(&mut d).unwrap();
    assert_eq!(s.setting, snp::UNICAST | snp::BROADCAST | snp::MULTICAST);
    assert!(d.running());
    d.inject(&frame(MCAST, 3, 60));
    assert_eq!(rx(&mut s, &mut d).unwrap().destination, MCAST);
}

#[test]
fn media_follows_the_link_status() {
    let (mut d, mut s) = up();
    assert!(s.media);
    d.net_status = 0;
    s.get_status(&mut d, false).unwrap();
    assert!(!s.media);
    d.net_status = 1;
    s.get_status(&mut d, false).unwrap();
    assert!(s.media);
}

#[test]
fn a_device_that_fails_start_leaves_the_interface_started() {
    let (mut d, mut s) = snp();
    s.start::<&str>().unwrap();
    d.reject_features = true;
    assert!(matches!(s.initialize(&mut d), Err(Fail::Device(Error::FeaturesRejected { .. }))));
    assert_eq!(s.state, State::Started);
    assert_eq!(d.status, 0, "reset again");
}

#[test]
fn halt_at_exit_boot_services_resets_the_device() {
    let (mut d, mut s) = up();
    s.halt(&mut d).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(d.status, 0);
    assert!(!s.frame_waiting(&mut d));
}

#[test]
fn multicast_ip_to_mac_per_rfc_1112_and_2464() {
    let mut v4 = [0u8; 16];
    v4[..4].copy_from_slice(&[224, 0x80 | 1, 2, 3]);
    assert_eq!(snp::mcast_ip_to_mac(false, &v4), Some([0x01, 0x00, 0x5e, 0x01, 2, 3]));
    v4[0] = 192;
    assert_eq!(snp::mcast_ip_to_mac(false, &v4), None);
    let mut v6 = [0u8; 16];
    v6[0] = 0xff;
    v6[12..].copy_from_slice(&[0xff, 0x44, 0x55, 0x66]);
    assert_eq!(snp::mcast_ip_to_mac(true, &v6), Some(MCAST2));
    v6[0] = 0xfe;
    assert_eq!(snp::mcast_ip_to_mac(true, &v6), None);
}
