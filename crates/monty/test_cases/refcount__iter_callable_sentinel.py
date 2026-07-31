# Ownership tests for iter(callable, sentinel): reference counts and cleanup on every exit path.
#
# A callable-driven iterator owns its callable and its sentinel through a hidden two-element pair
# tuple, so the only way to assert that ownership from Python is to release every iterator and every
# produced value by the end of the file and then require that:
#   * each retained heap object is directly bound and its count is exactly its number of bindings -
#     a leaked clone of a callable, sentinel or produced value shows up as a count that is too high,
#     and an over-release trips the ref-count-panic build, and
#   * every live heap entry is reachable from a binding - the harness compares unique references
#     against the heap entry count - so an orphaned iterator, pair tuple or dropped produced value
#     is a failure.
#
# Every exit path a step can take is represented below: a yielded value, a value equal to the
# sentinel, an exception from the callable, exhaustion, and rejection at construction. Nothing here
# crosses the collector's allocation interval, deliberately: sweeping the empty tuple singleton
# shifts `Heap::entry_count`, whose `skip(1)` assumes that singleton is the first live entry, so
# strict matching cannot hold for any program that collects. Collection behaviour is covered by
# `iter__callable_sentinel_gc.py` and by the Rust tests in `tests/resource_limits.rs`, which read
# `allocations_since_gc` directly and can therefore prove a collection actually ran.

# === Normal yield and sentinel stop ===
# Two heap values are yielded and a third, equal to the sentinel by value, is produced and dropped.
# The dropped value must not survive as an orphan, and the sentinel must not keep the pair tuple's
# reference once the iterator is released. The `for` loop also drives the iterator through the
# GetIter pass-through, so the loop must not add an unbalanced reference to it either.
state = {'calls': 0}
stop_list = ['STOP']


def build_item():
    state['calls'] += 1
    return [state['calls']] if state['calls'] < 3 else ['STOP']


drive_iter = iter(build_item, stop_list)
produced = []
for value in drive_iter:
    produced.append(value)
assert produced == [[1], [2]], 'both values before the by-value sentinel are yielded'
assert state['calls'] == 3, 'two yielded values plus one sentinel probe'
drive_iter = None
produced = None
value = None

# === Exhaustion, self-iterability and release ===
# Re-wrapping an iterator returns the identical object, so it must not add a reference that the
# release below cannot undo; an exhausted iterator still owns its pair tuple until its last binding
# goes, and the default handed to next() must come back as the very same object.
exhaust_state = {'calls': 0}
exhaust_stop = ['DONE']
exhaust_default = ['FALLBACK']


def exhaust_step():
    exhaust_state['calls'] += 1
    return [exhaust_state['calls']] if exhaust_state['calls'] < 2 else ['DONE']


exhaust_iter = iter(exhaust_step, exhaust_stop)
same_iter = iter(exhaust_iter)
assert same_iter is exhaust_iter, 're-wrapping an iterator returns the identical object'
first = next(same_iter)
assert first == [1], 'the first value is yielded through the re-wrapped name'
first = None
stopped = False
try:
    next(exhaust_iter)
except StopIteration:
    stopped = True
assert stopped, 'the by-value sentinel stops the iterator'
assert exhaust_state['calls'] == 2, 'one yielded value plus one sentinel probe'
after_stop = next(exhaust_iter, exhaust_default)
assert after_stop is exhaust_default, 'an exhausted iterator returns the exact default object'
assert exhaust_state['calls'] == 2, 'an exhausted iterator never calls the callable again'
after_stop = None
same_iter = None
exhaust_iter = None

# === Release without draining, with a heap callable ===
# A function carrying a default is itself a heap object, so releasing a partly consumed iterator has
# to release the pair tuple, and through it both the callable and the sentinel. Leaking the pair
# would leave this callable at a count of two.
undrained_state = {'calls': 0}
undrained_stop = ['NEVER']


def undrained_step(tag='u'):
    undrained_state['calls'] += 1
    return [undrained_state['calls'], tag]


undrained_iter = iter(undrained_step, undrained_stop)
taken = next(undrained_iter)
assert taken == [1, 'u'], 'a defaults-carrying function drives the iterator'
assert undrained_state['calls'] == 1, 'exactly one call was made'
taken = None
undrained_iter = None

# === Callable exception path ===
# An exception from the callable propagates before any state is written back, so the step has to
# release the callable and sentinel clones it took, and the iterator must remain usable.
error_state = {'calls': 0}
error_stop = ['NEVER']


def raising_step():
    error_state['calls'] += 1
    if error_state['calls'] == 1:
        raise ValueError('boom')
    return [error_state['calls']]


flaky = iter(raising_step, error_stop)
raised = False
try:
    next(flaky)
except ValueError as exc:
    raised = True
    assert str(exc) == 'boom', 'the callable exception propagates unchanged'
assert raised, 'the first step must raise'
survivor = next(flaky)
assert survivor == [2], 'the iterator is still usable after a propagated exception'
assert error_state['calls'] == 2, 'exactly one call per step, including the step that raised'
flaky = None
survivor = None

# === Non-callable rejection ===
# The constructor takes ownership of both arguments, so rejecting a non-callable first argument has
# to release both of them; keeping either one alive shows up as a count of two below.
bad_callable = ['not callable']
bad_sentinel = ['sentinel']
rejected = False
try:
    iter(bad_callable, bad_sentinel)
except TypeError as exc:
    rejected = True
    assert str(exc) == 'iter(v, w): v must be callable', 'a non-callable first argument is rejected eagerly'
assert rejected, 'the constructor must reject a list as the callable'
assert bad_callable == ['not callable'], 'the rejected callable argument is untouched'
assert bad_sentinel == ['sentinel'], 'the rejected sentinel argument is untouched'
# ref-counts={'state': 1, 'stop_list': 1, 'exhaust_state': 1, 'exhaust_stop': 1, 'exhaust_default': 1, 'undrained_state': 1, 'undrained_stop': 1, 'undrained_step': 1, 'error_state': 1, 'error_stop': 1, 'bad_callable': 1, 'bad_sentinel': 1}
