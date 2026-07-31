# Conformance tests for the two-argument form of iter(), the sentinel iteration protocol.
#
# iter(callable, sentinel) returns an iterator that calls its first argument with no arguments on
# every step. If the produced value compares equal to the sentinel with `==` (rich value
# comparison, never identity) iteration stops with StopIteration and that value is NOT yielded;
# otherwise the value is yielded. Exceptions raised by the callable propagate unchanged and do not
# exhaust the iterator, while a produced sentinel exhausts it permanently.
#
# Every call counter below is asserted, so any extra, missing or eager invocation fails the test
# rather than passing silently.

# === Drive until sentinel ===
# The canonical idiom is a for loop, which additionally requires the object iter() returns to be
# accepted as an iterable in its own right.
calls = {'read': 0, 'grow': 0}


def read_int():
    calls['read'] += 1
    return calls['read'] if calls['read'] < 4 else 0


driven = []
for value in iter(read_int, 0):
    driven.append(value)
assert driven == [1, 2, 3], 'a for loop yields every value produced before the sentinel'
assert calls['read'] == 4, 'three yielded values plus one sentinel probe'

# Driving a plain one-argument iterator with a for loop is the prerequisite for the loop above:
# both reach the same opcode, so a narrow implementation that only accepted callable-driven
# iterators would leave ordinary iterator objects non-iterable.
generic = []
for value in iter([1, 2, 3]):
    generic.append(value)
assert generic == [1, 2, 3], 'a for loop over a one-argument iterator yields the underlying values'

# A function carrying default arguments is a distinct kind of callable value, and it must drive the
# iterator exactly as a plain function does.


def grow(step=2):
    calls['grow'] += step
    return calls['grow']


assert [v for v in iter(grow, 6)] == [2, 4], 'a function with default arguments drives the iterator'
assert calls['grow'] == 6, 'two yielded values plus one sentinel probe'

# === Immediate sentinel ===
# When the very first call produces the sentinel the iterator is empty, and that value is dropped
# rather than yielded.
immediate = {'n': 0}


def always_stop():
    immediate['n'] += 1
    return 'STOP'


assert [v for v in iter(always_stop, 'STOP')] == [], 'a callable that stops immediately yields nothing'
assert immediate['n'] == 1, 'exactly one probe is needed to discover an empty iterator'

# === Equality is by value, not identity ===
# The stop test uses `==`, so values that are equal without being the same object still stop it.
floats = {'n': 0}


def float_step():
    floats['n'] += 1
    return floats['n'] + 0.0


assert [v for v in iter(float_step, 3)] == [1.0, 2.0], 'a float result stops against an equal integer sentinel'
assert floats['n'] == 3, 'two yielded values plus the 3.0 == 3 sentinel probe'
assert [v for v in iter(lambda: 0, False)] == [], '0 == False stops immediately'
assert [v for v in iter(lambda: 1, True)] == [], '1 == True stops immediately'

# === Self-iterability ===
# iter() on an existing iterator returns that very object, so wrapping it again neither copies it
# nor advances it.
shared = {'n': 0}


def counter():
    shared['n'] += 1
    return shared['n']


counting_iter = iter(counter, 5)
assert iter(counting_iter) is counting_iter, 'iter() on a callable-driven iterator returns the same object'
assert shared['n'] == 0, 'passing the iterator back through iter() calls nothing'
assert next(counting_iter) == 1, 'the iterator advances once'
assert next(iter(counting_iter)) == 2, 're-wrapping preserves the iteration state'
assert shared['n'] == 2, 'exactly one call per step, and none for the re-wrapping'

# === Exception propagation and reuse ===
# An exception from the callable propagates unchanged in type and message, and it must leave the
# iterator usable: the following step calls the callable again.
raising = {'n': 0}


def sometimes_raises():
    raising['n'] += 1
    if raising['n'] == 2:
        raise ValueError('kaboom')
    return raising['n']


flaky = iter(sometimes_raises, 9)
assert next(flaky) == 1, 'the first step yields normally'
try:
    next(flaky)
    assert False, 'expected the callable to propagate ValueError'
except ValueError as exc:
    assert str(exc) == 'kaboom', 'the callable exception propagates with its message unchanged'
    assert exc.args == ('kaboom',), 'the exception arguments are untouched'
assert next(flaky) == 3, 'a propagated exception does not exhaust the iterator'
assert raising['n'] == 3, 'exactly one call per step, including the step that raised'

# The same holds when a for loop is the driver: the exception escapes the loop statement, and the
# values produced before it were already yielded.
looping = {'n': 0}


def raise_on_second():
    looping['n'] += 1
    if looping['n'] == 2:
        raise ValueError('inside the loop')
    return looping['n']


collected = []
try:
    for value in iter(raise_on_second, 0):
        collected.append(value)
    assert False, 'expected the for loop to propagate ValueError'
except ValueError as exc:
    assert str(exc) == 'inside the loop', 'the callable exception propagates out of the for loop'
assert collected == [1], 'values yielded before the exception are unaffected'
assert looping['n'] == 2, 'the loop made exactly one call per step'

# An error raised by the sentinel comparison itself behaves the same way: it propagates and leaves
# the iterator live, because exhaustion is recorded only when a value really did equal the sentinel.
# Exceeding the recursion limit is the comparison's only failure mode, and comparing two distinct
# self-referential lists reaches it; the message is not asserted because it reports the interpreter's
# own stack usage and so is not stable. The call counter is what proves the produced value was
# handled rather than the step being retried, and the two steps after the error prove the iterator's
# `done` flag and index were left untouched.
cyclic_sentinel = []
cyclic_sentinel.append(cyclic_sentinel)
cyclic_value = []
cyclic_value.append(cyclic_value)
comparing = {'n': 0}


def produce_cyclic():
    comparing['n'] += 1
    return cyclic_value


cyclic_iter = iter(produce_cyclic, cyclic_sentinel)
try:
    next(cyclic_iter)
    assert False, 'expected the sentinel comparison to exceed the recursion limit'
except RecursionError:
    assert comparing['n'] == 1, 'the failed comparison consumed exactly one call'
cyclic_sentinel[0] = 'no longer self referential'
assert next(cyclic_iter) is cyclic_value, 'a comparison error does not exhaust the iterator'
assert next(cyclic_iter) is cyclic_value, 'and the iterator keeps working afterwards'
assert comparing['n'] == 3, 'one call per step across the comparison error'

# === Argument count errors ===
# Both forms take one or two positional arguments; the count errors are unchanged by the new form.
try:
    iter()
    assert False, 'expected iter() with no arguments to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter expected at least 1 argument, got 0', 'the zero-argument message is unchanged'
try:
    iter([1], 2, 3)
    assert False, 'expected iter() with three arguments to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter expected at most 2 arguments, got 3', 'the three-argument message is unchanged'

# === Laziness ===
# Construction calls the callable zero times, and each step calls it exactly once.
lazy = {'n': 0}


def lazy_step():
    lazy['n'] += 1
    return lazy['n'] if lazy['n'] < 4 else 0


lazy_iter = iter(lazy_step, 0)
assert lazy['n'] == 0, 'construction never calls the callable'
assert next(lazy_iter) == 1, 'the first step yields the first value'
assert lazy['n'] == 1, 'one step means one call'
assert next(lazy_iter) == 2, 'the second step yields the second value'
assert lazy['n'] == 2, 'two steps mean two calls'
drained = []
while True:
    try:
        drained.append(next(lazy_iter))
    except StopIteration:
        break
assert drained == [3], 'the last value before the sentinel is still yielded'
assert lazy['n'] == 4, 'draining a three-value run totals four calls'

# === Sticky exhaustion ===
# Once the sentinel has been produced the callable is never invoked again, however the exhausted
# iterator is driven.
sticky = {'n': 0}


def stop_after_two():
    sticky['n'] += 1
    return sticky['n'] if sticky['n'] < 3 else 'END'


sticky_iter = iter(stop_after_two, 'END')
assert next(sticky_iter) == 1, 'the first step yields 1'
assert next(sticky_iter) == 2, 'the second step yields 2'
try:
    next(sticky_iter)
    assert False, 'expected StopIteration when the sentinel is produced'
except StopIteration as exc:
    assert str(exc) == '', 'StopIteration carries no message'
assert sticky['n'] == 3, 'the sentinel probe is the third and final call'
try:
    next(sticky_iter)
    assert False, 'expected StopIteration again from an exhausted iterator'
except StopIteration as exc:
    assert str(exc) == '', 'the repeated StopIteration also carries no message'
assert sticky['n'] == 3, 'an exhausted iterator never calls the callable again'
assert next(sticky_iter, 'DEFAULT') == 'DEFAULT', 'next() returns its default once the iterator is exhausted'
assert sticky['n'] == 3, 'supplying a default makes no call either'
for value in sticky_iter:
    assert False, 'an exhausted iterator yields nothing in a for loop'
assert sticky['n'] == 3, 'driving an exhausted iterator with a for loop makes no call'

# === Comprehension form ===
# Comprehensions drive the iterator through the same path as a for statement.
comp = {'n': 0}


def comp_step():
    comp['n'] += 1
    return 'k' + str(comp['n']) if comp['n'] < 3 else 'STOP'


assert [v for v in iter(comp_step, 'STOP')] == ['k1', 'k2'], 'a list comprehension drives the iterator'
assert comp['n'] == 3, 'two yielded values plus one sentinel probe'
comp['n'] = 0
assert {v for v in iter(comp_step, 'STOP')} == {'k1', 'k2'}, 'a set comprehension drives the iterator'
comp['n'] = 0
assert {v: len(v) for v in iter(comp_step, 'STOP')} == {'k1': 2, 'k2': 2}, 'a dict comprehension drives the iterator'

# === while True plus next() ===
# Driving with next() by hand is the other supported shape, and a closure is a third distinct kind
# of callable value: it lives on the heap and carries the variables it captured.


def make_reader(values):
    box = {'i': 0}

    def reader():
        i = box['i']
        box['i'] = i + 1
        return values[i] if i < len(values) else 'EOF'

    return reader


reader_iter = iter(make_reader(['a', 'b', 'c']), 'EOF')
gathered = []
while True:
    try:
        gathered.append(next(reader_iter))
    except StopIteration:
        break
assert gathered == ['a', 'b', 'c'], 'a closure-driven iterator drains through while and next()'
assert next(reader_iter, 'SPENT') == 'SPENT', 'the closure-driven iterator stays exhausted'

# === Heap object sentinel ===
# The sentinel may be a heap object; it is compared by value on every step and never mutated.
heaped = {'n': 0}


def build_list():
    heaped['n'] += 1
    return [heaped['n']] if heaped['n'] < 3 else []


empty_sentinel = []
assert [v for v in iter(build_list, empty_sentinel)] == [[1], [2]], 'an empty-list sentinel stops by value'
assert heaped['n'] == 3, 'two yielded values plus one sentinel probe'
assert empty_sentinel == [], 'the sentinel object itself is untouched'

tupled = {'n': 0}


def build_tuple():
    tupled['n'] += 1
    return (tupled['n'],) if tupled['n'] < 2 else (0, 0)


assert [v for v in iter(build_tuple, (0, 0))] == [(1,)], 'a tuple sentinel built freshly each step stops by value'
assert tupled['n'] == 2, 'one yielded value plus one sentinel probe'

# === Callable kinds and the non-callable diagnostic ===
# Builtin types are callable values, so they are legitimate first arguments.
assert [v for v in iter(int, 0)] == [], 'int() produces 0, which equals the sentinel immediately'
assert [v for v in iter(list, [])] == [], 'list() produces [], which equals the sentinel immediately'
assert [v for v in iter(dict, {})] == [], 'dict() produces {}, which equals the sentinel immediately'
assert next(iter(int, 5)) == 0, 'a builtin type whose result is not the sentinel yields that result'
assert [v for v in iter(lambda: 7, 7)] == [], 'a lambda is callable'

# A non-callable first argument is rejected eagerly, when the iterator is constructed, rather than
# on the first step.
try:
    iter(5, 1)
    assert False, 'expected a non-callable first argument to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'the eager callability message matches CPython'
try:
    iter([1, 2], 3)
    assert False, 'expected a non-callable heap first argument to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'a heap value that is not callable is rejected too'
try:
    iter(None, None)
    assert False, 'expected None as first argument to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'None is not callable either'
