use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use tenuto::lifecycle::hooks::TestHook;
use tenuto::lifecycle::panic::{TakeOnceSlot, TakeResult, in_contained_job, run_contained};
use tenuto::lifecycle::terminal::TerminalCleanup;

#[test]
fn a_contained_panic_becomes_a_failed_job_and_the_flag_resets() {
    assert!(!in_contained_job());
    let failed = run_contained("artwork", || -> u32 { panic!("decoder exploded") });
    assert_eq!(failed.map_err(|e| e.label), Err("artwork"));
    assert!(!in_contained_job(), "flag restored after unwind");
    assert_eq!(
        run_contained("artwork", || 7).ok(),
        Some(7),
        "a later job succeeds"
    );
}

#[test]
fn nested_containment_restores_the_outer_value() {
    let observed = run_contained("outer", || {
        let inner = run_contained("inner", in_contained_job).ok();
        (inner, in_contained_job())
    });
    assert_eq!(observed.ok(), Some((Some(true), true)));
    assert!(!in_contained_job());
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_slot_is_taken_once_and_dropped_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let slot = TakeOnceSlot::new();
    assert!(slot.publish(Counted(drops.clone())).is_ok());
    assert!(matches!(slot.try_take(), TakeResult::Taken(_)));
    assert!(matches!(slot.try_take(), TakeResult::Empty));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn a_busy_slot_is_skipped_without_deadlock() {
    let slot: TakeOnceSlot<u8> = TakeOnceSlot::new();
    assert!(slot.publish(1).is_ok());
    let holder = slot.clone();
    let barrier = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let (b, r) = (barrier.clone(), release.clone());
    let thread = std::thread::spawn(move || {
        holder.hold_for_test(|| {
            b.wait();
            r.wait();
        })
    });
    barrier.wait();
    assert!(matches!(slot.try_take(), TakeResult::Busy));
    release.wait();
    thread.join().expect("holder");
    assert!(matches!(slot.try_take(), TakeResult::Taken(1)));
}

#[test]
fn a_poisoned_slot_still_yields_its_value() {
    let slot: TakeOnceSlot<u8> = TakeOnceSlot::new();
    assert!(slot.publish(9).is_ok());
    let poisoner = slot.clone();
    let _ = std::thread::spawn(move || poisoner.hold_for_test(|| panic!("poison the slot"))).join();
    assert!(matches!(slot.try_take(), TakeResult::Taken(9)));
    assert!(matches!(slot.try_take(), TakeResult::Empty));
}

#[test]
fn terminal_restoration_writes_each_undo_once() {
    let cleanup = TerminalCleanup::default();
    cleanup.mark_alternate();
    cleanup.set_mouse(true);
    cleanup.mark_cursor_hidden();
    let mut first = Vec::new();
    cleanup.restore_into(&mut first);
    let text = String::from_utf8_lossy(&first);
    assert!(
        text.contains("\x1b[?1049l") && text.contains("\x1b[?25h") && text.contains("\x1b[?1000l"),
        "{text:?}"
    );
    let mut second = Vec::new();
    cleanup.restore_into(&mut second);
    assert!(second.is_empty());
}

#[test]
fn hook_names_parse_exactly() {
    assert_eq!(
        TestHook::parse(Some("panic-after-terminal")),
        TestHook::PanicAfterTerminal
    );
    assert_eq!(
        TestHook::parse(Some("artwork-job-panic")),
        TestHook::ArtworkJobPanic
    );
    assert_eq!(
        TestHook::parse(Some("PANIC-AFTER-TERMINAL")),
        TestHook::None
    );
    assert_eq!(TestHook::parse(None), TestHook::None);
}
