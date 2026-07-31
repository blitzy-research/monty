//! Iterator support for Python for loops and the `iter()` type constructor.
//!
//! This module provides the `MontyIter` struct which encapsulates iteration state
//! for different iterable types. It uses index-based iteration internally to avoid
//! borrow conflicts when accessing the heap during iteration.
//!
//! The design stores iteration state (indices) rather than Rust iterators, allowing
//! `for_next()` to take `&mut VM` for cloning values and allocating strings.
//!
//! For constructors like `list()` and `tuple()`, use `MontyIter::new()` followed
//! by `collect()` to materialize all items into a Vec.
//!
//! ## Builtin Support
//!
//! The `iterator_next()` helper implements the `next()` builtin.
//!
//! `MontyIter::init()` implements both forms of the `iter()` builtin. The two-argument form
//! `iter(callable, sentinel)` produces a *callable-driven* iterator, which drives a callable
//! instead of walking a container and is consequently the only iterator that re-enters the VM
//! while advancing; see [`MontyIter::init`] for its semantics and `callable_sentinel_step` for
//! the constraints that re-entry imposes.

use std::mem;

use crate::{
    args::ArgValues,
    bytecode::VM,
    exception_private::{ExcType, RunResult},
    heap::{ContainsHeap, DropWithHeap, Heap, HeapData, HeapGuard, HeapId, HeapItem, HeapRead, HeapReadOutput},
    intern::{BytesId, Interns},
    resource::ResourceTracker,
    types::{PyTrait, Range, dict_view::DictView, str::allocate_char},
    value::Value,
};

/// Iterator state for Python for loops.
///
/// Contains the current iteration index and the type-specific iteration data.
/// Uses index-based iteration to avoid borrow conflicts when accessing the heap.
///
/// For strings, stores the string content with a byte offset for O(1) UTF-8 iteration.
///
/// # Foot-gun: `value` is not always the iterated object
///
/// `value` exists so the garbage collector can reach everything the iterator keeps alive -
/// [`MontyIter::value`] is the single edge `collect_child_ids` follows for `HeapData::Iter` - so it
/// holds whatever the variant needs traced rather than the iterable: `Value::None` for the variants
/// that copy their state out (`Range`, `IterStr`, released in [`MontyIter::new`]), the iterated
/// container for `HeapRef`, and the `(callable, sentinel)` pair tuple for `CallableSentinel`, whose
/// ownership model is documented on that variant.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct MontyIter {
    /// Current iteration index, shared across all iterator types.
    index: usize,
    /// Type-specific iteration data.
    iter_value: IterValue,
    /// The owned value this iterator keeps alive, retained for garbage-collection traversal and
    /// reference counting rather than for iteration itself - it is **not** necessarily the object
    /// being iterated. See the foot-gun on the struct above.
    value: Value,
}

impl MontyIter {
    /// Creates an iterator from the `iter()` constructor call.
    ///
    /// - `iter(iterable)` - Returns an iterator for the iterable. If the argument is
    ///   already an iterator, returns the same object.
    /// - `iter(callable, sentinel)` - Returns a callable-driven iterator. On every step it
    ///   invokes `callable` with **no** arguments; if the produced value compares equal to
    ///   `sentinel` iteration stops with `StopIteration` and that value is **not** yielded,
    ///   otherwise the value is yielded. The stop test is `==` rich value comparison
    ///   ([`Value::py_eq`]), never identity, so `3.0` stops against an integer sentinel `3`
    ///   and a `[]` sentinel stops against a freshly built empty list. The result is
    ///   self-iterable, and every exception raised by `callable` propagates unchanged in type
    ///   and message, because sentinel equality is the sole stop condition; see
    ///   `callable_sentinel_step`.
    ///
    /// `iter` accepts exactly one or two **positional-only** arguments; the argument-count errors
    /// are produced by `ArgValues::get_one_two_args` and already match CPython byte for byte.
    ///
    /// # Foot-guns
    ///
    /// - `callable` is **never** invoked here. CPython calls it zero times at construction,
    ///   and the VM's `CallBuiltinType` dispatch arm relies on this constructor being unable
    ///   to push a frame ("IP sync deferred to error path"). Do not pre-fetch a first value.
    /// - Callability is validated **eagerly**, matching CPython's `iter(v, w): v must be
    ///   callable`, so a bad first argument fails at construction rather than on first use.
    /// - The two-argument form owns **two** values while `collect_child_ids` follows only one edge
    ///   out of an iterator, so both are moved into a two-element `(callable, sentinel)` tuple whose
    ///   single owning reference becomes `value`; `iter_value` keeps a non-owning mirror of that id.
    ///   `IterValue::CallableSentinel` documents why that indirection is what keeps mark-and-sweep
    ///   sound with no change to `heap.rs`, and `callable_sentinel_pair` reads the pair back.
    /// - Either allocation below can be rejected by a resource limit, and `Heap::allocate` checks
    ///   the allocation-count and memory limits *before* inserting while taking its `HeapData` by
    ///   value, so a rejection destroys that data through ordinary Rust destruction, which has no
    ///   heap access and therefore releases nothing. Both stages are written around that:
    ///   - The iterator entry is allocated holding **no** heap reference, and the pair moves in only
    ///     once the entry exists, so a rejected entry destroys nothing that needs the heap and the
    ///     still-owned pair is released heap-aware - freeing the callable and the sentinel with it.
    ///   - The pair allocation cannot be arranged that way, because a tuple is immutable and can
    ///     only be built with its items already in it, so the two heap ids are copied out first and
    ///     a rejection releases exactly the counts the destroyed tuple took. In the
    ///     `ref-count-panic` diagnostic build that recovery is unreachable: the rejected values trip
    ///     `Value`'s panicking destructor before `allocate_tuple` returns. That is a property of the
    ///     by-value rejection contract in `heap.rs` - a shared safety boundary this change may not
    ///     touch - and the one-argument form below reaches it the same way, so it is recorded here
    ///     rather than worked around.
    pub fn init(vm: &mut VM<'_, '_, impl ResourceTracker>, args: ArgValues) -> RunResult<Value> {
        let (iterable, sentinel) = args.get_one_two_args("iter", vm.heap)?;

        if let Some(s) = sentinel {
            // CPython validates callability before the iterator even exists, so a non-callable
            // first argument must fail here rather than on the first `next()`.
            if !Self::is_callable(&iterable, vm) {
                // Single linear path with no branching, so direct drops are appropriate here.
                iterable.drop_with_heap(vm);
                s.drop_with_heap(vm);
                return Err(ExcType::type_error("iter(v, w): v must be callable"));
            }

            // Both values move into the pair with no clone, so neither may be dropped from here on;
            // the pair's single reference is the one edge `collect_child_ids` follows out of an
            // iterator. See `IterValue::CallableSentinel`. Their heap ids are copied out first
            // because a rejected allocation is the one path where the move is not the end of the
            // story: it destroys the tuple - and with it the ownership it had just taken - without
            // touching the heap, and an id is all that is needed to release the count that was lost.
            let (callable_id, sentinel_id) = (iterable.ref_id(), s.ref_id());
            let pair_value = match super::allocate_tuple(smallvec::smallvec![iterable, s], vm.heap) {
                Ok(pair_value) => pair_value,
                Err(err) => {
                    // Release exactly the two counts the rejected tuple owned. Nothing else can be
                    // outstanding here: `Value::drop_with_heap` only ever acts on `Value::Ref`, so
                    // the ids captured above cover every argument that held a reference at all.
                    for id in [callable_id, sentinel_id].into_iter().flatten() {
                        Value::Ref(id).drop_with_heap(vm);
                    }
                    return Err(err.into());
                }
            };
            let Some(pair) = pair_value.ref_id() else {
                panic!("iter(callable, sentinel): a two-element tuple must allocate as a heap reference")
            };

            // Built directly rather than through `MontyIter::new` so that `IterValue::from_heap_data`
            // - and with it every other `MontyIter::new` call site - need not learn about this variant.
            // `value` starts as `Value::None` rather than the pair so that this fallible allocation
            // owns no heap reference: a rejection then destroys an iterator that has nothing to
            // release, leaving the pair owned by `pair_value` and releasable heap-aware below.
            // `iter_value` already carries the non-owning mirror of the pair's id.
            let iter = Self {
                index: 0,
                iter_value: IterValue::CallableSentinel { pair, done: false },
                value: Value::None,
            };
            let id = match vm.heap.allocate(HeapData::Iter(iter)) {
                Ok(id) => id,
                Err(err) => {
                    // Dropping the pair frees the tuple, which cascades to the callable and the
                    // sentinel, so the rejected construction leaves nothing behind.
                    pair_value.drop_with_heap(vm);
                    return Err(err.into());
                }
            };

            // Hand the pair's single owning reference to the entry now that the entry exists. This
            // step cannot fail and cannot re-enter the interpreter, so no collection point separates
            // it from the allocation above and nothing - the collector included - can observe the
            // iterator while `value` is still `Value::None`.
            let HeapReadOutput::Iter(mut entry) = vm.heap.read(id) else {
                panic!("iter(callable, sentinel): the freshly allocated entry must read back as an iterator")
            };
            entry.get_mut(vm.heap).value = pair_value;
            drop(entry);
            // `Heap::allocate` derives its cycle hint from `HeapData::has_refs`, which was false
            // while `value` was `Value::None`; record what allocating the finished iterator would
            // have recorded, so cycle detection sees exactly what it saw before.
            vm.heap.mark_potential_cycle();
            return Ok(Value::Ref(id));
        }

        // Check if already an iterator - return self
        if let Value::Ref(id) = &iterable
            && matches!(vm.heap.get(*id), HeapData::Iter(_))
        {
            // Already an iterator - return it (refcount already correct from caller)
            return Ok(iterable);
        }

        // Create new iterator
        let iter = Self::new(iterable, vm)?;
        let id = vm.heap.allocate(HeapData::Iter(iter))?;
        Ok(Value::Ref(id))
    }

    /// Returns whether `value` can be invoked, for the eager check in `iter(callable, sentinel)`.
    ///
    /// # Foot-gun: this must stay in step with `VM::call_function`
    ///
    /// `VM::call_function` (and `VM::call_heap_callable`, which it delegates to for heap values)
    /// is the **single source of truth** for what monty can call. This predicate mirrors the
    /// exact set those two accept and exists only so the two-argument `iter()` form can reject a
    /// bad first argument at construction time, the way CPython does. If a new callable kind is
    /// ever added there, it must be added here too, otherwise `iter()` will reject something the
    /// interpreter is perfectly able to call.
    ///
    /// Answering `true` guarantees dispatch succeeds, not that the call will complete: external
    /// functions are callable yet still fail inside `VM::evaluate_function`, an inherited
    /// limitation shared with `map`, `filter` and `sorted`.
    fn is_callable(value: &Value, vm: &VM<'_, '_, impl ResourceTracker>) -> bool {
        match value {
            // Immediates dispatched directly by `VM::call_function`. `Value::Builtin` covers all
            // three callable builtin kinds (functions, exception types and types), so `len`,
            // `ValueError` and `int` are all legitimate callables.
            Value::Builtin(_) | Value::ModuleFunction(_) | Value::DefFunction(_) | Value::ExtFunction(_) => true,
            // Heap values routed to `VM::call_heap_callable`, which accepts exactly these three.
            Value::Ref(heap_id) => matches!(
                vm.heap.get(*heap_id),
                HeapData::Closure(_) | HeapData::FunctionDefaults(_) | HeapData::ExtFunction(_)
            ),
            _ => false,
        }
    }

    /// Creates a new MontyIter from a Value.
    ///
    /// Returns an error if the value is not iterable.
    /// For strings, copies the string content for byte-offset based iteration.
    /// For ranges, the data is copied so the heap reference is dropped immediately.
    pub fn new(mut value: Value, vm: &mut VM<'_, '_, impl ResourceTracker>) -> RunResult<Self> {
        if let Some(iter_value) = IterValue::new(&value, vm) {
            // For Range, we copy next/step/len into IterValue::Range, so we don't need
            // to keep the heap object alive during iteration. Drop it immediately to avoid
            // GC issues (the Range isn't in any namespace slot, so GC wouldn't see it).
            // Same for IterStr which copies the string content.
            if matches!(iter_value, IterValue::Range { .. } | IterValue::IterStr { .. }) {
                value.drop_with_heap(vm);
                value = Value::None;
            }
            Ok(Self {
                index: 0,
                iter_value,
                value,
            })
        } else {
            let err = ExcType::type_error_not_iterable(value.py_type(vm));
            value.drop_with_heap(vm);
            Err(err)
        }
    }

    /// Drops the iterator and its held value properly.
    pub fn drop_with_heap(self, heap: &mut impl ContainsHeap) {
        self.value.drop_with_heap(heap);
    }

    /// Collects HeapIds from this iterator for reference counting cleanup.
    pub fn py_dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        self.value.py_dec_ref_ids(stack);
    }

    /// Returns whether this iterator holds a heap reference (`Value::Ref`).
    ///
    /// Used during allocation to determine if this container could create cycles.
    #[inline]
    #[must_use]
    pub fn has_refs(&self) -> bool {
        matches!(self.value, Value::Ref(_))
    }

    /// Returns the owned value this iterator keeps alive, for garbage-collection traversal.
    ///
    /// `collect_child_ids` is the only caller, and this is the single edge it follows for
    /// `HeapData::Iter`, so what comes back is whatever the variant needs traced rather than the
    /// iterated object - see the foot-gun on [`MontyIter`] for the per-variant breakdown.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Returns the next item from the iterator, advancing the internal index.
    ///
    /// Returns `Ok(None)` when the iterator is exhausted.
    /// Returns `Err` if allocation fails (for string character iteration), if a dict/set changes
    /// size during iteration (RuntimeError), or - for a callable-driven iterator - if the callable
    /// or the sentinel comparison raises.
    pub fn for_next(&mut self, vm: &mut VM<'_, '_, impl ResourceTracker>) -> RunResult<Option<Value>> {
        // Check timeout on every iteration step. For NoLimitTracker this is
        // inlined as a no-op. For LimitedTracker it ensures that Rust-side loops
        // (sum, sorted, min, max, etc.) cannot bypass the VM's per-instruction
        // timeout check by running entirely within a single bytecode instruction.
        vm.heap.check_time()?;
        match &mut self.iter_value {
            IterValue::Range { next, step, len } => {
                if self.index >= *len {
                    return Ok(None);
                }
                let value = *next;
                *next += *step;
                self.index += 1;
                Ok(Some(Value::Int(value)))
            }
            IterValue::IterStr {
                string,
                byte_offset,
                len,
            } => {
                if self.index >= *len {
                    Ok(None)
                } else {
                    // Get next char at current byte offset
                    let c = string[*byte_offset..]
                        .chars()
                        .next()
                        .expect("index < len implies char exists");
                    *byte_offset += c.len_utf8();
                    self.index += 1;
                    Ok(Some(allocate_char(c, vm.heap)?))
                }
            }
            IterValue::InternBytes { bytes_id, len } => {
                if self.index >= *len {
                    return Ok(None);
                }
                let i = self.index;
                self.index += 1;
                let bytes = vm.interns.get_bytes(*bytes_id);
                Ok(Some(Value::Int(i64::from(bytes[i]))))
            }
            IterValue::HeapRef {
                heap_id,
                len,
                checks_mutation,
            } => {
                // Check exhaustion for types with captured len
                if let Some(l) = len
                    && self.index >= *l
                {
                    return Ok(None);
                }
                let i = self.index;
                let expected_len = if *checks_mutation { *len } else { None };
                let item = get_heap_item(vm, *heap_id, i, expected_len)?;
                // Check for list exhaustion (list can shrink during iteration)
                let Some(item) = item else {
                    return Ok(None);
                };
                self.index += 1;
                Ok(Some(item))
            }
            IterValue::CallableSentinel { pair, done } => {
                // A callable-sentinel iterator lives in a `HeapData::Iter` entry and Python drives it
                // through `HeapRead::advance`, so this arm exists for parity with that path. `self`
                // and `vm` are disjoint parameters here, so the nested call needs no borrow window.
                // That exclusive `&mut self` is also why this arm needs no re-read of `done` after the
                // step, unlike `advance_callable_sentinel`: a bare `MontyIter` is unreachable from
                // Python - `MontyIter::init` is this variant's only producer and allocates straight
                // into the heap, and `IterValue::from_heap_data` never builds it - so no nested call
                // can advance *this* iterator behind the step's back.
                if *done {
                    return Ok(None);
                }
                if let Some(value) = callable_sentinel_step(vm, *pair)? {
                    // `index` is a disjoint field, so it can be bumped while `iter_value` is borrowed.
                    self.index += 1;
                    Ok(Some(value))
                } else {
                    // Exhaustion is sticky, and is recorded ONLY for sentinel equality - the single
                    // condition the step reports as `Ok(None)`. Every exception propagates via `?`
                    // and never reaches here, leaving the iterator live for the next call.
                    *done = true;
                    Ok(None)
                }
            }
        }
    }

    /// Returns the remaining size for iterables based on current state.
    ///
    /// For immutable types (Range, Tuple, Str, Bytes, FrozenSet), returns the exact remaining count.
    /// For List, returns current length minus index (may change if list is mutated).
    /// For Dict and Set, returns the captured length minus index (used for size-change detection).
    /// For callable-driven iterators the remaining count is unknowable, so this reports 0, which is
    /// also what CPython's `operator.length_hint(iter(lambda: 1, 0))` returns.
    pub fn size_hint(&self, heap: &Heap<impl ResourceTracker>) -> usize {
        let len = match &self.iter_value {
            IterValue::Range { len, .. } | IterValue::IterStr { len, .. } | IterValue::InternBytes { len, .. } => *len,
            IterValue::HeapRef { heap_id, len, .. } => {
                // For List (len=None), check current length dynamically
                len.unwrap_or_else(|| {
                    let HeapData::List(list) = heap.get(*heap_id) else {
                        panic!("HeapRef with len=None should only be List")
                    };
                    list.len()
                })
            }
            // Zero because the remaining number of steps cannot be known without invoking the
            // callable, which a size hint must not do. Reached only through a bare `MontyIter`, since
            // Python drives callable-sentinel iterators through `HeapRead::advance`.
            IterValue::CallableSentinel { .. } => 0,
        };
        len.saturating_sub(self.index)
    }

    /// Collects all remaining items from the iterator into a Vec.
    ///
    /// Consumes the iterator and returns all items. Used by `list()`, `tuple()`,
    /// and similar constructors that need to materialize all items.
    ///
    /// Pre-allocates capacity based on `size_hint()` for better performance.
    pub fn collect<T: FromIterator<Value>>(self, vm: &mut VM<'_, '_, impl ResourceTracker>) -> RunResult<T> {
        let mut guard = HeapGuard::new(self, vm);
        let (this, vm) = guard.as_parts_mut();
        HeapedMontyIter(this, vm).collect()
    }
}

struct HeapedMontyIter<'this, 'a, 'p, T: ResourceTracker>(&'this mut MontyIter, &'this mut VM<'a, 'p, T>);

impl<T: ResourceTracker> Iterator for HeapedMontyIter<'_, '_, '_, T> {
    type Item = RunResult<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.for_next(self.1).transpose()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.0.size_hint(self.1.heap);
        (remaining, Some(remaining))
    }
}

impl<'h> HeapRead<'h, MontyIter> {
    /// Advances an iterator and returns the next value.
    ///
    /// Returns `Ok(None)` when the iterator is exhausted.
    /// Returns `Err` for dict/set size changes, allocation failures, or - for a callable-driven
    /// iterator - an error raised by the callable or by the sentinel comparison.
    pub(crate) fn advance(&mut self, vm: &mut VM<'h, '_, impl ResourceTracker>) -> RunResult<Option<Value>> {
        let this = self.get_mut(vm.heap);
        match &mut this.iter_value {
            IterValue::Range { next, step, len } => {
                if this.index >= *len {
                    Ok(None)
                } else {
                    let value = *next;
                    *next += *step;
                    this.index += 1;
                    Ok(Some(Value::Int(value)))
                }
            }
            IterValue::IterStr {
                string,
                byte_offset,
                len,
            } => {
                if this.index >= *len {
                    Ok(None)
                } else {
                    // Get the next character at current byte offset
                    let c = string[*byte_offset..]
                        .chars()
                        .next()
                        .expect("index < len implies char exists");
                    this.index += 1;
                    *byte_offset += c.len_utf8();
                    Ok(Some(allocate_char(c, vm.heap)?))
                }
            }
            IterValue::InternBytes { bytes_id, len } => {
                if this.index >= *len {
                    Ok(None)
                } else {
                    let i = this.index;
                    this.index += 1;
                    let bytes = vm.interns.get_bytes(*bytes_id);
                    Ok(Some(Value::Int(i64::from(bytes[i]))))
                }
            }
            IterValue::HeapRef {
                heap_id,
                len,
                checks_mutation,
            } => {
                if let Some(l) = len
                    && this.index >= *l
                {
                    return Ok(None);
                }

                let heap_id = *heap_id;
                let expected_len = if *checks_mutation { *len } else { None };
                let index = this.index;
                let item = get_heap_item(vm, heap_id, index, expected_len)?;

                // Check for list exhaustion (list can shrink during iteration)
                let Some(item) = item else {
                    return Ok(None);
                };
                self.get_mut(vm.heap).index += 1;
                Ok(Some(item))
            }
            // Binds nothing on purpose: `get_mut` borrows `vm.heap` (not `self`) for as long as any
            // binding taken from `this` is live, and this arm must hand `&mut vm` to a nested VM
            // call. With no bindings, non-lexical lifetimes end that borrow right here, freeing
            // `vm`. The helper then re-opens short windows of its own around that call.
            IterValue::CallableSentinel { .. } => self.advance_callable_sentinel(vm),
        }
    }

    /// Advances a callable-driven `iter(callable, sentinel)` iterator.
    ///
    /// Split out of `advance` because it is the only advance path that re-enters the VM: invoking
    /// the callable can push a frame and run a nested `run()` loop. That makes the heap borrow
    /// window the central concern, so the body is structured as **short windows** around the step -
    /// read the `Copy` fields (`pair`, `done`) and let the borrow end, step with no borrow live at
    /// all, then re-acquire to re-read `done` and persist the outcome - mirroring how the `HeapRef`
    /// arm copies its state out before calling `get_heap_item` and re-acquires afterwards.
    ///
    /// # Foot-guns
    ///
    /// - Never hold anything derived from `get_mut`/`get` across the step. Doing so would either
    ///   fail to compile or, worse, alias heap data across a nested interpreter run.
    /// - **State read before the step describes the past, so `done` is re-read after it.** The step
    ///   runs arbitrary Python, which can advance *this very iterator* through a nested `next()` -
    ///   the callable only has to reach the iterator, and unlike every other variant this one hands
    ///   control back to the interpreter mid-advance. If that inner advance sees the sentinel, the
    ///   iterator is exhausted before the outer call has its value, and yielding that value would
    ///   hand out a result from an already-stopped iterator. Exhaustion happens first and wins:
    ///   the late value is released and `Ok(None)` returned, which is what CPython does. Any state
    ///   this function grows later must be re-read the same way.
    /// - The windows after the step write back **through the iterator entry**, which neither its
    ///   reference count nor this `HeapRead`'s reader count keeps alive, because the collector sweeps
    ///   on reachability alone. `callable_sentinel_step` runs the nested evaluation and the comparison -
    ///   the only points at which a collection can occur - inside [`VM::with_gc_paused`], and control
    ///   reaches the write-back with no collection point after that guard ends, so the entry and
    ///   everything it owns are still there. Introducing one in between, whether an allocation or a
    ///   further re-entry, would require the suspension to be extended to cover it.
    /// - The reader this `HeapRead` holds on the *iterator* entry may safely stay alive across the
    ///   step: `dec_ref` only asserts on readers when it would actually free an entry, and every
    ///   caller keeps the iterator alive for the whole call. A nested advance of the same entry
    ///   therefore takes its own reader and its own `get_mut` window, which is sound precisely
    ///   because this function holds no borrow across the step.
    /// - `done` is written **only** on the `Ok(None)` path, which the step reports for sentinel
    ///   equality alone. Every propagated error leaves via `?` before any window after the step is
    ///   opened, so a raised exception keeps the iterator usable.
    /// - The VM's frame state is not this function's concern, and must not become it:
    ///   `callable_sentinel_step` restores the caller's frame depth and instruction pointer before it
    ///   returns, so an error propagated from here reaches the exception handling of whichever
    ///   operation drove the advance. Doing it there instead of here is what keeps a callable's
    ///   exception catchable on *every* drive path rather than only the one this method serves.
    fn advance_callable_sentinel(&mut self, vm: &mut VM<'h, '_, impl ResourceTracker>) -> RunResult<Option<Value>> {
        // Window 1: copy the state out, then let the borrow on `vm.heap` end immediately.
        let (pair, done) = match &self.get(vm.heap).iter_value {
            IterValue::CallableSentinel { pair, done } => (*pair, *done),
            _ => panic!("advance_callable_sentinel: iterator is not callable-driven"),
        };

        // Sticky exhaustion: once stopped, the callable must never be invoked again, so that a
        // second `next()` raises `StopIteration` and `next(it, default)` returns the default
        // without any further call.
        if done {
            return Ok(None);
        }

        // No heap borrow is live here, which is what makes re-entering the VM sound. The step suspends
        // collection across the nested run - the only point at which one could happen - and no
        // collection point follows before the write-back, so this entry survives into window 2.
        let stepped = callable_sentinel_step(vm, pair)?;

        // Window 2: re-read the sticky flag rather than reuse the pre-call snapshot. The step ran
        // arbitrary Python, which may have advanced *this* iterator through a nested `next()` and
        // seen the sentinel, so `done` above describes a state that no longer exists.
        let done = match &self.get(vm.heap).iter_value {
            IterValue::CallableSentinel { done, .. } => *done,
            _ => panic!("advance_callable_sentinel: iterator is not callable-driven"),
        };

        // Window 3: persist the outcome.
        match stepped {
            // Produced by an iterator that a re-entrant advance has already stopped, so the value
            // arrives too late to be yielded: exhaustion happened first and stays authoritative,
            // exactly as in CPython, where the outer `next(it, default)` returns its default. The
            // value is released here because nothing downstream will.
            Some(value) if done => {
                value.drop_with_heap(vm);
                Ok(None)
            }
            Some(value) => {
                self.get_mut(vm.heap).index += 1;
                Ok(Some(value))
            }
            None => {
                // Exhaustion is sticky and is recorded ONLY here, for the sentinel equality that is
                // the step's single `Ok(None)` condition. Every error already propagated via `?`
                // before this point, so a raised exception leaves `done` false and the iterator
                // usable. Writing it again when a re-entrant advance got here first is harmless.
                let IterValue::CallableSentinel { done, .. } = &mut self.get_mut(vm.heap).iter_value else {
                    panic!("advance_callable_sentinel: iterator is not callable-driven");
                };
                *done = true;
                Ok(None)
            }
        }
    }
}

/// Gets an item from a heap-allocated container at the given index.
///
/// Returns `Ok(None)` if the index is out of bounds (for lists that shrunk during iteration).
/// Returns `Err` if a dict/set changed size during iteration (RuntimeError).
fn get_heap_item(
    vm: &VM<'_, '_, impl ResourceTracker>,
    heap_id: HeapId,
    index: usize,
    expected_len: Option<usize>,
) -> RunResult<Option<Value>> {
    match vm.heap.get(heap_id) {
        HeapData::List(list) => {
            // Check if list shrunk during iteration
            if index >= list.len() {
                return Ok(None);
            }
            Ok(Some(list.as_slice()[index].clone_with_heap(vm)))
        }
        HeapData::Tuple(tuple) => Ok(Some(tuple.as_slice()[index].clone_with_heap(vm))),
        HeapData::NamedTuple(namedtuple) => Ok(Some(namedtuple.as_vec()[index].clone_with_heap(vm))),
        HeapData::Dict(dict) => {
            // Check for dict mutation
            if let Some(expected) = expected_len
                && dict.len() != expected
            {
                return Err(ExcType::runtime_error_dict_changed_size());
            }
            Ok(Some(
                dict.key_at(index).expect("index should be valid").clone_with_heap(vm),
            ))
        }
        HeapData::DictKeysView(view) => {
            let dict = view.dict(vm.heap);
            if let Some(expected) = expected_len
                && dict.len() != expected
            {
                return Err(ExcType::runtime_error_dict_changed_size());
            }
            Ok(Some(
                dict.key_at(index).expect("index should be valid").clone_with_heap(vm),
            ))
        }
        HeapData::DictItemsView(view) => {
            let dict = view.dict(vm.heap);
            if let Some(expected) = expected_len
                && dict.len() != expected
            {
                return Err(ExcType::runtime_error_dict_changed_size());
            }
            let (key, value) = dict.item_at(index).expect("index should be valid");
            Ok(Some(super::allocate_tuple(
                smallvec::smallvec![key.clone_with_heap(vm), value.clone_with_heap(vm)],
                vm.heap,
            )?))
        }
        HeapData::DictValuesView(view) => {
            let dict = view.dict(vm.heap);
            if let Some(expected) = expected_len
                && dict.len() != expected
            {
                return Err(ExcType::runtime_error_dict_changed_size());
            }
            Ok(Some(
                dict.value_at(index).expect("index should be valid").clone_with_heap(vm),
            ))
        }
        HeapData::Bytes(bytes) => Ok(Some(Value::Int(i64::from(bytes.as_slice()[index])))),
        HeapData::Set(set) => {
            // Check for set mutation
            if let Some(expected) = expected_len
                && set.len() != expected
            {
                return Err(ExcType::runtime_error_set_changed_size());
            }
            Ok(Some(
                set.storage()
                    .value_at(index)
                    .expect("index should be valid")
                    .clone_with_heap(vm),
            ))
        }
        HeapData::FrozenSet(frozenset) => Ok(Some(
            frozenset
                .storage()
                .value_at(index)
                .expect("index should be valid")
                .clone_with_heap(vm),
        )),
        _ => panic!("get_heap_item: unexpected heap data type"),
    }
}

/// Performs one step of a callable-driven `iter(callable, sentinel)` iterator.
///
/// Invokes the callable with zero arguments and compares the result to the sentinel using `==`
/// rich value comparison. Returns `Ok(None)` when iteration stops and `Ok(Some(value))` otherwise.
///
/// Sentinel equality is the **only** stop condition: the equal value is released and deliberately
/// **not** yielded. Every exception the callable raises propagates unchanged in type and message,
/// because exception transparency on this path is unconditional.
///
/// # Why the step suspends collection
///
/// This is the only iterator advance that re-enters the VM, so it is the only one that can reach a
/// collection point mid-step, while everything the step depends on is held in **Rust locals** -
/// exactly what the mark phase cannot enumerate. The nested evaluation and the comparison therefore
/// run inside [`VM::with_gc_paused`], whose own documentation carries the contract and its limits.
/// Protecting the iterator entry protects everything it owns, because `collect_child_ids` follows
/// `HeapData::Iter` -> `MontyIter::value` -> the `HeapData::Tuple` pair -> the callable and the
/// sentinel. The guard sits here, the single choke point every consumer funnels through, rather than
/// being repeated at each call site.
///
/// # Why the step restores frame state
///
/// Re-entering the VM can also return with the callable's frames still registered - an error that
/// leaves the nested `run()` loop without going through `catch_sync!` skips the unwinding that would
/// have popped them - and with `instruction_ip` pointing inside the callable. Both leave the *caller*
/// looking for its `except` clause in a frame that no longer runs, so a perfectly catchable exception
/// escapes uncaught, breaking the transparency this form promises. [`VM::with_frame_state_restored`]
/// carries the details; the guard belongs **here** rather than in a consumer because an advance is
/// driven from three places - `Opcode::ForIter`, the `next()` builtin through [`iterator_next`], and
/// dict-view set operators through `collect_iterable_to_set` - and only this choke point covers all
/// of them, including any consumer added later.
///
/// # Foot-guns
///
/// - **No heap borrow may be live when this is called.** It re-enters the VM via
///   `VM::evaluate_function`, which can push a frame and run a nested `run()` loop. Callers must
///   close their `get_mut` window first; see `HeapRead::<MontyIter>::advance_callable_sentinel`.
/// - Suspension protects against the collector, not against `dec_ref`. Every caller must still hold
///   a reference count on the iterator for the duration of the advance.
/// - Every propagating failure leaves through `?` before any state is written, so this function must
///   **not** record exhaustion: the caller sets `done` on `Ok(None)`, which is what leaves the
///   iterator live after a *propagated* exception, exactly as CPython does. An exception from the
///   callable arrives as a `RunError` and so propagates unchanged in type and message; a failing
///   comparison arrives as a `ResourceError`, whose `RunError` conversion decides catchability.
/// - **`Ok(Some(value))` reports what the callable produced, not that the value may be yielded.** The
///   nested evaluation can advance *this very iterator* through a recursive `next()`, and that inner
///   advance may be the one that sees the sentinel. Whether the produced value is yielded or released
///   therefore depends on state that only exists once this has returned, which is why the caller
///   re-reads `done` afterwards and discards a value that arrives at an already-stopped iterator.
///   This function reads no mutable iterator state of its own - only the immutable pair - so it stays
///   correct under re-entry without any further coordination.
/// - **Never special-case an exception type here.** Transparency is unconditional, so no error may be
///   inspected, converted, or swallowed - `StopIteration` included. CPython's `calliter_iternext` does
///   clear a `StopIteration` raised by the callable and treats it as a second exhaustion signal; monty
///   deliberately does not, because unchanged propagation of *every* callable exception is this form's
///   specified contract. That difference is an intentional, documented decision, so re-introducing a
///   filter here would break the contract rather than improve fidelity.
fn callable_sentinel_step(vm: &mut VM<'_, '_, impl ResourceTracker>, pair: HeapId) -> RunResult<Option<Value>> {
    // Per-step time limit. Instruction boundaries do not cover repeated advances inside a single
    // operation, which a pure-Rust drive loop performs, nor a direct callable that returns without
    // entering `VM::run` at all. Placed before the guard so a rejection unwinds with nothing acquired,
    // and outside it because the limit must hold while collection is suspended.
    vm.heap.check_time()?;

    // Suspended across the nested call and the comparison below, which is where a collection can
    // occur while these values are held only in Rust locals. See the section above.
    vm.with_gc_paused(|vm| {
        // Wrapped so the frame depth and instruction pointer the caller had are back in place before
        // anything - value or error - leaves this function, whichever consumer drove the advance.
        // Inside the collection suspension so the value handed out through this boundary is still
        // covered by it. See the section above.
        vm.with_frame_state_restored(|vm| {
            // Guard the owned clones as one unit so every exit path - including `?` from the call and
            // from the comparison - releases both. Manual drops would leak here: there are four
            // distinct exits between acquiring these values and releasing them.
            let mut owned_guard = HeapGuard::new(callable_sentinel_pair(vm, pair), vm);
            let ((callable, sentinel), vm) = owned_guard.as_parts();

            // Propagated with `?` and nothing more: whatever the callable raises reaches the caller
            // unchanged in type and message, so no error can be mistaken for exhaustion.
            let produced = vm.evaluate_function("iter(callable, sentinel)", callable, ArgValues::Empty)?;

            // The produced value needs its own guard because its fate is conditional: released when it
            // equals the sentinel, handed back to the caller otherwise, and released if the comparison
            // itself fails. Declared after `owned_guard` so it is dropped first.
            let mut produced_guard = HeapGuard::new(produced, vm);
            let (produced, vm) = produced_guard.as_parts();
            if produced.py_eq(sentinel, vm)? {
                return Ok(None);
            }
            Ok(Some(produced_guard.into_inner()))
        })
    })
}

/// Reads the `(callable, sentinel)` pair tuple backing a callable-driven iterator.
///
/// Returns owned clones of both values, reference counted via `clone_with_heap` so the caller can
/// use them after the heap borrow ends. Takes `&VM` immutably and deliberately does **not** open
/// a `HeapRead<Tuple>`, so no reader count is taken on the pair and no `dec_ref` panic can
/// originate here.
///
/// # Panics
///
/// Panics if `pair` is not a two-element tuple. That is a programmer error, not a runtime condition:
/// [`MontyIter::init`] is the pair's only producer and always allocates exactly `(callable, sentinel)`,
/// tuples are immutable, and the pair is never exposed to Python, so its two-element shape holds for
/// the iterator's life.
fn callable_sentinel_pair(vm: &VM<'_, '_, impl ResourceTracker>, pair: HeapId) -> (Value, Value) {
    let HeapData::Tuple(tuple) = vm.heap.get(pair) else {
        panic!("callable_sentinel_pair: expected the iter(callable, sentinel) pair tuple")
    };
    let [callable, sentinel] = tuple.as_slice() else {
        panic!("callable_sentinel_pair: the iter(callable, sentinel) pair must hold exactly two items")
    };
    (callable.clone_with_heap(vm), sentinel.clone_with_heap(vm))
}

/// Gets the next item from an iterator.
///
/// If the iterator is exhausted:
/// - If `default` is `Some`, returns the default value
/// - If `default` is `None`, raises `StopIteration`
///
/// This implements Python's `next()` builtin semantics.
///
/// # Arguments
/// * `iter_value` - Must be an iterator (heap-allocated MontyIter)
/// * `default` - Optional default value to return when exhausted
/// * `vm` - The VM, providing the heap and interning table the advance needs
///
/// # Errors
/// Returns `StopIteration` if exhausted with no default, or propagates errors from iteration.
///
/// # Foot-gun: a heap `default` survives the advance only because the step suspends collection
///
/// `default` lives in a `HeapGuard` - a Rust local, invisible to the mark phase - while advancing a
/// callable-driven iterator re-enters the interpreter and so can reach a collection point.
/// `callable_sentinel_step` suspends collection for exactly this reason, so nothing here may be
/// restructured to hold a heap value across an advance outside that suspension.
///
/// That same re-entry is why the error propagated by the `?` below is catchable at all: the step
/// restores the caller's frame depth and instruction pointer first, so the `try`/`except` containing
/// this `next()` call is what the exception lookup sees rather than an abandoned callable frame.
pub fn iterator_next(
    iter_value: &Value,
    default: Option<Value>,
    vm: &mut VM<'_, '_, impl ResourceTracker>,
) -> RunResult<Value> {
    let mut default_guard = HeapGuard::new(default, vm);
    let vm = default_guard.heap();

    let Value::Ref(iter_id) = iter_value else {
        return Err(ExcType::type_error_not_iterable(iter_value.py_type(vm)));
    };

    let result = match vm.heap.read(*iter_id) {
        HeapReadOutput::Iter(mut iter) => iter.advance(vm)?,
        other => {
            let data_type = other.py_type(vm);
            return Err(ExcType::type_error(format!("'{data_type}' object is not an iterator")));
        }
    };

    match result {
        Some(item) => Ok(item),
        None => {
            // Iterator exhausted
            match default_guard.into_inner() {
                Some(d) => Ok(d),
                None => Err(ExcType::stop_iteration()),
            }
        }
    }
}

/// Type-specific iteration data for different Python iterable types.
///
/// Each variant stores the data needed to iterate over a specific type,
/// excluding the index which is stored in the parent `MontyIter` struct.
///
/// # Foot-gun: variant order is append-only
///
/// This enum is serialized into interpreter snapshots and postcard encodes enum discriminants
/// **positionally**, so a new variant must be *appended* after the existing ones; moving or
/// inserting one silently stops every stored snapshot from loading.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum IterValue {
    /// Iterating over a Range, yields `Value::Int`.
    Range {
        /// Next value to yield.
        next: i64,
        /// Step between values.
        step: i64,
        /// Total number of elements.
        len: usize,
    },
    /// Iterating over a string (heap or interned), yields single-char Str values.
    ///
    /// Stores a copy of the string content plus a byte offset for O(1) UTF-8 character access.
    /// We store the string rather than referencing the heap because `for_next()` needs mutable
    /// heap access to allocate the returned character strings, which would conflict with
    /// borrowing the source string from the heap.
    IterStr {
        /// Copy of the string content for iteration.
        string: String,
        /// Current byte offset into the string (points to next char to yield).
        byte_offset: usize,
        /// Total number of characters in the string.
        len: usize,
    },
    /// Iterating over interned bytes, yields `Value::Int` for each byte.
    InternBytes { bytes_id: BytesId, len: usize },
    /// Iterating over a heap-allocated container (List, Tuple, NamedTuple, Dict, Bytes, Set, FrozenSet).
    ///
    /// - `len`: `None` for List (checked dynamically since lists can mutate during iteration),
    ///   `Some(n)` for other types (captured at construction for exhaustion checking).
    /// - `checks_mutation`: `true` for Dict/Set (raises RuntimeError if size changes),
    ///   `false` for other types.
    HeapRef {
        heap_id: HeapId,
        len: Option<usize>,
        checks_mutation: bool,
    },
    /// Iterating by repeatedly calling a callable until it returns the sentinel.
    ///
    /// Produced only by the two-argument `iter(callable, sentinel)` form. Unlike every other
    /// variant this one has no length and no backing container: each step re-enters the VM to
    /// invoke the callable, then compares the result to the sentinel with `==`.
    ///
    /// # Foot-guns
    ///
    /// - `pair` is a **non-owning mirror** of the `HeapId` already owned by `MontyIter::value`,
    ///   exactly as `HeapRef::heap_id` mirrors it, so it is *not* separately reference counted and
    ///   must never be dropped, cloned into a `Value`, or outlive `MontyIter::value`. It points at
    ///   a two-element tuple `(callable, sentinel)`. That indirection is the whole design: this
    ///   iterator owns two values while `collect_child_ids` traces only `MontyIter::value`, so
    ///   routing both through one container lets the collector reach them through the pre-existing
    ///   `HeapData::Iter` -> `HeapData::Tuple` arms - keeping mark-and-sweep sound with no change
    ///   to `heap.rs` and none to the lifecycle methods, which all route through `value` alone. It
    ///   is also why no `Value` is held inline despite the `Clone` derive. A tuple is the right
    ///   container because it is immutable and reference-traced element-wise, so the pair's shape is
    ///   guaranteed for the iterator's whole life; [`MontyIter::init`] allocates it and documents the
    ///   ownership consequences at construction.
    /// - `done` is **sticky** and is set **only** when a produced value compares equal to the
    ///   sentinel. An exception escaping the callable must leave it `false` so the iterator stays
    ///   live, matching CPython, where the next `next()` after a propagated error succeeds. Once
    ///   set, the callable is never invoked again.
    CallableSentinel { pair: HeapId, done: bool },
}

impl IterValue {
    fn new(value: &Value, vm: &mut VM<'_, '_, impl ResourceTracker>) -> Option<Self> {
        match &value {
            Value::InternString(string_id) => Some(Self::from_str(vm.interns.get_str(*string_id))),
            Value::InternBytes(bytes_id) => Some(Self::from_intern_bytes(*bytes_id, vm.interns)),
            Value::Ref(heap_id) => Self::from_heap_data(*heap_id, vm.heap),
            _ => None,
        }
    }

    /// Creates a Range iterator value.
    fn from_range(range: &Range) -> Self {
        Self::Range {
            next: range.start,
            step: range.step,
            len: range.len(),
        }
    }

    /// Creates an iterator value over a string.
    ///
    /// Copies the string content and counts characters for the length field.
    fn from_str(s: &str) -> Self {
        let len = s.chars().count();
        Self::IterStr {
            string: s.to_owned(),
            byte_offset: 0,
            len,
        }
    }

    /// Creates an iterator value over interned bytes.
    fn from_intern_bytes(bytes_id: BytesId, interns: &Interns) -> Self {
        let bytes = interns.get_bytes(bytes_id);
        Self::InternBytes {
            bytes_id,
            len: bytes.len(),
        }
    }

    /// Creates an iterator value from heap data.
    fn from_heap_data(heap_id: HeapId, heap: &Heap<impl ResourceTracker>) -> Option<Self> {
        match heap.get(heap_id) {
            // List: no captured len (checked dynamically), no mutation check
            HeapData::List(_) => Some(Self::HeapRef {
                heap_id,
                len: None,
                checks_mutation: false,
            }),
            // Tuple/NamedTuple/Bytes/FrozenSet: captured len, no mutation check
            HeapData::Tuple(tuple) => Some(Self::HeapRef {
                heap_id,
                len: Some(tuple.as_slice().len()),
                checks_mutation: false,
            }),
            HeapData::NamedTuple(namedtuple) => Some(Self::HeapRef {
                heap_id,
                len: Some(namedtuple.len()),
                checks_mutation: false,
            }),
            HeapData::Bytes(b) => Some(Self::HeapRef {
                heap_id,
                len: Some(b.len()),
                checks_mutation: false,
            }),
            HeapData::FrozenSet(frozenset) => Some(Self::HeapRef {
                heap_id,
                len: Some(frozenset.len()),
                checks_mutation: false,
            }),
            // Dict and dict views: captured len, WITH mutation check
            HeapData::Dict(dict) => Some(Self::HeapRef {
                heap_id,
                len: Some(dict.len()),
                checks_mutation: true,
            }),
            HeapData::DictKeysView(view) => Some(Self::HeapRef {
                heap_id,
                len: Some(view.dict(heap).len()),
                checks_mutation: true,
            }),
            HeapData::DictItemsView(view) => Some(Self::HeapRef {
                heap_id,
                len: Some(view.dict(heap).len()),
                checks_mutation: true,
            }),
            HeapData::DictValuesView(view) => Some(Self::HeapRef {
                heap_id,
                len: Some(view.dict(heap).len()),
                checks_mutation: true,
            }),
            HeapData::Set(set) => Some(Self::HeapRef {
                heap_id,
                len: Some(set.len()),
                checks_mutation: true,
            }),
            // String: copy content for iteration
            HeapData::Str(s) => Some(Self::from_str(s.as_str())),
            // Range: copy values for iteration
            HeapData::Range(range) => Some(Self::from_range(range)),
            // other types are not iterable
            _ => None,
        }
    }
}

impl DropWithHeap for MontyIter {
    #[inline]
    fn drop_with_heap<H: ContainsHeap>(self, heap: &mut H) {
        Self::drop_with_heap(self, heap);
    }
}

impl HeapItem for MontyIter {
    fn py_estimate_size(&self) -> usize {
        mem::size_of::<Self>()
    }

    fn py_dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        self.value.py_dec_ref_ids(stack);
    }
}
