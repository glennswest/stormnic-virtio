//! The trace ring behind the quiet console (stormnic-ixgbe#22).
extern crate alloc;
#[path = "../src/trace.rs"]
mod trace;
use trace::{Ring, KEEP};

#[test]
fn keeps_the_last_lines_oldest_first_and_counts_the_dropped() {
    let mut r = Ring::new();
    for i in 0..KEEP + 3 { r.push(format!("step {i}")); }
    let (lines, dropped) = r.take();
    assert_eq!(lines.len(), KEEP);
    assert_eq!(dropped, 3);
    assert_eq!(lines.front().map(String::as_str), Some("step 3"));
    assert_eq!(lines.back().map(|s| s.clone()), Some(format!("step {}", KEEP + 2)));
}

#[test]
fn take_empties_the_ring_so_a_second_failure_replays_only_newer_steps() {
    let mut r = Ring::new();
    r.push("a".into());
    let _ = r.take();
    r.push("b".into());
    let (lines, dropped) = r.take();
    assert_eq!((lines.len(), dropped), (1, 0));
    assert_eq!(lines[0], "b");
}

#[test]
fn clear_forgets_lines_and_the_dropped_count() {
    let mut r = Ring::new();
    for i in 0..KEEP + 1 { r.push(format!("{i}")); }
    r.clear();
    let (lines, dropped) = r.take();
    assert!(lines.is_empty());
    assert_eq!(dropped, 0);
}
