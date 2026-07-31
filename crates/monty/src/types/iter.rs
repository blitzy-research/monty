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
/// container for `HeapRef`, and the `[callable, sentinel]` pair tuple for `CallableSentinel`, whose
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
    ///   self-iterable and exceptions raised by `callable` propagate unchanged.
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

            // Both values move into the pair tuple with no clone, and `allocate_tuple` returns a
            // `Value::Ref` because the input is never empty. Keeping only that one reference in
            // `value` is what makes both of them reachable from `collect_child_ids` - see
            // `IterValue::CallableSentinel` for why the pair exists.
            let pair_value = super::allocate_tuple(smallvec::smallvec![iterable, s], vm.heap)?;

            return Self::allocate_callable_sentinel(pair_value, vm);
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

    /// Allocates the heap entry for a callable-driven `iter(callable, sentinel)` iterator.
    ///
    /// `pair_value` must be the single owning reference to the two-element `[callable, sentinel]`
    /// pair tuple; ownership of that reference moves into the new iterator, whose `value` field
    /// becomes the collector's only edge to it. The callable is deliberately **not** invoked, so
    /// construction stays frame-push-free (see [`MontyIter::init`]). The entry is built here rather
    /// than through [`MontyIter::new`] so that `IterValue::from_heap_data` - and with it every
    /// other `MontyIter::new` call site - need not learn about this variant.
    ///
    /// # Foot-guns
    ///
    /// - The entry is allocated holding `Value::None` and the pair reference is moved in
    ///   **afterwards**, because `Heap::allocate` takes its `HeapData` by value and destroys a
    ///   rejected one with ordinary Rust destruction, which has no heap access: handing the pair
    ///   over up front would leak the whole pair sub-graph - or panic in `Value::drop` under
    ///   `ref-count-panic` - whenever a resource limit rejects the iterator, and no call-site guard
    ///   could repair that once ownership is inside `allocate`. The caller's own `allocate_tuple`
    ///   inherits that limitation, as every owned-value allocation in the crate does, because an
    ///   immutable tuple cannot be allocated reference-free and filled later.
    /// - Nothing fallible may be inserted between `allocate` succeeding and the move-in. Across that
    ///   gap `pair_value` is the pair's *only* owner - the new iterator still holds `Value::None` -
    ///   while nothing owns the new iterator entry, so an early return would strand both. The
    ///   move-in goes through the **infallible** `HeapRead::get_mut`, which is what closes the gap.
    /// - `Heap::mark_potential_cycle` re-establishes the cycle metadata `Heap::allocate` maintains
    ///   for reference-holding data, which the reference-free iterator never raised. It is
    ///   **conservative** rather than load-bearing - a pair that holds references already raised the
    ///   flag during its own allocation, and a reference-free pair has no outgoing edge - but
    ///   `Heap::should_gc` gates *all* collection on that flag, so keep the mutation and the
    ///   metadata in step.
    fn allocate_callable_sentinel(pair_value: Value, vm: &mut VM<'_, '_, impl ResourceTracker>) -> RunResult<Value> {
        let Some(pair) = pair_value.ref_id() else {
            panic!("iter(callable, sentinel): allocate_tuple must return a reference for a two-element pair")
        };

        // Reference-free, so a rejected allocation destroys nothing that owns a heap reference.
        let iter = Self {
            index: 0,
            iter_value: IterValue::CallableSentinel { pair, done: false },
            value: Value::None,
        };
        let id = match vm.heap.allocate(HeapData::Iter(iter)) {
            Ok(id) => id,
            Err(err) => {
                // The rejected iterator held no reference, so the pair is still ours to release,
                // which cascades into both elements: one direct drop on a single linear path.
                pair_value.drop_with_heap(vm);
                return Err(err.into());
            }
        };

        // Infallible move-in. The entry was created immediately above, so it exists and its variant
        // is known.
        let HeapReadOutput::Iter(mut entry) = vm.heap.read(id) else {
            panic!("iter(callable, sentinel): a freshly allocated iterator must read back as HeapData::Iter")
        };
        // Overwrites `Value::None`, which owns nothing, so this cannot drop a reference; ownership
        // of the pair transfers into the iterator here.
        entry.get_mut(vm.heap).value = pair_value;
        // Release the reader count before handing the reference out, so the caller sees an entry
        // with no outstanding readers.
        drop(entry);
        // This entry now holds a reference even though it was allocated without one, so re-establish
        // the cycle metadata `Heap::allocate` maintains for reference-holding data. Conservative
        // rather than load-bearing - see the foot-gun above.
        vm.heap.mark_potential_cycle();

        Ok(Value::Ref(id))
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
        // inlined as a no-op. For LimitTracker it ensures that Rust-side loops
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
                // NOTE: this arm is currently unreachable. `IterValue::CallableSentinel` is only
                // ever produced by `MontyIter::init`, and `IterValue::from_heap_data` returns
                // `None` for `HeapData::Iter`, so `MontyIter::new` - the only route into a bare
                // `MontyIter` - can never build one. It is implemented correctly rather than left
                // as `unreachable!()` so the two advance paths cannot silently diverge if that
                // ever changes; `HeapRead::advance` is the path Python code actually takes. Unlike
                // there, `self` and `vm` are disjoint parameters, so `vm` can be handed to the
                // nested call directly with no borrow window to juggle.
                if *done {
                    return Ok(None);
                }
                if let Some(value) = callable_sentinel_step(vm, *pair)? {
                    // `index` is a disjoint field, so it can be bumped while `iter_value` is borrowed.
                    self.index += 1;
                    Ok(Some(value))
                } else {
                    // Exhaustion is sticky, and is recorded ONLY on sentinel equality. An error
                    // from the step above propagated via `?` and never reaches here, leaving
                    // the iterator live for the next call - exactly as CPython behaves.
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
            // `vm`. The helper then re-opens two short windows of its own.
            IterValue::CallableSentinel { .. } => self.advance_callable_sentinel(vm),
        }
    }

    /// Advances a callable-driven `iter(callable, sentinel)` iterator.
    ///
    /// Split out of `advance` because it is the only advance path that re-enters the VM: invoking
    /// the callable can push a frame and run a nested `run()` loop. That makes the heap borrow
    /// window the central concern, so the body is structured as **two short windows** around the
    /// step - read the `Copy` fields (`pair`, `done`) and let the borrow end, step with no borrow
    /// live at all, then re-acquire to persist the outcome - mirroring how the `HeapRef` arm copies
    /// its state out before calling `get_heap_item` and re-acquires afterwards.
    ///
    /// # Foot-guns
    ///
    /// - Never hold anything derived from `get_mut`/`get` across the step. Doing so would either
    ///   fail to compile or, worse, alias heap data across a nested interpreter run.
    /// - The second window writes back **through the iterator entry**, so that entry has to survive
    ///   the nested run - which neither its reference count nor this `HeapRead`'s reader count
    ///   achieves, because the collector sweeps on reachability alone. `callable_sentinel_step`
    ///   suspends collection for the whole step precisely to protect it.
    /// - The reader this `HeapRead` holds on the *iterator* entry may safely stay alive across the
    ///   step: `dec_ref` only asserts on readers when it would actually free an entry, and every
    ///   caller keeps the iterator alive for the whole call - `ForIter` peeks rather than pops,
    ///   `iterator_next` holds it as the live argument, and `dict_view::collect_iterable_to_set`
    ///   holds it in a `HeapGuard` around its drive loop.
    /// - `done` is written **only** on the `Ok(None)` path, so an error, which leaves via `?` before
    ///   the second window, keeps the iterator usable - matching CPython.
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

        // No heap borrow is live here, which is what makes re-entering the VM sound. The step
        // itself suspends collection, which is what keeps this entry - and everything it owns -
        // alive across the nested run.
        let stepped = callable_sentinel_step(vm, pair)?;

        // Window 2: persist the outcome.
        let this = self.get_mut(vm.heap);
        if let Some(value) = stepped {
            this.index += 1;
            Ok(Some(value))
        } else {
            // Exhaustion is sticky and is recorded ONLY here, on sentinel equality. An error from
            // the step above already propagated via `?` before this point, so a raised exception
            // leaves `done` false and the iterator usable - exactly as CPython behaves.
            let IterValue::CallableSentinel { done, .. } = &mut this.iter_value else {
                panic!("advance_callable_sentinel: iterator is not callable-driven");
            };
            *done = true;
            Ok(None)
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
/// rich value comparison. Returns `Ok(None)` when the sentinel is reached (the produced value is
/// released and deliberately **not** yielded, matching CPython) and `Ok(Some(value))` otherwise.
///
/// # Why the step suspends garbage collection
///
/// This is the only iterator advance in the interpreter that re-enters the VM, so it is the only
/// one that can reach a collection point mid-step. [`VM::with_gc_paused`] documents the mechanism
/// and its scheduling consequences; what matters here is *which* values it protects, each of them
/// reachable only from the **Rust call stack** and therefore invisible to the mark phase:
///
/// - the iterator entry itself, which `advance_callable_sentinel` writes its step state back
///   through, and which `next(iter(f, s))` owns only via the argument list popped before the
///   builtin ran,
/// - that entry's pair tuple, reached transitively from it, and
/// - whatever the **enclosing** runtime-Rust frame is accumulating, such as the `Set` that
///   `dict_view::collect_iterable_to_set` fills while driving this iterator, or the default value
///   `iterator_next` is holding for the exhausted case.
///
/// Suspension covers all three at once; rooting them individually cannot, because `HeapRead`
/// carries no `HeapId` and no registry can enumerate an enclosing Rust frame's locals.
///
/// # Foot-guns
///
/// - **No heap borrow may be live when this is called.** It re-enters the VM via
///   `VM::evaluate_function`, which can push a frame and run a nested `run()` loop. Callers must
///   close their `get_mut` window first; see `HeapRead::<MontyIter>::advance_callable_sentinel`.
/// - Keep the pause scoped to the step. Holding it longer lets unreachable cycles accumulate,
///   which under a memory-limited `ResourceTracker` surfaces as a spurious allocation failure
///   instead of a collection.
/// - Both failure paths leave through `?` before any state is written, so this function must
///   **not** record exhaustion: the caller sets `done` only when `Ok(None)` is returned, which is
///   what leaves the iterator live after a raised exception, exactly as CPython does. The two paths
///   are not equivalent, though. `VM::evaluate_function` already returns a `RunError`, so an
///   exception from the **callable** propagates unchanged in type and message - that is what makes
///   exception transparency automatic. `Value::py_eq` returns a `ResourceError`, so a failing
///   **comparison** goes through `From<ResourceError> for RunError`, which also decides
///   catchability: `Recursion` becomes a catchable `RecursionError`, while allocation, memory and
///   time failures become uncatchable so untrusted code cannot suppress a limit violation.
fn callable_sentinel_step(vm: &mut VM<'_, '_, impl ResourceTracker>, pair: HeapId) -> RunResult<Option<Value>> {
    // Suspended for the whole step, not merely for the call: the clones taken below, the iterator
    // entry they came from and the caller's own locals are owned by Rust locals, which the mark
    // phase cannot see. See the section above.
    vm.with_gc_paused(|vm| {
        // Guard the owned clones as one unit so every exit path - including `?` from the call and
        // from the comparison - releases both. Manual drops would leak here: there are four
        // distinct exits between acquiring these values and releasing them.
        let mut owned_guard = HeapGuard::new(callable_sentinel_pair(vm, pair), vm);
        let ((callable, sentinel), vm) = owned_guard.as_parts();

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
}

/// Reads the `[callable, sentinel]` pair tuple backing a callable-driven iterator.
///
/// Returns owned clones of both values, reference counted via `clone_with_heap` so the caller can
/// use them after the heap borrow ends. Takes `&VM` immutably and deliberately does **not** open
/// a `HeapRead<Tuple>`, so no reader count is taken on the pair and no `dec_ref` panic can
/// originate here.
///
/// # Panics
///
/// Panics if `pair` is not a two-element tuple. That is a programmer error rather than a runtime
/// condition: `MontyIter::init` is the only producer of the pair and it always allocates exactly
/// `[callable, sentinel]`, which nothing else can reach because the tuple is never exposed to
/// Python and a tuple is immutable in any case.
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
    ///   a two-element tuple `[callable, sentinel]`. That indirection is the whole design: this
    ///   iterator owns two values while `collect_child_ids` traces only `MontyIter::value`, so
    ///   routing both through one tuple lets the collector reach them through the pre-existing
    ///   `HeapData::Iter` -> `HeapData::Tuple` arms - keeping mark-and-sweep sound with no change
    ///   to `heap.rs` and none to the lifecycle methods, which all route through `value` alone. It
    ///   is also why no `Value` is held inline despite the `Clone` derive.
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
