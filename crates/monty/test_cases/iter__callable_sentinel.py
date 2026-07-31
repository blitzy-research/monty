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

# Every kind of callable the eager check accepts must actually be invoked, not
# merely accepted, so each one below drives a real iteration. Plain functions and
# lambdas are covered above; a builtin, a closure over an enclosing local and a
# function with default arguments are three separate dispatch paths.
# A builtin: int() produces 0 on every call, so the integer sentinel 0 stops it
# on the first probe - enough to prove a builtin is both accepted and called.
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

# Self-iterability is also what lets a for loop consume an already-constructed
# iterator at all, and that applies to every iterator rather than only to the
# callable-driven one: a one-argument iterator must drive a for loop too. This is
# the prerequisite every sentinel loop in this file relies on, so it is asserted
# directly instead of only through the two-argument form.
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
