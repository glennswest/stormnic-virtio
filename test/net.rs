//! The virtio-net queues against the simulated device in `sim.rs`.
#[path = "../src/virtio.rs"]
#[allow(dead_code)]
mod virtio;
#[allow(dead_code)]
mod sim;
use sim::*;
use virtio::net::{self, BUF_SIZE};
use virtio::Error;

fn pos(d: &Dev, op: (u32, u32)) -> usize {
    d.ops.iter().position(|o| *o == op).unwrap_or_else(|| panic!("no {op:x?}"))
}

#[test]
fn start_programs_both_queues_and_posts_every_receive_buffer() {
    let (d, net) = started();
    assert_eq!(net.sizes(), (32, 32));
    let (rx, tx) = (d.queue(0), d.queue(1));
    assert_eq!((rx.size, rx.msix, rx.enable), (32, 0xffff, 1));
    assert_eq!((rx.desc, rx.driver, rx.device), (DEVICE_BASE, DEVICE_BASE + 512, DEVICE_BASE + 1024));
    assert_eq!((tx.desc, tx.driver, tx.device), (DEVICE_BASE + 2048, DEVICE_BASE + 2560, DEVICE_BASE + 3072));
    assert_eq!(d.status, 15, "ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK");
    // Receive: 32 device-writable 2 KiB buffers from 4096, all available,
    // and no interrupts asked for.
    for i in 0..32u64 {
        let desc = DEVICE_BASE + 16 * i;
        assert_eq!((d.r64(desc), d.r32(desc + 8), d.r16(desc + 12)), (DEVICE_BASE + 4096 + 2048 * i, 2048, 2));
    }
    assert_eq!((d.r16(DEVICE_BASE + 512), d.r16(DEVICE_BASE + 514)), (1, 32));
    // Order: addresses, enable, DRIVER_OK, then the receive notify.
    let enable = pos(&d, (28, 1));
    assert!(pos(&d, (52, 1)) < enable, "addresses before enable");
    assert!(enable < pos(&d, (20, 15)) && pos(&d, (20, 15)) < pos(&d, (0x1000, 0)));
    assert!(d.ops.iter().all(|o| *o != (0x1000 + 3 * MULTIPLIER, 1)), "nothing to send yet");
}

#[test]
fn a_device_offering_small_queues_gets_them() {
    let (mut d, mut net) = setup();
    d.q[0].max = 8;
    d.q[1].max = 24;
    net.start(&mut d, MULTIPLIER).unwrap();
    assert_eq!(net.sizes(), (8, 16));
    assert_eq!(d.r16(DEVICE_BASE + 514), 8);
}

#[test]
fn a_missing_queue_fails_start_and_resets_the_device() {
    let (mut d, mut net) = setup();
    d.q[1].max = 0;
    assert_eq!(net.start(&mut d, MULTIPLIER), Err(Error::NoQueue { queue: 1 }));
    assert_eq!(d.status, 0);
    assert!(!net.running);
}

#[test]
fn a_frame_round_trips_through_transmit_and_receive() {
    let (mut d, mut net) = started();
    d.loopback = true;
    let f = frame_to([0xff; 6], 100, 7);
    assert!(net.transmit(&mut d, &f).unwrap());
    assert_eq!(d.wire, vec![f.clone()]);
    assert_eq!(net.peek(&mut d).unwrap(), Some((100, [0xff; 6])));
    let mut out = [0u8; BUF_SIZE];
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), Some(100));
    assert_eq!(&out[..100], &f[..]);
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), None);
    // The buffer went back: 33 published, and the device was told.
    assert_eq!(d.r16(DEVICE_BASE + 514), 33);
    assert_eq!(d.ops.iter().filter(|o| **o == (0x1000, 0)).count(), 2);
    assert_eq!(net.tx_pending::<&str>().unwrap(), 0);
}

#[test]
fn short_frames_are_padded_and_bad_lengths_refused() {
    let (mut d, mut net) = started();
    assert!(net.transmit(&mut d, &frame_to([0xff; 6], 42, 1)).unwrap());
    assert_eq!(d.wire[0].len(), 60);
    assert_eq!(&d.wire[0][42..], &[0; 18]);
    assert_eq!(net.transmit(&mut d, &[0; 13]), Err(Error::FrameLength { len: 13 }));
    assert_eq!(net.transmit(&mut d, &[0; 1519]), Err(Error::FrameLength { len: 1519 }));
    assert!(net.transmit(&mut d, &frame_to([0xff; 6], 1518, 1)).unwrap());
}

#[test]
fn indexes_wrap_past_65535_without_losing_a_frame() {
    let (mut d, mut net) = started();
    d.loopback = true;
    let mut out = [0u8; BUF_SIZE];
    for n in 0..70_000u32 {
        let f = frame_to(sim::MAC, 64 + (n % 64) as usize, n as u8);
        assert!(net.transmit(&mut d, &f).unwrap(), "frame {n}");
        assert_eq!(net.receive(&mut d, &mut out).unwrap(), Some(f.len()), "frame {n}");
        assert_eq!(out[20], n as u8);
    }
    assert_eq!(d.dropped, 0);
}

#[test]
fn a_full_transmit_queue_refuses_until_the_device_returns_descriptors() {
    let (mut d, mut net) = started();
    d.hold_tx = true;
    for i in 0..32 { assert!(net.transmit(&mut d, &frame_to([0xff; 6], 60, i)).unwrap()); }
    assert!(!net.transmit(&mut d, &frame_to([0xff; 6], 60, 0)).unwrap());
    assert_eq!(net.tx_pending::<&str>().unwrap(), 32);
    d.release_tx();
    assert_eq!(net.reclaim::<&str>().unwrap(), 32);
    assert!(net.transmit(&mut d, &frame_to([0xff; 6], 60, 0)).unwrap());
    assert_eq!(d.wire.len(), 33);
}

#[test]
fn malformed_receive_elements_are_dropped_and_their_buffers_reposted() {
    let (mut d, mut net) = started();
    // A used element shorter than header + Ethernet header, then a good one.
    let addr = DEVICE_BASE + 4096;
    d.q[0].seen = 2;
    d.push_raw(0, 0, 20);
    d.put(addr + 2048 + 12, &frame_to(sim::MAC, 64, 3));
    d.push_raw(0, 1, 12 + 64);
    let mut out = [0u8; BUF_SIZE];
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), Some(64));
    assert_eq!(d.r16(DEVICE_BASE + 514), 34, "both buffers back");
    // An id the device was never given.
    d.push_raw(0, 40, 100);
    assert_eq!(net.receive(&mut d, &mut out), Err(Error::BadUsed { queue: 0, id: 40 }));
}

#[test]
fn a_frame_longer_than_the_caller_buffer_stays_queued() {
    let (mut d, mut net) = started();
    assert!(d.inject(&frame_to(sim::MAC, 300, 1)));
    let mut small = [0u8; 100];
    assert_eq!(net.receive(&mut d, &mut small), Err(Error::FrameLength { len: 300 }));
    let mut out = [0u8; BUF_SIZE];
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), Some(300));
}

#[test]
fn skip_drops_the_peeked_frame() {
    let (mut d, mut net) = started();
    d.inject(&frame_to([0x02, 1, 1, 1, 1, 1], 60, 1));
    d.inject(&frame_to(sim::MAC, 70, 2));
    assert_eq!(net.peek(&mut d).unwrap(), Some((60, [0x02, 1, 1, 1, 1, 1])));
    net.skip(&mut d).unwrap();
    assert_eq!(net.peek(&mut d).unwrap(), Some((70, sim::MAC)));
}

#[test]
fn stop_drains_transmit_then_resets_the_device() {
    let (mut d, mut net) = started();
    assert!(net.transmit(&mut d, &frame_to([0xff; 6], 60, 0)).unwrap());
    assert_eq!(net.stop(&mut d).unwrap(), 0);
    assert_eq!(d.status, 0);
    assert_eq!(d.queue(0).enable, 0);
    // A device that never sends: 100 ms, then reset anyway.
    let (mut d, mut net) = started();
    d.hold_tx = true;
    assert!(net.transmit(&mut d, &frame_to([0xff; 6], 60, 0)).unwrap());
    assert_eq!(net.stop(&mut d).unwrap(), 1);
    assert_eq!((d.status, d.delays), (0, 100));
    // Frames from the network after a reset find no buffer.
    assert!(!d.inject(&frame_to(sim::MAC, 60, 0)));
}

#[test]
fn the_dma_check_sends_one_broadcast_and_sees_its_descriptor_back() {
    let (mut d, mut net) = started();
    assert_eq!(net::check(&mut d, &mut net, sim::MAC).unwrap(), Some(0));
    assert_eq!(d.wire, vec![net::check_frame(sim::MAC).to_vec()]);
    assert_eq!(&d.wire[0][14..39], b"stormnic-virtio DMA check");
    let (mut d, mut net) = started();
    d.hold_tx = true;
    assert_eq!(net::check(&mut d, &mut net, sim::MAC).unwrap(), None);
    assert_eq!(d.delays, 101);
}

#[test]
fn start_after_stop_starts_clean() {
    let (mut d, mut net) = started();
    d.loopback = true;
    for i in 0..5 { net.transmit(&mut d, &frame_to(sim::MAC, 60, i)).unwrap(); }
    net.stop(&mut d).unwrap();
    net.start(&mut d, MULTIPLIER).unwrap();
    let mut out = [0u8; BUF_SIZE];
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), None, "nothing left over from before the reset");
    net.transmit(&mut d, &frame_to(sim::MAC, 61, 9)).unwrap();
    assert_eq!(net.receive(&mut d, &mut out).unwrap(), Some(61));
}
