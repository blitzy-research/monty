//! Rust-level regressions for the two-argument `iter(callable, sentinel)` form.
//!
//! A callable-driven iterator is the only iterator whose advance re-enters the interpreter, which
//! makes it the only one that can be driven entirely from Rust while allocating, consuming time and
//! reaching a collection point mid-step. The behaviours pinned here are exactly the ones a
//! `test_cases/` fixture cannot observe, because a fixture can neither configure a
//! `ResourceTracker`, nor suspend execution at an external call, nor read the collector's counters:
//!
//! * a Rust-side drive loop must keep honouring a configured duration limit,
//! * each of the two allocations construction performs must clean up when a limit rejects it,
//!   without stranding the callable's, the sentinel's or the pair's reference count,
//! * collection must resume once the step's scoped suspension is released, and
//! * a live, partly consumed - or exhausted - callable-driven iterator must survive a snapshot
//!   round trip, since the new `IterValue` variant is serialized with the rest of the heap.
//!
//! The companion fixtures `test_cases/iter__callable_sentinel.py`,
//! `test_cases/iter__callable_sentinel_gc.py` and `test_cases/refcount__iter_callable_sentinel.py`
//! cover the observable semantics, forced-collection survival and reference-count ownership against
//! CPython; nothing here duplicates them.

use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use monty::{
    ExcType, FunctionCall, LimitedTracker, MontyObject, MontyRun, NameLookupResult, NoLimitTracker, PrintWriter,
    ResourceLimits, RunProgress,
};

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

/// The baseline half of the calibration pair: everything the construction test does except `iter()`.
///
/// It binds the same three names as [`CONSTRUCTING_PROGRAM`], builds the same two heap values - a
/// closure as the callable and a list as the sentinel, so two reference counts are at risk whenever
/// a construction step is rejected - and ends in the same non-allocating expression. Because the two
/// programs differ *only* in the `iter(stepper, sentinel)` call, the difference between their
/// minimum allocation limits is exactly what construction costs, which is what lets a limit be aimed
/// at one construction stage instead of at "somewhere in the program".
const BASELINE_PROGRAM: &str = r"
def make_stepper(seed):
    def stepper():
        return [seed]

    return stepper


stepper = make_stepper(1)
sentinel = []
probe = stepper
len(sentinel)
";

/// The constructing half of the calibration pair - see [`BASELINE_PROGRAM`].
const CONSTRUCTING_PROGRAM: &str = r"
def make_stepper(seed):
    def stepper():
        return [seed]

    return stepper


stepper = make_stepper(1)
sentinel = []
probe = iter(stepper, sentinel)
len(sentinel)
";

/// Returns the smallest allocation limit under which `code` runs to completion.
///
/// Sweeping for the limit rather than hard-coding an allocation count is what keeps the calibrated
/// tests below honest: if the interpreter's allocation pattern ever shifts, the limits move with it
/// instead of silently aiming at the wrong construction stage. Every rejection along the way is
/// asserted to be a clean allocation-limit `MemoryError`, so a program that fails for an unrelated
/// reason cannot be mistaken for a limit that is merely too small.
fn minimum_allocation_limit(code: &str) -> usize {
    /// Comfortably above what either calibration program needs, so exceeding it means a real defect.
    const CEILING: usize = 64;

    for max_allocations in 1..=CEILING {
        let run = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
        let limits = ResourceLimits::new().max_allocations(max_allocations);
        match run.run(vec![], LimitedTracker::new(limits), PrintWriter::Stdout) {
            Ok(_) => return max_allocations,
            Err(exc) => assert_eq!(
                exc.exc_type(),
                ExcType::MemoryError,
                "max_allocations={max_allocations}: expected the allocation limit, got: {exc}"
            ),
        }
    }
    panic!("no allocation limit up to {CEILING} let the program complete: {code}")
}

/// Calibrates the two construction stages and returns the limit that rejects each one.
///
/// The returned pair is `(pair_stage, iterator_stage)`: with `pair_stage` allocations available every
/// allocation before `iter()` succeeds and the `[callable, sentinel]` pair list is the one rejected,
/// while `iterator_stage` admits the pair and rejects the iterator entry instead. Asserting the
/// two-allocation difference here is what proves each limit really lands on the stage its caller
/// claims - the alternative, a bare "some MemoryError was raised", would keep passing even if
/// construction stopped allocating the pair at all.
fn calibrated_construction_stages() -> (usize, usize) {
    let baseline = minimum_allocation_limit(BASELINE_PROGRAM);
    let constructing = minimum_allocation_limit(CONSTRUCTING_PROGRAM);
    assert_eq!(
        constructing,
        baseline + 2,
        "iter(callable, sentinel) must cost exactly two allocations - the pair list and the iterator \
         entry - but the baseline program needed {baseline} and the constructing program {constructing}"
    );
    (baseline, baseline + 1)
}

/// Asserts `code` is rejected by `max_allocations` with a clean allocation-limit `MemoryError`.
///
/// "Clean" is enforced from two directions: the exception type and message are checked here, and
/// under the `ref-count-panic` feature a reference stranded by the rejected allocation aborts in
/// `Value::drop` before this function can return - which is how a leak in either construction stage
/// becomes a test failure rather than a silent count that nothing observes.
fn assert_allocation_limit_rejects(code: &str, max_allocations: usize) {
    let run = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let limits = ResourceLimits::new().max_allocations(max_allocations);
    let exc = run
        .run(vec![], LimitedTracker::new(limits), PrintWriter::Stdout)
        .expect_err("the allocation limit must reject this construction");
    assert_eq!(
        exc.exc_type(),
        ExcType::MemoryError,
        "max_allocations={max_allocations}: expected the allocation limit, got: {exc}"
    );
    assert!(
        exc.message().is_some_and(|m| m.contains("allocation limit exceeded")),
        "max_allocations={max_allocations}: expected an allocation limit message, got: {exc}"
    );
}

/// Construction must cost exactly two allocations, which is what the two tests below aim at.
#[test]
fn callable_sentinel_construction_costs_exactly_two_allocations() {
    let (pair_stage, iterator_stage) = calibrated_construction_stages();
    assert_eq!(
        iterator_stage,
        pair_stage + 1,
        "the iterator entry must be the allocation immediately after the pair list"
    );
    assert_allocation_limit_rejects(CONSTRUCTING_PROGRAM, pair_stage);
    assert_allocation_limit_rejects(CONSTRUCTING_PROGRAM, iterator_stage);
    let run = MontyRun::new(CONSTRUCTING_PROGRAM.to_owned(), "test.py", vec![]).unwrap();
    let limits = ResourceLimits::new().max_allocations(iterator_stage + 1);
    let output = run
        .run(vec![], LimitedTracker::new(limits), PrintWriter::Stdout)
        .expect("one more allocation than the iterator entry needs must let construction succeed");
    assert_eq!(
        output.to_string(),
        "0",
        "the program ends in len(sentinel), an empty list"
    );
}

/// A rejected pair-list allocation must release the callable and the sentinel, not strand them.
///
/// This is the earlier of the two fallible construction steps. `Heap::allocate` enforces the limits
/// *before* inserting an entry and takes its `HeapData` by value, so a construction that handed
/// `[callable, sentinel]` over up front would destroy both owned references with ordinary Rust
/// destruction - which has no heap access - the moment a limit rejected the pair. Both values here
/// are heap values, so two counts are at risk.
#[test]
fn callable_sentinel_pair_allocation_rejection_is_clean() {
    let (pair_stage, _) = calibrated_construction_stages();
    assert_allocation_limit_rejects(CONSTRUCTING_PROGRAM, pair_stage);
}

/// A rejected iterator-entry allocation must release the pair, and through it both of its elements.
///
/// The later of the two fallible construction steps: the pair list now exists and is owned only by
/// the local that is about to move into the iterator, so a rejection here has to release the whole
/// pair sub-graph rather than the two values individually.
#[test]
fn callable_sentinel_iterator_allocation_rejection_is_clean() {
    let (_, iterator_stage) = calibrated_construction_stages();
    assert_allocation_limit_rejects(CONSTRUCTING_PROGRAM, iterator_stage);
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

// === Collector scheduling either side of a step ===

/// Collection must resume, and the work it deferred must actually run, once a step's scoped
/// suspension is released.
///
/// The callable-driven step suspends tracing collection while it re-enters the interpreter, and a
/// callable that itself drives a second callable-driven iterator nests that suspension, so the depth
/// has to come back to zero exactly. A leaked depth is invisible to every other test here: the run
/// still produces the right answer, it simply never collects again - which under a memory-limited
/// tracker surfaces much later, and somewhere else entirely, as a spurious allocation failure.
/// `RefCountOutput` is the only window onto the collector's own scheduling state from outside the
/// crate, which is why this lives in Rust rather than in a fixture.
///
/// The program makes a collection due at the deepest point of a nested drive: the inner callable
/// allocates far past the collector's threshold while two iterators, both hidden callable/sentinel
/// pairs and both accumulators are live. `Heap::should_gc` stays true across the suspension - only
/// `Heap::collect_garbage` clears `allocations_since_gc` - so the collection is serviced at the first
/// instruction boundary reached with no suspension active. The short tail loop after the drive is
/// what makes the counter readable: with the depth balanced the counter lands in the double digits,
/// accounting for the tail's own allocations alone, whereas a leaked depth strands the entire
/// six-figure churn - both outstanding on the counter and still live on the heap. The two bounds are
/// therefore three orders of magnitude away from the failing value, which keeps them robust while
/// staying decisive.
#[cfg(feature = "ref-count-return")]
#[test]
fn collection_resumes_after_a_paused_callable_sentinel_step() {
    let code = r"
state = {'outer': 0, 'inner': 0}


def churn():
    total = 0
    for _ in range(12000):
        pair = [[1], [2], [3], [4], [5], [6], [7], [8], [9], [10]]
        total += len(pair)
    return total


def inner_step():
    state['inner'] += 1
    if state['inner'] == 2 and state['outer'] == 0:
        assert churn() == 120000, 'the churn helper must allocate on every iteration'
    return ['i', state['inner']] if state['inner'] <= 2 else ['STOP']


def outer_step():
    inner = []
    state['inner'] = 0
    for item in iter(inner_step, ['STOP']):
        inner.append(item)
    state['outer'] += 1
    return inner if state['outer'] <= 2 else []


collected = []
for item in iter(outer_step, []):
    collected.append(item)

tail = 0
for _ in range(20):
    tail += len([[1], [2]])

[len(collected), state['outer'], state['inner'], tail]
";
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let output = runner
        .run_ref_counts(vec![])
        .expect("the nested drive must run to completion");

    assert_eq!(
        output.py_object,
        MontyObject::List(vec![
            MontyObject::Int(2),
            MontyObject::Int(3),
            MontyObject::Int(3),
            MontyObject::Int(40),
        ]),
        "the nested drive must yield two outer values, each drained from three inner calls"
    );
    assert!(
        output.allocations_since_gc < 1_000,
        "the collection the step deferred must have run once the suspension was released, clearing the \
         collector's counter; {} allocations are still outstanding, so collection never resumed",
        output.allocations_since_gc
    );
    assert!(
        output.heap_count < 200,
        "the resumed collection must have freed the churn; {} heap entries are still live",
        output.heap_count
    );
}

// === Snapshot round trips ===

/// Resolves the `NameLookup` yields an external name produces before it can be called.
///
/// Names are resolved lazily as execution reaches them, so a run that is meant to suspend at an
/// external *call* has to answer the lookup for that name first. Mirrors the helper in
/// `binary_serde.rs`; kept local because every integration test is its own crate.
fn resolve_name_lookups(mut progress: RunProgress<NoLimitTracker>) -> RunProgress<NoLimitTracker> {
    while let RunProgress::NameLookup(lookup) = progress {
        let name = lookup.name.clone();
        progress = lookup
            .resume(
                NameLookupResult::Value(MontyObject::Function { name, docstring: None }),
                PrintWriter::Stdout,
            )
            .expect("resolving an external name must not fail");
    }
    progress
}

/// What a snapshot round trip observed.
struct RoundTrip {
    /// The name of the external function the run suspended at.
    function_name: String,
    /// The arguments seen at that call, which record what the iterator had produced by then.
    args: Vec<MontyObject>,
    /// The final value produced by resuming the live snapshot.
    live: MontyObject,
    /// The final value produced by resuming a `dump`/`load` copy of that same snapshot.
    reloaded: MontyObject,
}

/// Runs `code` until it suspends at its external call, then resumes both the live snapshot and a
/// `dump`/`load` copy of it with `result`.
///
/// Suspending at an external call is the only way to force the new `IterValue` variant, and the
/// hidden pair it points at, through postcard: a snapshot carries the whole heap, so an iterator
/// sitting in globals has to survive encoding and decoding before execution can continue. It is also
/// why the variant is appended to `IterValue` rather than inserted - postcard encodes enum
/// discriminants positionally, so an insertion would silently reinterpret every older snapshot.
///
/// Resuming the live snapshot as well as the reloaded one gives the round trip a same-run control:
/// both must reach the same value, so any difference is attributable to serialization alone. It also
/// disposes of the live snapshot the only way a snapshot can be disposed of, by running it to
/// completion. Abandoning an *unresumed* snapshot strands the reference count of every
/// heap-referencing global it holds, because `Snapshot` owns `Value`s and has no `Drop` that drains
/// them; that is pre-existing and has nothing to do with iterators - `state = {'n': 1}` followed by
/// `ext_fn(state['n'])` is enough to reproduce it - but it does abort under
/// `--features ref-count-panic`, so a test must not leave one behind.
fn round_trip_at_external_call(code: &str, result: MontyObject) -> RoundTrip {
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let progress = resolve_name_lookups(
        runner
            .start(vec![], NoLimitTracker, PrintWriter::Stdout)
            .expect("the run must reach its external call"),
    );
    let bytes = progress
        .dump()
        .expect("a paused run holding a callable-driven iterator must serialize");
    let reloaded: RunProgress<NoLimitTracker> =
        RunProgress::load(&bytes).expect("a snapshot holding a callable-driven iterator must deserialize");

    let live_call: FunctionCall<NoLimitTracker> = progress
        .into_function_call()
        .expect("the run must be paused at the external call");
    let function_name = live_call.function_name.clone();
    let args = live_call.args.clone();

    RoundTrip {
        function_name,
        args,
        live: resume_to_completion(live_call, result.clone(), "live"),
        reloaded: resume_to_completion(
            reloaded
                .into_function_call()
                .expect("the reloaded run must still be paused at the external call"),
            result,
            "reloaded",
        ),
    }
}

/// Resumes `call` with `result` and returns the value the run completes with.
///
/// `label` names which of the two snapshots is being resumed, so a failure says whether the live run
/// or the reloaded one broke.
fn resume_to_completion(call: FunctionCall<NoLimitTracker>, result: MontyObject, label: &str) -> MontyObject {
    call.resume(result, PrintWriter::Stdout)
        .unwrap_or_else(|error| panic!("the {label} snapshot must resume: {error}"))
        .into_complete()
        .unwrap_or_else(|| panic!("the {label} snapshot must run to completion"))
}

/// A partly consumed callable-driven iterator must keep iterating after a snapshot round trip.
///
/// The iterator's step state lives in three places at once: `index` and `done` inside the `MontyIter`
/// entry, the callable and sentinel inside the hidden pair the entry's single `value` edge points at,
/// and the callable's own captured state. All three have to come back from the snapshot, and they
/// have to come back consistent with each other - a restored iterator that re-ran its callable from
/// the beginning, or lost the sentinel it stops on, would still deserialize cleanly.
#[test]
fn a_partly_consumed_callable_sentinel_iterator_survives_a_snapshot() {
    let round_trip = round_trip_at_external_call(
        r"
def make_stepper():
    box = {'n': 0}

    def stepper():
        box['n'] = box['n'] + 1
        return box['n'] if box['n'] < 4 else 0

    return stepper


it = iter(make_stepper(), 0)
first = next(it)
bridge = ext_fn(first)
rest = []
while True:
    try:
        rest.append(next(it))
    except StopIteration:
        break

[first, bridge, rest, next(it, 'DEFAULT')]
",
        MontyObject::Int(100),
    );

    assert_eq!(
        round_trip.function_name, "ext_fn",
        "the run must suspend at the external call"
    );
    assert_eq!(
        round_trip.args,
        vec![MontyObject::Int(1)],
        "exactly one value was drawn from the iterator before the snapshot"
    );

    let expected = MontyObject::List(vec![
        MontyObject::Int(1),
        MontyObject::Int(100),
        MontyObject::List(vec![MontyObject::Int(2), MontyObject::Int(3)]),
        MontyObject::String("DEFAULT".to_owned()),
    ]);
    assert_eq!(
        round_trip.reloaded, expected,
        "the restored iterator resumes its restored callable from where it left off, then stops on its sentinel"
    );
    assert_eq!(
        round_trip.live, expected,
        "the live snapshot reaches the same value, so the round trip changed nothing"
    );
}

/// An exhausted callable-driven iterator must stay exhausted across a snapshot round trip.
///
/// Exhaustion is sticky and is recorded as the `done` flag on the new variant, so the round trip has
/// to preserve it: the callable must not be invoked again after the snapshot. The call counter the
/// program returns is what makes that observable - it is read once before the snapshot, as the
/// external call's argument, and again afterwards, so a `done` flag lost in serialization shows up as
/// an extra invocation rather than as a wrong final value.
#[test]
fn an_exhausted_callable_sentinel_iterator_stays_exhausted_across_a_snapshot() {
    let round_trip = round_trip_at_external_call(
        r"
state = {'calls': 0}


def stepper():
    state['calls'] = state['calls'] + 1
    return 0


it = iter(stepper, 0)
stopped = False
try:
    next(it)
except StopIteration:
    stopped = True

bridge = ext_fn(state['calls'])
[stopped, bridge, next(it, 'DEFAULT'), state['calls']]
",
        MontyObject::Int(7),
    );

    assert_eq!(
        round_trip.args,
        vec![MontyObject::Int(1)],
        "the terminal sentinel probe is the only call made before the snapshot"
    );

    let expected = MontyObject::List(vec![
        MontyObject::Bool(true),
        MontyObject::Int(7),
        MontyObject::String("DEFAULT".to_owned()),
        MontyObject::Int(1),
    ]);
    assert_eq!(
        round_trip.reloaded, expected,
        "the restored iterator is still exhausted: it hands back the default without calling back into the callable"
    );
    assert_eq!(
        round_trip.live, expected,
        "the live snapshot agrees, so the trailing call count is the `done` flag surviving rather than a coincidence"
    );
}
