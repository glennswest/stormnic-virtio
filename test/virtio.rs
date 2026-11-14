//! Capability parsing, feature negotiation and identification (virtio spec
//! 4.1.4, 3.1.1) against config-space images and the simulated device.
#[path = "../src/virtio.rs"]
#[allow(dead_code)]
mod virtio;
#[allow(dead_code)]
mod sim;
use sim::*;
use virtio::{CapError, Caps, Duplex, Error, PortLine, Speed, Window};

/// Config space with the status register's capability bit, the pointer at
/// 0x34, and `caps` as (offset, bytes) — QEMU's layout by default.
fn config(caps: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut c = vec![0u8; 256];
    c[0..4].copy_from_slice(&0x1041_1af4u32.to_le_bytes());
    c[6] = 0x10;
    c[0x34] = caps.first().map_or(0, |(o, _)| *o as u8);
    for (i, (o, b)) in caps.iter().enumerate() {
        c[*o..*o + b.len()].copy_from_slice(b);
        c[*o + 1] = caps.get(i + 1).map_or(0, |(n, _)| *n as u8);
    }
    c
}

fn vcap(kind: u8, bar: u8, offset: u32, length: u32, extra: Option<u32>) -> Vec<u8> {
    let mut v = vec![0x09, 0, if extra.is_some() { 20 } else { 16 }, kind, bar, 0, 0, 0];
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(&length.to_le_bytes());
    if let Some(m) = extra { v.extend_from_slice(&m.to_le_bytes()); }
    v
}

fn parse(c: &[u8]) -> Result<Caps, CapError<()>> {
    virtio::capabilities(|off| Ok(u32::from_le_bytes(c[off as usize..off as usize + 4].try_into().unwrap())))
}

fn qemu() -> Vec<(usize, Vec<u8>)> {
    vec![
        (0x98, vec![0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), // MSI-X
        (0x84, vcap(5, 0, 0, 0, Some(0))),                    // PCI cfg access
        (0x70, vcap(2, 4, 0x3000, 0x1000, Some(4))),
        (0x60, vcap(4, 4, 0x2000, 0x1000, None)),
        (0x50, vcap(3, 4, 0x1000, 0x1000, None)),
        (0x40, vcap(1, 4, 0x0000, 0x1000, None)),
    ]
}

#[test]
fn finds_the_structures_qemu_offers() {
    let caps = parse(&config(&qemu())).unwrap();
    assert_eq!(caps.common, Window { bar: 4, offset: 0, length: 0x1000 });
    assert_eq!(caps.isr, Some(Window { bar: 4, offset: 0x1000, length: 0x1000 }));
    assert_eq!(caps.device, Window { bar: 4, offset: 0x2000, length: 0x1000 });
    assert_eq!(caps.notify, Window { bar: 4, offset: 0x3000, length: 0x1000 });
    assert_eq!(caps.notify_multiplier, 4);
}

#[test]
fn the_first_usable_capability_of_a_type_wins_and_reserved_bars_are_ignored() {
    let mut c = qemu();
    c.insert(0, (0xb0, vcap(1, 7, 0x8000, 0x1000, None)));  // reserved BAR: ignored
    c.push((0xc0, vcap(1, 2, 0x9000, 0x1000, None)));       // after the first good one
    let caps = parse(&config(&c)).unwrap();
    assert_eq!(caps.common, Window { bar: 4, offset: 0, length: 0x1000 });
}

#[test]
fn a_legacy_only_device_has_no_virtio_structures() {
    let c = config(&[(0x40, vec![0x11, 0, 0, 0, 0, 0, 0, 0])]);
    assert_eq!(parse(&c), Err(CapError::Missing(1)));
    // A notify capability too short for its multiplier does not count.
    let mut q = qemu();
    q[2] = (0x70, vcap(2, 4, 0x3000, 0x1000, None));
    assert_eq!(parse(&config(&q)), Err(CapError::Missing(2)));
    let mut none = config(&[]);
    none[6] = 0;
    assert_eq!(parse(&none), Err(CapError::NoList));
}

#[test]
fn a_capability_list_that_loops_is_refused() {
    let mut c = config(&qemu());
    c[0x41] = 0x40;
    assert_eq!(parse(&c), Err(CapError::Loop));
}

#[test]
fn negotiation_follows_spec_3_1_1_and_accepts_only_what_the_driver_handles() {
    let mut d = Dev::new();
    let f = virtio::negotiate(&mut d).unwrap();
    assert_eq!(f, 1 << 5 | 1 << 16 | 1 << 32, "MAC, STATUS, VERSION_1; no CSUM, MRG_RXBUF, CTRL_VQ, EVENT_IDX");
    assert_eq!(d.driver_features, f);
    let statuses: Vec<u32> = d.ops.iter().filter(|(o, _)| *o == 20).map(|(_, v)| *v).collect();
    assert_eq!(statuses, vec![0, 1, 3, 11], "reset, ACKNOWLEDGE, DRIVER, FEATURES_OK");
}

#[test]
fn access_platform_and_order_platform_are_accepted_when_offered() {
    let mut d = Dev::new();
    d.offered |= 1 << 33 | 1 << 36 | 1 << 34;
    let f = virtio::negotiate(&mut d).unwrap();
    assert_eq!(f, 1 << 5 | 1 << 16 | 1 << 32 | 1 << 33 | 1 << 36, "RING_PACKED (34) left alone");
}

#[test]
fn a_device_without_version_1_or_mac_is_refused_and_marked_failed() {
    let mut d = Dev::new();
    d.offered &= !(1 << 32);
    assert!(matches!(virtio::negotiate(&mut d), Err(Error::NotModern { .. })));
    assert_eq!(d.status, 1 | 2 | 128);
    let mut d = Dev::new();
    d.offered &= !(1 << 5);
    assert!(matches!(virtio::negotiate(&mut d), Err(Error::NoMac { .. })));
}

#[test]
fn features_ok_that_does_not_stay_set_is_an_error() {
    let mut d = Dev::new();
    d.reject_features = true;
    assert!(matches!(virtio::negotiate(&mut d), Err(Error::FeaturesRejected { status: 3, .. })));
}

#[test]
fn reset_waits_for_device_status_to_read_zero() {
    let mut d = Dev::new();
    d.status = 15;
    d.reset_delay = 3;
    virtio::reset(&mut d).unwrap();
    assert_eq!(d.delays, 3);
    let mut d = Dev::new();
    d.reset_delay = 5000;
    assert_eq!(virtio::reset(&mut d), Err(Error::ResetTimeout { status: 1 }));
    assert_eq!(d.delays, 1000);
}

#[test]
fn identify_reads_mac_link_and_queues_and_leaves_the_device_reset() {
    let mut d = Dev::new();
    let id = virtio::identify(&mut d).unwrap();
    assert_eq!((id.mac, id.link, id.queues), (MAC, true, 3));
    assert_eq!(d.status, 0);
    d.net_status = 0;
    assert!(!virtio::identify(&mut d).unwrap().link);
    // Without VIRTIO_NET_F_STATUS the link is up.
    d.offered &= !(1 << 16);
    assert!(virtio::identify(&mut d).unwrap().link);
}

#[test]
fn speed_and_duplex_are_read_only_when_speed_duplex_is_negotiated() {
    let mut d = Dev::new();
    d.speed = 10_000;
    d.duplex = 1;
    let id = virtio::identify(&mut d).unwrap();
    assert_eq!(id.features & 1 << 63, 0, "not offered, not accepted");
    assert_eq!(id.speed, Speed { mbps: None, duplex: None });
    d.offered |= 1 << 63;
    let id = virtio::identify(&mut d).unwrap();
    assert_ne!(id.features & 1 << 63, 0, "accepted when offered");
    assert_eq!(id.speed, Speed { mbps: Some(10_000), duplex: Some(Duplex::Full) });
    d.duplex = 0;
    assert_eq!(virtio::identify(&mut d).unwrap().speed.duplex, Some(Duplex::Half));
    // QEMU's default: speed and duplex unknown.
    d.speed = 0xffff_ffff;
    d.duplex = 0xff;
    assert_eq!(virtio::identify(&mut d).unwrap().speed, Speed { mbps: None, duplex: None });
    // Above 0x7fffffff is not a speed.
    d.speed = 0x8000_0000;
    assert_eq!(virtio::identify(&mut d).unwrap().speed.mbps, None);
}

#[test]
fn the_standard_fec_of_a_virtual_port_is_none_at_any_speed() {
    for mbps in [None, Some(1000), Some(10_000), Some(25_000), Some(100_000)] {
        let s = Speed { mbps, duplex: Some(Duplex::Full) };
        assert_eq!(virtio::standard_fec(s), virtio::Fec::None);
    }
}

fn line(link: bool, speed: Speed) -> String {
    PortLine { version: "0.2.0", at: "0000:00:03.0", device: 0x1041, name: "virtio-net (modern)", mac: MAC, link, speed }
        .to_string()
}

#[test]
fn the_port_line_names_driver_address_mac_media_speed_fec_and_link() {
    assert_eq!(
        line(true, Speed { mbps: Some(10_000), duplex: Some(Duplex::Full) }),
        "stormnic-virtio 0.2.0: 0000:00:03.0 1af4:1041 virtio-net (modern) port 0: MAC 52:54:00:12:34:56, \
         media virtual, speed 10000 Mb/s full duplex, FEC none, link UP"
    );
    assert_eq!(
        line(true, Speed { mbps: None, duplex: None }),
        "stormnic-virtio 0.2.0: 0000:00:03.0 1af4:1041 virtio-net (modern) port 0: MAC 52:54:00:12:34:56, \
         media virtual, speed unknown, FEC none, link UP"
    );
}

#[test]
fn a_port_without_link_is_an_error_line() {
    let l = line(false, Speed { mbps: None, duplex: None });
    assert!(l.contains(", FEC none, link DOWN: error: "), "{l}");
}

#[test]
fn the_mac_is_read_again_when_the_config_generation_changes() {
    let mut d = Dev::new();
    d.change_during_read = true;
    assert_eq!(virtio::identify(&mut d).unwrap().mac, MAC);
    assert_eq!(d.generation, 1);
}

#[test]
fn an_io_failure_is_reported() {
    let mut d = Dev::new();
    d.fail_io = true;
    assert_eq!(virtio::identify(&mut d), Err(Error::Io("io")));
}

#[test]
fn queue_sizes_are_powers_of_two_at_most_32() {
    use virtio::queue::size_for;
    assert_eq!([size_for(256), size_for(32), size_for(16), size_for(24), size_for(1), size_for(0)], [32, 32, 16, 16, 1, 0]);
}
