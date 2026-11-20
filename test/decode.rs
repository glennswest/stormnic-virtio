//! Memory decode and bus mastering (src/decode.rs) against a fake PciIo,
//! including release after a device that would not reset (finding
//! b0a545cc9c24). The module and the tests before `release_kept` are
//! stormnic-ixgbe's (its test/decode.rs).
#[path = "../src/decode.rs"]
mod decode;
use decode::*;
use std::cell::RefCell;

/// A fake PciIo: attributes track the command register the way the PCI bus
/// driver does, and each operation can be made to fail like AMI Aptio 4.
#[derive(Default)]
struct Fake {
    command: RefCell<u16>,
    supported: u64,
    fail_get: bool,
    fail_supported: bool,
    /// Enable fails when asked for more than one bit at once.
    fail_combined: bool,
    /// Every attribute call fails.
    fail_all: bool,
    /// Command register bits the hardware ignores on write.
    stuck_clear: u16,
    fail_config: bool,
    calls: RefCell<Vec<String>>,
}

const ERR: &str = "UNSUPPORTED";

fn bits(a: u64) -> u16 {
    let mut c = 0;
    if a & ATTRIBUTE_MEMORY != 0 {
        c |= COMMAND_MEMORY;
    }
    if a & ATTRIBUTE_BUS_MASTER != 0 {
        c |= COMMAND_BUS_MASTER;
    }
    c
}

impl Pci for Fake {
    type Status = &'static str;
    fn get(&self) -> Result<u64, &'static str> {
        self.calls.borrow_mut().push("get".into());
        if self.fail_get || self.fail_all {
            return Err(ERR);
        }
        let c = *self.command.borrow();
        Ok((if c & COMMAND_MEMORY != 0 { ATTRIBUTE_MEMORY } else { 0 })
            | (if c & COMMAND_BUS_MASTER != 0 { ATTRIBUTE_BUS_MASTER } else { 0 }))
    }
    fn supported(&self) -> Result<u64, &'static str> {
        self.calls.borrow_mut().push("supported".into());
        if self.fail_supported || self.fail_all {
            return Err(ERR);
        }
        Ok(self.supported)
    }
    fn enable(&self, a: u64) -> Result<(), &'static str> {
        self.calls.borrow_mut().push(format!("enable {a:#x}"));
        if self.fail_all || a & !self.supported != 0 || (self.fail_combined && a.count_ones() > 1) {
            return Err(ERR);
        }
        *self.command.borrow_mut() |= bits(a);
        Ok(())
    }
    fn disable(&self, a: u64) -> Result<(), &'static str> {
        self.calls.borrow_mut().push(format!("disable {a:#x}"));
        if self.fail_all {
            return Err(ERR);
        }
        *self.command.borrow_mut() &= !bits(a);
        Ok(())
    }
    fn set(&self, a: u64) -> Result<(), &'static str> {
        self.calls.borrow_mut().push(format!("set {a:#x}"));
        if self.fail_all {
            return Err(ERR);
        }
        let mut c = self.command.borrow_mut();
        *c = (*c & !(COMMAND_MEMORY | COMMAND_BUS_MASTER)) | bits(a);
        Ok(())
    }
    fn config_read_u16(&self, offset: u32) -> Result<u16, &'static str> {
        assert_eq!(offset, COMMAND);
        if self.fail_config {
            return Err("DEVICE_ERROR");
        }
        Ok(*self.command.borrow())
    }
    fn config_write_u16(&self, offset: u32, v: u16) -> Result<(), &'static str> {
        assert_eq!(offset, COMMAND);
        self.calls.borrow_mut().push(format!("write {v:#06x}"));
        if self.fail_config {
            return Err("DEVICE_ERROR");
        }
        *self.command.borrow_mut() = v & !self.stuck_clear;
        Ok(())
    }
}

const BOTH: u64 = ATTRIBUTE_MEMORY | ATTRIBUTE_BUS_MASTER;
const CMD: u16 = COMMAND_MEMORY | COMMAND_BUS_MASTER;
/// I/O space enable and SERR#: bits that must survive.
const OTHER: u16 = 0x0101;

fn fake() -> Fake {
    Fake { supported: BOTH | 0x1 | 0x8000, command: RefCell::new(OTHER), ..Default::default() }
}

#[test]
fn edk2_path_uses_attributes_only() {
    let f = fake();
    let e = enable(&f).unwrap();
    assert_eq!(e.by_attributes, BOTH);
    assert_eq!(e.command_before, None);
    assert_eq!(e.command, OTHER | CMD);
    assert!(!f.calls.borrow().iter().any(|c| c.starts_with("write")));
    release(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER);
    assert_eq!(f.calls.borrow().last().unwrap(), "set 0x0");
}

#[test]
fn supported_without_bus_master_falls_back_to_command_register() {
    // The old code refused here with UNSUPPORTED before calling Enable.
    let f = Fake { supported: ATTRIBUTE_MEMORY, ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.by_attributes, ATTRIBUTE_MEMORY);
    assert_eq!(e.command_before, Some(OTHER | COMMAND_MEMORY));
    assert_eq!(e.command, OTHER | CMD);
    release(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER);
}

#[test]
fn every_attribute_call_failing_still_enables_through_config() {
    let f = Fake { fail_all: true, ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.original, Err(ERR));
    assert_eq!(e.supported, Err(ERR));
    assert_eq!(e.enable, Err(ERR));
    assert_eq!(e.by_attributes, 0);
    assert_eq!(e.command_before, Some(OTHER));
    assert_eq!(*f.command.borrow(), OTHER | CMD);
    release(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER);
    // Nothing was enabled through attributes, so release made no attribute call.
    assert!(!f.calls.borrow().iter().any(|c| c.starts_with("set") || c.starts_with("disable")));
}

#[test]
fn combined_enable_refused_tries_each_bit() {
    let f = Fake { fail_combined: true, ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.enable, Err(ERR));
    assert_eq!(e.by_attributes, BOTH);
    assert_eq!(e.command_before, None);
    let calls = f.calls.borrow().clone();
    assert!(calls.contains(&"enable 0x2".to_string()) && calls.contains(&"enable 0x4".to_string()));
}

#[test]
fn get_failing_releases_with_disable() {
    let f = Fake { fail_get: true, ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.by_attributes, BOTH);
    release(&f, &e).unwrap();
    assert_eq!(f.calls.borrow().last().unwrap(), "disable 0x6");
    assert_eq!(*f.command.borrow(), OTHER);
}

#[test]
fn supported_failing_still_asks_for_both() {
    let f = Fake { fail_supported: true, ..fake() };
    let e = enable(&f).unwrap();
    assert!(f.calls.borrow().contains(&"enable 0x6".to_string()));
    assert_eq!(e.by_attributes, BOTH);
}

#[test]
fn bits_that_will_not_set_fail_start() {
    let f = Fake { fail_all: true, stuck_clear: COMMAND_BUS_MASTER, ..fake() };
    match enable(&f) {
        Err(Error::NotSet(c, e)) => {
            assert_eq!(c, OTHER | COMMAND_MEMORY);
            assert_eq!(e.command_before, Some(OTHER));
        }
        r => panic!("{r:?}"),
    }
}

#[test]
fn config_access_failing_fails_start() {
    let f = Fake { fail_all: true, fail_config: true, ..fake() };
    assert_eq!(enable(&f), Err(Error::Config("DEVICE_ERROR")));
}

#[test]
fn already_enabled_by_firmware_is_left_as_found() {
    let f = Fake { fail_all: true, command: RefCell::new(OTHER | CMD), ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.command_before, None);
    release(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | CMD);
}

#[test]
fn stop_bus_master_clears_bme_when_disable_is_refused() {
    let f = Fake { fail_all: true, command: RefCell::new(OTHER | CMD), ..fake() };
    stop_bus_master(&f).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | COMMAND_MEMORY);
}

#[test]
fn stop_bus_master_reports_a_stuck_bme() {
    struct Stuck(Fake);
    impl Pci for Stuck {
        type Status = &'static str;
        fn get(&self) -> Result<u64, &'static str> { self.0.get() }
        fn supported(&self) -> Result<u64, &'static str> { self.0.supported() }
        fn enable(&self, a: u64) -> Result<(), &'static str> { self.0.enable(a) }
        fn disable(&self, _: u64) -> Result<(), &'static str> { Err(ERR) }
        fn set(&self, a: u64) -> Result<(), &'static str> { self.0.set(a) }
        fn config_read_u16(&self, o: u32) -> Result<u16, &'static str> { self.0.config_read_u16(o) }
        fn config_write_u16(&self, _: u32, _: u16) -> Result<(), &'static str> { Ok(()) }
    }
    let f = Stuck(Fake { command: RefCell::new(OTHER | CMD), ..fake() });
    assert_eq!(stop_bus_master(&f), Err(None));
}

// Finding b0a545cc9c24: a device that would not reset may still DMA, so
// releasing it must leave bus mastering off even when firmware had it on.

#[test]
fn release_after_bme_stopped_turns_it_back_on() {
    // What the old Stop / Start failure path did: stop_bus_master, then
    // release. The firmware had BME on, so release put it back on.
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    let e = enable(&f).unwrap();
    stop_bus_master(&f).unwrap();
    release(&f, &e).unwrap();
    assert_eq!(*f.command.borrow() & COMMAND_BUS_MASTER, COMMAND_BUS_MASTER);
}

#[test]
fn release_kept_leaves_bme_off_when_firmware_had_it_on() {
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.original, Ok(BOTH));
    release_kept(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | COMMAND_MEMORY);
    assert!(f.calls.borrow().contains(&"set 0x2".to_string()));
}

#[test]
fn release_kept_through_config_leaves_bme_off() {
    // Attributes refused (AMI Aptio 4): Start set MSE | BME by config write.
    let f = Fake { fail_all: true, ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.command_before, Some(OTHER));
    release_kept(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER);
}

#[test]
fn release_kept_with_memory_already_on_keeps_it() {
    let f = Fake { fail_all: true, command: RefCell::new(OTHER | COMMAND_MEMORY), ..fake() };
    let e = enable(&f).unwrap();
    assert_eq!(e.command_before, Some(OTHER | COMMAND_MEMORY));
    release_kept(&f, &e).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | COMMAND_MEMORY);
}

#[test]
fn release_kept_with_get_failing_disables_both() {
    let f = Fake { fail_get: true, command: RefCell::new(OTHER | CMD), ..fake() };
    let e = enable(&f).unwrap();
    release_kept(&f, &e).unwrap();
    assert_eq!(*f.command.borrow() & COMMAND_BUS_MASTER, 0);
}

#[test]
fn release_kept_checks_bme_after_firmware_sets_it_again() {
    // A firmware whose Set turns on whatever Start found (it ignores the
    // bits asked for): the last check clears BME again.
    struct Sticky(Fake);
    impl Pci for Sticky {
        type Status = &'static str;
        fn get(&self) -> Result<u64, &'static str> { self.0.get() }
        fn supported(&self) -> Result<u64, &'static str> { self.0.supported() }
        fn enable(&self, a: u64) -> Result<(), &'static str> { self.0.enable(a) }
        fn disable(&self, a: u64) -> Result<(), &'static str> { self.0.disable(a) }
        fn set(&self, _: u64) -> Result<(), &'static str> { self.0.set(BOTH) }
        fn config_read_u16(&self, o: u32) -> Result<u16, &'static str> { self.0.config_read_u16(o) }
        fn config_write_u16(&self, o: u32, v: u16) -> Result<(), &'static str> { self.0.config_write_u16(o, v) }
    }
    let f = Sticky(Fake { command: RefCell::new(OTHER | CMD), ..fake() });
    let e = enable(&f).unwrap();
    release_kept(&f, &e).unwrap();
    assert_eq!(*f.0.command.borrow() & COMMAND_BUS_MASTER, 0);
}

#[test]
fn release_kept_reports_a_stuck_bme() {
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    let e = enable(&f).unwrap();
    // Neither attributes nor config writes clear BME.
    struct NoClear(Fake);
    impl Pci for NoClear {
        type Status = &'static str;
        fn get(&self) -> Result<u64, &'static str> { self.0.get() }
        fn supported(&self) -> Result<u64, &'static str> { self.0.supported() }
        fn enable(&self, a: u64) -> Result<(), &'static str> { self.0.enable(a) }
        fn disable(&self, _: u64) -> Result<(), &'static str> { Err(ERR) }
        fn set(&self, _: u64) -> Result<(), &'static str> { Err(ERR) }
        fn config_read_u16(&self, o: u32) -> Result<u16, &'static str> { self.0.config_read_u16(o) }
        fn config_write_u16(&self, o: u32, v: u16) -> Result<(), &'static str> {
            self.0.config_write_u16(o, v | COMMAND_BUS_MASTER)
        }
    }
    assert_eq!(release_kept(&NoClear(f), &e), Err(None));
}

// ExitBootServices (finding b0a545cc9c24): a device that would not reset
// has BME cleared, with config accesses only.

#[test]
fn exit_with_device_reset_leaves_the_command_register() {
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    contain_at_exit(&f, &Ok::<(), ()>(())).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | CMD);
    assert!(f.calls.borrow().is_empty());
}

#[test]
fn exit_with_reset_failed_clears_bme_by_config_only() {
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    contain_at_exit(&f, &Err::<(), _>("reset timed out")).unwrap();
    assert_eq!(*f.command.borrow(), OTHER | COMMAND_MEMORY);
    // No attribute call in an ExitBootServices notify: only the write.
    assert_eq!(*f.calls.borrow(), vec![format!("write {:#06x}", OTHER | COMMAND_MEMORY)]);
}

#[test]
fn exit_with_reset_failed_reports_a_stuck_bme() {
    let f = Fake { command: RefCell::new(OTHER | CMD), ..fake() };
    struct NoClear(Fake);
    impl Pci for NoClear {
        type Status = &'static str;
        fn get(&self) -> Result<u64, &'static str> { self.0.get() }
        fn supported(&self) -> Result<u64, &'static str> { self.0.supported() }
        fn enable(&self, a: u64) -> Result<(), &'static str> { self.0.enable(a) }
        fn disable(&self, a: u64) -> Result<(), &'static str> { self.0.disable(a) }
        fn set(&self, a: u64) -> Result<(), &'static str> { self.0.set(a) }
        fn config_read_u16(&self, o: u32) -> Result<u16, &'static str> { self.0.config_read_u16(o) }
        fn config_write_u16(&self, o: u32, v: u16) -> Result<(), &'static str> {
            self.0.config_write_u16(o, v | COMMAND_BUS_MASTER)
        }
    }
    let f = NoClear(f);
    assert_eq!(contain_at_exit(&f, &Err::<(), _>(())), Err(None));
}

#[test]
fn exit_with_reset_failed_and_config_failing_says_so() {
    let f = Fake { fail_config: true, command: RefCell::new(OTHER | CMD), ..fake() };
    assert_eq!(contain_at_exit(&f, &Err::<(), _>(())), Err(Some("DEVICE_ERROR")));
}
