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

# === StopIteration from the callable signals exhaustion ===
# StopIteration is the one exception the sentinel form does not propagate: it is
# a second way of saying "no more values", so iteration ends instead of the
# program aborting. Exhaustion is just as sticky as it is for the sentinel, and
# any message the callable attached is dropped because the StopIteration a later
# next() raises is a fresh one.
stop_raiser_calls = [0]


def stop_raiser():
    stop_raiser_calls[0] = stop_raiser_calls[0] + 1
    raise StopIteration


it = iter(stop_raiser, 0)
try:
    next(it)
    assert False, 'expected a StopIteration from the callable to end iteration'
except StopIteration as exc:
    assert str(exc) == '', 'StopIteration from the callable carries no message'
assert stop_raiser_calls[0] == 1, 'the callable was invoked exactly once'
try:
    next(it)
    assert False, 'expected the second next() to raise StopIteration again'
except StopIteration as exc:
    assert str(exc) == '', 'the repeated StopIteration also carries no message'
assert stop_raiser_calls[0] == 1, 'exhaustion is sticky, so the callable is not re-invoked'
assert next(it, 'DEF') == 'DEF', 'next(it, default) returns the default after a callable StopIteration'
assert stop_raiser_calls[0] == 1, 'next(it, default) must not re-invoke the callable'

# a message on the raised StopIteration is discarded, not reported
msg_calls = [0]


def stop_with_message():
    msg_calls[0] = msg_calls[0] + 1
    raise StopIteration('custom')


try:
    next(iter(stop_with_message, 0))
    assert False, 'expected StopIteration to end iteration'
except StopIteration as exc:
    assert str(exc) == '', 'the message on the raised StopIteration is dropped'
assert msg_calls[0] == 1, 'the message-carrying callable was invoked exactly once'


# a for loop over a callable that raises immediately ends normally
def stop_first_call():
    return next(iter([]))


result = []
for value in iter(stop_first_call, 0):
    result.append(value)
assert result == [], 'a for loop ends normally when the callable raises StopIteration'

# values yielded before the StopIteration are kept
partial_calls = [0]


def stop_at_third():
    partial_calls[0] = partial_calls[0] + 1
    if partial_calls[0] == 3:
        raise StopIteration
    return partial_calls[0]


result = []
for value in iter(stop_at_third, 99):
    result.append(value)
assert result == [1, 2], 'the values produced before the StopIteration are yielded'
assert partial_calls[0] == 3, 'two yields plus the call that signalled exhaustion'

# the comprehension drive path behaves identically
comp_stop_calls = [0]


def stop_at_second():
    comp_stop_calls[0] = comp_stop_calls[0] + 1
    if comp_stop_calls[0] == 2:
        raise StopIteration
    return comp_stop_calls[0]


assert [v for v in iter(stop_at_second, 99)] == [1], 'a comprehension ends on a callable StopIteration'
assert comp_stop_calls[0] == 2, 'one yield plus the call that signalled exhaustion'

# the canonical wrapper idiom: drive an inner iterator through next()
source = iter([1, 2, 3])


def pull():
    return next(source)


result = []
for value in iter(pull, None):
    result.append(value)
assert result == [1, 2, 3], 'iter(callable, sentinel) can wrap an inner iterator driven by next()'


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
