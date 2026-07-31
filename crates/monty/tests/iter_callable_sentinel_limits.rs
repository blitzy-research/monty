//! Resource-limit regressions for the two-argument `iter(callable, sentinel)` form.
//!
//! A callable-driven iterator is the only iterator whose advance re-enters the interpreter, which
//! makes it the only one that can be driven entirely from Rust while allocating and consuming time.
//! The two behaviours pinned here are the ones that cannot be observed from a `test_cases/` fixture,
//! because a fixture has no way to configure a `ResourceTracker`:
//!
//! * a Rust-side drive loop must keep honouring a configured duration limit, and
//! * a rejected allocation during construction must surface as a clean `MemoryError` without
//!   stranding the callable's or the sentinel's reference count.

use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use monty::{ExcType, LimitedTracker, MontyRun, PrintWriter, ResourceLimits};

/// Runs `code` under a duration limit on a worker thread and asserts it stops with `TimeoutError`.
///
/// The limit is enforced by the interpreter; `deadline` is a wall-clock backstop so that a
/// regression fails the test instead of hanging the suite forever. On a breach the worker thread is
/// deliberately left running - it is an infinite loop that only the process exit can stop, and
/// abandoning it is what lets the assertion be reported.
fn assert_stops_on_time_limit(code: &str, limit: Duration, deadline: Duration) {
    let owned = code.to_owned();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let run = MontyRun::new(owned, "test.py", vec![]).unwrap();
        let limits = ResourceLimits::new().max_duration(limit);
        let outcome = match run.run(vec![], LimitedTracker::new(limits), PrintWriter::Stdout) {
            Ok(_) => Err("the program completed instead of hitting the time limit".to_owned()),
            Err(exc) => {
                if exc.exc_type() == ExcType::TimeoutError
                    && exc.message().is_some_and(|m| m.contains("time limit exceeded"))
                {
                    Ok(())
                } else {
                    Err(format!("expected a time limit error, got: {exc}"))
                }
            }
        };
        drop(sender.send(outcome));
    });

    let Ok(outcome) = receiver.recv_timeout(deadline) else {
        panic!(
            "the interpreter did not return within {deadline:?}: the callable-driven drive loop is ignoring the \
             configured time limit"
        )
    };
    worker.join().expect("the worker thread panicked");
    outcome.expect("the time limit must be reported as a TimeoutError");
}

/// A dict-view operator drives a callable-sentinel iterator from Rust and must stay interruptible.
///
/// `dict_view::collect_iterable_to_set` advances the iterator in a pure-Rust `while let` loop, and
/// `int` is a *direct* callable - calling it pushes no frame and never enters `VM::run` - so the
/// VM's per-instruction time check is reached exactly zero times for the whole drive. `int()` is `0`
/// and never equals the sentinel `1`, so without a check inside the step this union never returns.
#[test]
fn dict_view_union_drive_honours_the_time_limit() {
    let code = r"
d = {}
result = d.keys() | iter(int, 1)
result
";
    assert_stops_on_time_limit(code, Duration::from_millis(100), Duration::from_secs(30));
}

/// `isdisjoint` reaches the same Rust drive loop from a different caller, so it is pinned too.
#[test]
fn dict_view_isdisjoint_drive_honours_the_time_limit() {
    let code = r"
d = {}
result = d.keys().isdisjoint(iter(int, 1))
result
";
    assert_stops_on_time_limit(code, Duration::from_millis(100), Duration::from_secs(30));
}

/// A `for` loop over a callable-sentinel iterator stays interruptible as well.
///
/// `Opcode::ForIter` does reach the VM's per-instruction check between steps, so this is a
/// belt-and-braces companion to the two dict-view cases rather than the load-bearing one; it also
/// covers the case where the callable body itself is a no-op.
#[test]
fn for_loop_drive_honours_the_time_limit() {
    let code = r"
total = 0
for value in iter(int, 1):
    total = total + 1
total
";
    assert_stops_on_time_limit(code, Duration::from_millis(100), Duration::from_secs(30));
}

/// Every allocation limit must either be honoured cleanly or not trip at all - never leak or panic.
///
/// Constructing `iter(callable, sentinel)` allocates the `[callable, sentinel]` pair and then the
/// iterator entry, and `Heap::allocate` enforces the limits *before* inserting an entry while taking
/// its `HeapData` by value. Any construction step that handed an owned `Value::Ref` to `allocate`
/// would therefore strand that reference when the limit rejects it. Both the callable (a closure)
/// and the sentinel (a list) are heap values here, so two counts are at risk on every rejected step.
///
/// Sweeping the limit walks the rejection through every allocation the program performs, which is
/// what makes the test independent of the exact allocation count. Under `ref-count-panic` a stranded
/// reference aborts in `Value::drop`, so this is where the leak becomes observable; under the other
/// configurations the sweep still guards against a rejection turning into a panic or the wrong
/// exception type.
#[test]
fn callable_sentinel_construction_survives_every_allocation_limit() {
    let code = r"
def make_stepper(seed):
    def stepper():
        return [seed]

    return stepper


stepper = make_stepper(1)
sentinel = []
it = iter(stepper, sentinel)
result = next(it)
result
";

    let mut rejected = 0_usize;
    let mut accepted = 0_usize;
    for max_allocations in 1..=24_usize {
        let run = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
        let limits = ResourceLimits::new().max_allocations(max_allocations);
        match run.run(vec![], LimitedTracker::new(limits), PrintWriter::Stdout) {
            Ok(_) => accepted += 1,
            Err(exc) => {
                assert_eq!(
                    exc.exc_type(),
                    ExcType::MemoryError,
                    "max_allocations={max_allocations}: expected the allocation limit, got: {exc}"
                );
                assert!(
                    exc.message().is_some_and(|m| m.contains("allocation limit exceeded")),
                    "max_allocations={max_allocations}: expected an allocation limit message, got: {exc}"
                );
                rejected += 1;
            }
        }
    }

    // Both outcomes have to occur, otherwise the sweep silently stops exercising the rejection paths
    // (or stops exercising a successful construction) if the allocation count ever shifts.
    assert!(rejected > 0, "the sweep must reject at least one allocation limit");
    assert!(accepted > 0, "the sweep must accept at least one allocation limit");
}

/// A rejected allocation must not stop the *next* run from working, and must stay quick.
///
/// Guards against a rejection leaving global state - interner or heap metadata - in a shape that
/// breaks a subsequent construction, and keeps the sweep above honest by proving the same program
/// succeeds with a generous limit.
#[test]
fn callable_sentinel_construction_recovers_after_a_rejection() {
    let code = r"
def make_stepper(seed):
    def stepper():
        return [seed]

    return stepper


stepper = make_stepper(2)
sentinel = []
values = []
for value in iter(stepper, sentinel):
    values.append(value)
    if len(values) == 3:
        break

len(values)
";
    let tight = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let rejected = tight.run(
        vec![],
        LimitedTracker::new(ResourceLimits::new().max_allocations(3)),
        PrintWriter::Stdout,
    );
    assert!(rejected.is_err(), "three allocations cannot build this program");

    let started = Instant::now();
    let generous = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let output = generous
        .run(
            vec![],
            LimitedTracker::new(
                ResourceLimits::new()
                    .max_allocations(100_000)
                    .max_duration(Duration::from_secs(30)),
            ),
            PrintWriter::Stdout,
        )
        .expect("a generous limit must let the same program run");
    assert_eq!(
        output.to_string(),
        "3",
        "the loop must yield three values before breaking"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the run must finish well inside its own duration limit"
    );
}
