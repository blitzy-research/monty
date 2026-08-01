# Behavioral conformance for the two-argument form of the iter builtin,
# iter(callable, sentinel). Every expected value below is what CPython 3.14.6
# produces; the harness runs this file against both monty and CPython.

# === Drive until sentinel via for loop ===
# Each step calls the callable with no arguments. The value that compares equal
# to the sentinel stops iteration and is NOT yielded, so draining a three-item
# run costs four calls: three yields plus one sentinel probe.
drive_calls = [0]


def count_up():
    drive_calls[0] = drive_calls[0] + 1
    return drive_calls[0]


result = []
for value in iter(count_up, 4):
    result.append(value)
assert result == [1, 2, 3], 'for loop over iter(callable, sentinel) stops before the sentinel'
assert drive_calls[0] == 4, 'draining a three-item run calls the callable four times'

# the canonical "call this until it returns X" idiom, driven by a lambda
queue = ['a', 'b', '']
result = []
for value in iter(lambda: queue.pop(0), ''):
    result.append(value)
assert result == ['a', 'b'], 'a lambda callable drives the sentinel iterator'
assert queue == [], 'the sentinel value itself was consumed from the queue'

# Builtins, closures over an enclosing local and functions with default arguments
# are each accepted and invoked through their own dispatch path. int() produces 0 on
# every call, so the integer sentinel 0 stops that iteration on the first probe.
assert [v for v in iter(int, 0)] == [], 'the builtin int is accepted as a callable and int() returns the sentinel'


def make_counter():
    running = [0]

    def bump():
        running[0] = running[0] + 1
        return running[0]

    return bump


result = []
for value in iter(make_counter(), 3):
    result.append(value)
assert result == [1, 2], 'a closure over an enclosing local drives the sentinel iterator'

stepped = [0]


def step_by(step=2):
    stepped[0] = stepped[0] + step
    return stepped[0]


result = []
for value in iter(step_by, 6):
    result.append(value)
assert result == [2, 4], 'a function with default arguments drives the sentinel iterator'
assert stepped == [6], 'the default argument applied on all three calls, the sentinel probe included'

# === Immediate sentinel yields nothing ===
# The very first produced value equals the sentinel, so nothing is yielded and
# the callable is invoked exactly once.
empty_calls = [0]


def always_zero():
    empty_calls[0] = empty_calls[0] + 1
    return 0


result = []
for value in iter(always_zero, 0):
    result.append(value)
assert result == [], 'a callable that immediately returns the sentinel yields nothing'
assert empty_calls[0] == 1, 'the single sentinel probe is the only call'

# === Stop test uses == by value, not identity ===
# Cross-type numeric equality: the float 3.0 is a different object of a
# different type from the integer sentinel 3, yet 3.0 == 3, so it stops.
floats = [1.0, 2.0, 3.0]
float_index = [0]


def next_float():
    picked = floats[float_index[0]]
    float_index[0] = float_index[0] + 1
    return picked


result = []
for value in iter(next_float, 3):
    result.append(value)
assert result == [1.0, 2.0], 'float 3.0 stops against integer sentinel 3 by value equality'
assert float_index[0] == 3, 'two yields plus one sentinel probe'

# === Self-iterability ===
# iter() on a callable-sentinel iterator returns the identical object rather
# than wrapping it, so both names drive one shared iteration state.
alias_calls = [0]


def alias_next():
    alias_calls[0] = alias_calls[0] + 1
    return alias_calls[0]


it = iter(alias_next, 0)
alias = iter(it)
assert alias is it, 'iter() on a callable-sentinel iterator returns the identical object'
assert next(alias) == 1, 'the alias yields the first produced value'
assert next(it) == 2, 'the original continues from where the alias left off'
assert alias_calls[0] == 2, 'the two names share one iteration state, not two'

# A one-argument iterator is self-iterable too, so a for loop that asks it for its
# iterator gets the same object back and drives it directly.
result = []
for value in iter([1, 2, 3]):
    result.append(value)
assert result == [1, 2, 3], 'a for loop drives an already-constructed one-argument iterator'

# === Exception propagation from the callable ===
# The exception propagates unchanged in type and message, and it must NOT
# exhaust the iterator: the step after it succeeds.
boom_calls = [0]


def boom_then_continue():
    boom_calls[0] = boom_calls[0] + 1
    if boom_calls[0] == 1:
        raise ValueError('kaboom')
    return boom_calls[0]


it = iter(boom_then_continue, 99)
try:
    next(it)
    assert False, 'expected the callable to raise ValueError'
except ValueError as exc:
    assert str(exc) == 'kaboom', 'the exception from the callable propagates unchanged'
assert next(it) == 2, 'a propagated exception must not exhaust the iterator'
assert boom_calls[0] == 2, 'the callable was invoked exactly twice'


# the same propagation driven by a for loop, which advances the iterator
# through a different opcode than next() does
def always_boom():
    raise ValueError('kaboom')


result = []
try:
    for value in iter(always_boom, 0):
        result.append(value)
    assert False, 'expected the for loop to propagate ValueError'
except ValueError as exc:
    assert str(exc) == 'kaboom', 'the exception propagates unchanged through the for loop'
assert result == [], 'nothing was yielded before the callable raised'

# A raise the callable cannot handle, because its own except clause is invalid.
# That failure is reported while the callable is still mid-flight, so unlike the
# cases above the interpreter has to clean up after a callable that was abandoned
# rather than unwound, and the for loop still has to find its iterator afterwards.
handler_calls = [0]


def invalid_handler():
    handler_calls[0] = handler_calls[0] + 1
    try:
        raise ValueError('inner')
    except 123:
        return 0


result = []
try:
    for value in iter(invalid_handler, 0):
        result.append(value)
    assert False, 'expected the invalid except clause to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'catching classes that do not inherit from BaseException is not allowed', (
        'an invalid except clause inside the callable reports its own TypeError'
    )
assert result == [], 'nothing was yielded before the invalid except clause failed'
assert handler_calls[0] == 1, 'the callable was invoked exactly once before it failed'

# The abandoned callable left nothing behind: a further iteration, driven through
# the very same for loop machinery, still produces exactly the right values.
recovered = ['ok', 'go', '']
result = []
for value in iter(lambda: recovered.pop(0), ''):
    result.append(value)
assert result == ['ok', 'go'], 'iteration still works after the abandoned callable was cleaned up'
assert recovered == [], 'the recovery drive consumed the whole queue including its sentinel'

# The same abandoned callable driven by next(), which reaches the iterator through a
# builtin call rather than the for loop: that route unwinds to the surrounding handler
# as well, and leaves the iterator usable afterwards.
next_handler_calls = [0]


def invalid_handler_then_value():
    next_handler_calls[0] = next_handler_calls[0] + 1
    if next_handler_calls[0] == 1:
        try:
            raise ValueError('inner')
        except 123:
            return 0
    return next_handler_calls[0]


it = iter(invalid_handler_then_value, 0)
try:
    next(it)
    assert False, 'expected the invalid except clause to raise TypeError through next()'
except TypeError as exc:
    assert str(exc) == 'catching classes that do not inherit from BaseException is not allowed', (
        'a next() driven invalid except clause reports its own TypeError to the caller'
    )
assert next(it) == 2, 'the iterator is still usable after the abandoned callable'
assert next_handler_calls[0] == 2, 'the failed step and the following one account for both calls'

# And driven by a dict-view set operator, which advances an iterator object directly:
# a failure inside the callable reaches the surrounding handler on that route too.
view_handler_calls = [0]


def invalid_handler_view():
    view_handler_calls[0] = view_handler_calls[0] + 1
    try:
        raise ValueError('inner')
    except 123:
        return 'x'


try:
    {'a': 1}.keys() | iter(invalid_handler_view, 0)
    assert False, 'expected the invalid except clause to raise TypeError through the dict view'
except TypeError as exc:
    assert str(exc) == 'catching classes that do not inherit from BaseException is not allowed', (
        'a dict-view driven invalid except clause reports its own TypeError to the caller'
    )
assert view_handler_calls[0] == 1, 'the callable was invoked exactly once before it failed'

# a further drive through the same operator, with a callable that behaves: no state
# from the abandoned one remains on this route either
view_queue = ['b', 'c', '']
union = {'a': 1}.keys() | iter(lambda: view_queue.pop(0), '')
assert sorted(union) == ['a', 'b', 'c'], 'a dict-view set operator consumes a sentinel iterator'
assert view_queue == [], 'the union drive consumed the whole queue including its sentinel'

# === A callable-raised StopIteration reads as exhaustion ===
# StopIteration is the one exception the callable can raise that does NOT reach the
# caller: it is read as a second exhaustion signal, so the "call this until it stops"
# shape ends a drive cleanly instead of raising out of it. Exhaustion reached that way
# is indistinguishable from sentinel equality - it is sticky, and a later next() gets a
# fresh StopIteration rather than the message the callable used.
stopper_calls = [0]


def value_then_stop():
    stopper_calls[0] = stopper_calls[0] + 1
    if stopper_calls[0] == 1:
        return 'first'
    raise StopIteration('carried message')


it = iter(value_then_stop, 'never equal')
assert next(it) == 'first', 'the step before the raise yields its value'
assert next(it, 'DEFAULT') == 'DEFAULT', 'a callable-raised StopIteration is read as exhaustion'
assert stopper_calls[0] == 2, 'the raising step is the last invocation of the callable'
assert next(it, 'DEFAULT') == 'DEFAULT', 'that exhaustion is sticky, so the default comes back again'
assert stopper_calls[0] == 2, 'an exhausted iterator never invokes the callable again'
try:
    next(it)
    assert False, 'expected the exhausted iterator to raise StopIteration'
except StopIteration as exc:
    assert str(exc) == '', 'exhaustion raises a fresh StopIteration, not the message the callable used'
assert stopper_calls[0] == 2, 'raising StopIteration again needed no further call'
assert iter(it) is it, 'an exhausted callable-sentinel iterator is still self-iterable'
result = []
for value in it:
    result.append(value)
assert result == [], 'a for loop over the exhausted iterator yields nothing'
assert stopper_calls[0] == 2, 'the for loop drive invoked the callable no further either'


# A bare raise on the very first call ends the loop before anything is yielded.
def stop_at_once():
    raise StopIteration


result = []
for value in iter(stop_at_once, 0):
    result.append(value)
assert result == [], 'a callable that raises StopIteration immediately yields nothing'

# The values produced before the raise are still yielded, through the for loop and
# through the comprehension form.
partial_queue = [1, 2]


def drain_then_stop():
    if partial_queue:
        return partial_queue.pop(0)
    raise StopIteration('drained')


result = []
for value in iter(drain_then_stop, 0):
    result.append(value)
assert result == [1, 2], 'the values produced before the raise are still yielded'

comp_queue = ['a', 'b']


def comp_then_stop():
    if comp_queue:
        return comp_queue.pop(0)
    raise StopIteration


assert [v for v in iter(comp_then_stop, 'zzz')] == ['a', 'b'], 'the comprehension form ends on the raise too'

# The idiom this enables: a callable that forwards to an inner iterator, where the
# inner next() is what raises once the inner iterator runs out.
inner = iter([1, 2, 3])
result = []
for value in iter(lambda: next(inner), 99):
    result.append(value)
assert result == [1, 2, 3], 'a callable that forwards to next() drains the inner iterator'


# The raise need not be lexically inside the callable: one that arrives from a helper
# the callable called is read the same way, because what is inspected is the exception
# that reaches the iterator, not where it was raised.
def raise_stop():
    raise StopIteration('from a helper')


def call_helper():
    return raise_stop()


helper_it = iter(call_helper, 0)
assert next(helper_it, 'DEFAULT') == 'DEFAULT', 'a StopIteration from a nested call also ends iteration'

# Only StopIteration means exhaustion, so the two rules compose: a ValueError still
# propagates and still leaves the iterator live, the value after it is yielded, and the
# StopIteration after that is what finally stops the iterator.
mixed_calls = [0]


def boom_value_stop():
    mixed_calls[0] = mixed_calls[0] + 1
    if mixed_calls[0] == 1:
        raise ValueError('boom')
    if mixed_calls[0] == 2:
        return 2
    raise StopIteration('now done')


it = iter(boom_value_stop, 'never equal')
try:
    next(it)
    assert False, 'expected the callable to raise ValueError'
except ValueError as exc:
    assert str(exc) == 'boom', 'a non-StopIteration exception still propagates unchanged'
assert next(it) == 2, 'the ValueError did not exhaust the iterator'
assert next(it, 'DEF') == 'DEF', 'the StopIteration after it did exhaust the iterator'
assert next(it, 'DEF') == 'DEF', 'and that exhaustion is sticky'
assert mixed_calls[0] == 3, 'one invocation per step, and none once exhausted'

# Sentinel equality is still tested first, so a callable that returns the sentinel
# stops there and is never given the chance to raise.
equal_calls = [0]


def sentinel_then_stop():
    equal_calls[0] = equal_calls[0] + 1
    if equal_calls[0] == 1:
        return 0
    raise StopIteration('unreachable')


result = []
for value in iter(sentinel_then_stop, 0):
    result.append(value)
assert result == [], 'the sentinel stops iteration on the first step'
assert equal_calls[0] == 1, 'the raise was never reached'

# The dict-view set operators advance an iterator object directly, so the same
# exhaustion signal ends the drive on that route as well.
view_stop_queue = ['b', 'c']


def view_then_stop():
    if view_stop_queue:
        return view_stop_queue.pop(0)
    raise StopIteration


union = {'a': 1}.keys() | iter(view_then_stop, 'zzz')
assert sorted(union) == ['a', 'b', 'c'], 'a dict-view operator ends on a callable-raised StopIteration'

# === Argument-count errors ===
# iter takes one or two positional-only arguments; both bounds keep their
# CPython messages.
try:
    iter()
    assert False, 'expected iter() with no arguments to fail'
except TypeError as exc:
    assert str(exc) == 'iter expected at least 1 argument, got 0', 'zero-argument iter() message'

try:
    iter([1], 2, 3)
    assert False, 'expected iter() with three arguments to fail'
except TypeError as exc:
    assert str(exc) == 'iter expected at most 2 arguments, got 3', 'three-argument iter() message'

# === Laziness ===
# Construction invokes the callable zero times, and each next() invokes it
# exactly once.
lazy_calls = [0]


def lazy():
    lazy_calls[0] = lazy_calls[0] + 1
    return lazy_calls[0]


it = iter(lazy, 0)
assert lazy_calls[0] == 0, 'constructing iter(callable, sentinel) must not call the callable'
assert next(it) == 1, 'the first next() yields the first produced value'
assert lazy_calls[0] == 1, 'one next() invokes the callable exactly once'
assert next(it) == 2, 'the second next() yields the second produced value'
assert lazy_calls[0] == 2, 'a second next() adds exactly one more call'

# === Sticky exhaustion ===
# Once the sentinel has been seen the callable is never invoked again, so the
# call counter stays frozen across every later step.
stop_calls = [0]


def stop_immediately():
    stop_calls[0] = stop_calls[0] + 1
    return 0


it = iter(stop_immediately, 0)
try:
    next(it)
    assert False, 'expected the first next() to raise StopIteration'
except StopIteration as exc:
    assert str(exc) == '', 'StopIteration from the sentinel carries no message'
assert stop_calls[0] == 1, 'the sentinel probe invoked the callable exactly once'
try:
    next(it)
    assert False, 'expected the second next() to raise StopIteration again'
except StopIteration as exc:
    assert str(exc) == '', 'the repeated StopIteration also carries no message'
assert stop_calls[0] == 1, 'a second next() must not re-invoke the callable'
assert next(it, 'DEF') == 'DEF', 'next(it, default) returns the default once exhausted'
assert stop_calls[0] == 1, 'next(it, default) must not re-invoke the callable'

# === Re-entrant exhaustion outranks an in-flight result ===
# The callable recursively advances the SAME iterator, and that inner advance is the
# one that sees the sentinel, so the iterator is already exhausted by the time the
# outer step has a value of its own. That value belongs to a stopped iterator and is
# therefore discarded rather than yielded, leaving the outer next() to return its
# default. The iterator is reached through a list because a callable can only read
# globals that already exist where it is defined.
reentrant_calls = [0]
reentrant_holder = []
reentrant_inner = []


def reentrant_driver():
    reentrant_calls[0] = reentrant_calls[0] + 1
    if reentrant_calls[0] == 1:
        reentrant_inner.append(next(reentrant_holder[0], 'INNER'))
        return ['late']
    return 0


it = iter(reentrant_driver, 0)
reentrant_holder.append(it)
assert next(it, 'OUTER') == 'OUTER', 'a value produced after a re-entrant stop is discarded, not yielded'
assert reentrant_inner == ['INNER'], 'the recursive advance is the one that saw the sentinel'
assert reentrant_calls[0] == 2, 'the outer step plus its one recursive step invoke the callable twice'
assert next(it, 'AFTER') == 'AFTER', 'a re-entrant stop is exactly as sticky as an ordinary one'
assert reentrant_calls[0] == 2, 'no further call is made once the iterator has stopped'

# the same collision driven by a for loop, which advances through ForIter rather than
# next(), so the discarded value must never reach the loop variable either
forloop_calls = [0]
forloop_holder = []
forloop_inner = []


def forloop_driver():
    forloop_calls[0] = forloop_calls[0] + 1
    if forloop_calls[0] == 1:
        forloop_inner.append(next(forloop_holder[0], 'INNER'))
        return ['late']
    return 0


it = iter(forloop_driver, 0)
forloop_holder.append(it)
result = []
for value in it:
    result.append(value)
assert result == [], 'the for loop yields nothing when a re-entrant advance already stopped the iterator'
assert forloop_inner == ['INNER'], 'the recursive advance saw the sentinel while the loop step was in flight'
assert forloop_calls[0] == 2, 'the loop leaves the callable invoked exactly twice'

# a re-entrant advance that does NOT reach the sentinel must not suppress anything:
# only exhaustion outranks the in-flight value, so both values are still yielded
nested_calls = [0]
nested_holder = []
nested_inner = []


def nested_driver():
    nested_calls[0] = nested_calls[0] + 1
    if nested_calls[0] == 1:
        nested_inner.append(next(nested_holder[0], 'INNER'))
        return 'outer'
    return nested_calls[0]


it = iter(nested_driver, 99)
nested_holder.append(it)
assert next(it, 'DEF') == 'outer', 'a re-entrant advance that kept the iterator live still yields the outer value'
assert nested_inner == [2], 'the recursive advance yielded its own value rather than stopping'
assert nested_calls[0] == 2, 'the outer step and its recursive step account for both calls'
assert next(it, 'DEF') == 3, 'the iterator keeps producing after a re-entrant advance that did not stop it'
assert nested_calls[0] == 3, 'the following step invokes the callable exactly once more'

# the same collision reached through the other stop condition: here the recursive advance
# is stopped by a callable-raised StopIteration rather than by the sentinel, and it
# outranks the in-flight value in exactly the same way
raising_calls = [0]
raising_holder = []
raising_inner = []


def raising_driver():
    raising_calls[0] = raising_calls[0] + 1
    if raising_calls[0] == 1:
        raising_inner.append(next(raising_holder[0], 'INNER'))
        return ['late']
    raise StopIteration('inner raise')


it = iter(raising_driver, 'never equal')
raising_holder.append(it)
assert next(it, 'OUTER') == 'OUTER', 'a re-entrant StopIteration stop also discards the in-flight value'
assert raising_inner == ['INNER'], 'the recursive advance is the one whose step raised'
assert raising_calls[0] == 2, 'the outer step plus its one recursive step invoke the callable twice'
assert next(it, 'AFTER') == 'AFTER', 'that stop is as sticky as a re-entrant sentinel stop'
assert raising_calls[0] == 2, 'no further call is made once the iterator has stopped'

# and driven by a for loop, so the discarded value never reaches the loop variable
raising_loop_calls = [0]
raising_loop_holder = []
raising_loop_inner = []


def raising_loop_driver():
    raising_loop_calls[0] = raising_loop_calls[0] + 1
    if raising_loop_calls[0] == 1:
        raising_loop_inner.append(next(raising_loop_holder[0], 'INNER'))
        return ['late']
    raise StopIteration


it = iter(raising_loop_driver, 'never equal')
raising_loop_holder.append(it)
result = []
for value in it:
    result.append(value)
assert result == [], 'the for loop yields nothing when a re-entrant raise already stopped the iterator'
assert raising_loop_inner == ['INNER'], 'the recursive advance raised while the loop step was in flight'
assert raising_loop_calls[0] == 2, 'the loop leaves the callable invoked exactly twice'

# === Comprehension form ===
comp_calls = [0]


def comp_next():
    comp_calls[0] = comp_calls[0] + 1
    return comp_calls[0]


assert [v for v in iter(comp_next, 4)] == [1, 2, 3], 'comprehension over iter(callable, sentinel)'
assert comp_calls[0] == 4, 'the comprehension drives the callable four times'

# === while True plus next() drive path ===
while_calls = [0]


def while_next():
    while_calls[0] = while_calls[0] + 1
    return while_calls[0]


it = iter(while_next, 3)
result = []
while True:
    try:
        result.append(next(it))
    except StopIteration:
        break
assert result == [1, 2], 'while True plus next() drives the sentinel iterator to exhaustion'
assert while_calls[0] == 3, 'two yields plus one sentinel probe'

# === Heap object sentinel compared by value ===
# The sentinel is its own empty list, never the same object as the empty list
# the callable produces, so stopping proves the test is value comparison.
pending = [[1], [2], []]
pending_index = [0]


def next_list():
    picked = pending[pending_index[0]]
    pending_index[0] = pending_index[0] + 1
    return picked


result = []
for value in iter(next_list, []):
    result.append(value)
assert result == [[1], [2]], 'an empty-list sentinel stops iteration by value, not identity'
assert pending_index[0] == 3, 'two yields plus one sentinel probe'

# === Non-callable first argument ===
# Callability is validated eagerly, so the two-argument form never falls back
# to treating the first argument as a plain iterable.
try:
    iter(5, 1)
    assert False, 'expected a non-callable first argument to fail'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'non-callable int first argument message'

try:
    iter([1, 2], 0)
    assert False, 'expected an iterable but non-callable first argument to fail'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'non-callable list first argument message'


# === Inline capturing callables in statement-header positions ===
# The canonical shape of this idiom writes the callable inline, inside a function, over
# that function's own local - `for v in iter(lambda: buf.pop(0), 0):`. The callable runs
# in a frame of its own while the captured local lives in the enclosing frame, so the two
# must share one object: a mutation the callable performs has to be visible after the
# loop, and a name rebound after the iterator was built has to be visible to the callable.
# Each case below places the construction in a different statement position - a for
# header, a while test, an if condition, an assert and a raise - because a statement's
# header expression is evaluated in the enclosing frame exactly as its body is, and the
# sections above only ever build inline callables at module scope.
def drive_inline(first):
    buffer = [first, first + 1, 0]
    collected = []
    for value in iter(lambda: buffer.pop(0), 0):
        collected.append(value)
    return (collected, buffer)


assert drive_inline(1) == ([1, 2], []), 'an inline lambda in a for header drives over an enclosing local'


def rebind_after_construction():
    source = [1, 0]
    stream = iter(lambda: source.pop(0), 0)
    source = [7, 8, 0]
    collected = []
    for value in stream:
        collected.append(value)
    return (collected, source)


assert rebind_after_construction() == ([7, 8], []), 'the inline callable reads the current binding, not a copy'


def while_test_inline():
    pulls = ['a', 'b', '']
    seen = []
    while next(iter(lambda: pulls.pop(0), ''), '') != '':
        seen.append(len(pulls))
    return (seen, pulls)


assert while_test_inline() == ([2, 1], []), 'an inline lambda in a while test drives over an enclosing local'


def if_condition_inline(probe):
    box = [probe]
    if next(iter(lambda: box.pop(0), 0), 'stopped') == probe:
        return 'yielded'
    return 'stopped'


assert if_condition_inline(5) == 'yielded', 'an inline lambda in an if condition yields a non-sentinel value'
assert if_condition_inline(0) == 'stopped', 'the same shape reports exhaustion when the first value is the sentinel'


def assert_position_inline(probe):
    box = [probe, 0]
    assert next(iter(lambda: box.pop(0), 0)) == probe, 'the inline callable ran inside the assert'
    return box


assert assert_position_inline(3) == [0], 'an inline lambda in an assert drives over an enclosing local'


def raise_position_inline():
    payload = ['detail', 0]
    try:
        raise ValueError(next(iter(lambda: payload.pop(0), 0)))
    except ValueError as exc:
        return (str(exc), payload)


assert raise_position_inline() == ('detail', [0]), 'an inline lambda in a raise drives over an enclosing local'


def nested_headers():
    queue = [1, 2, 0]
    collected = []
    while len(collected) < 2:
        if len(queue) > 1:
            for value in iter(lambda: queue.pop(0), 0):
                collected.append(value)
    return (collected, queue)


assert nested_headers() == ([1, 2], []), 'header positions nest, so a for inside if inside while still captures'


# A callback builtin in the same position captures the same way, so one control keeps the
# two shapes honest about sharing a single mechanism rather than an iter-specific one.
def sorted_key_inline(bias):
    offsets = [bias]
    ordered = []
    for value in sorted([2, 1], key=lambda item: item + offsets[0]):
        ordered.append(value)
    return (ordered, offsets)


assert sorted_key_inline(10) == ([1, 2], [10]), 'an inline capturing key in a for header orders correctly'
