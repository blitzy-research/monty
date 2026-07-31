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

# The same holds when a for loop is the driver, and that route matters on its own: a for statement
# reaches the iterator through a different opcode than next() does, and the callable below is a
# plain function, so invoking it pushes a frame and runs a nested interpreter loop. The enclosing
# try must still find its handler once that frame has been popped, so the flag below is asserted
# rather than the assertion being left to `except`. The iterator is bound to a name so the step
# after the error can be driven by hand: a for-driven error must leave it usable, exactly as a
# next()-driven one does.
looping = {'n': 0}


def raise_on_second():
    looping['n'] += 1
    if looping['n'] == 2:
        raise ValueError('kaboom')
    return looping['n'] if looping['n'] < 6 else 0


loop_iter = iter(raise_on_second, 0)
collected = []
handled = False
try:
    for value in loop_iter:
        collected.append(value)
    assert False, 'expected the for loop to propagate ValueError'
except ValueError as exc:
    handled = True
    assert str(exc) == 'kaboom', 'the callable exception propagates out of the for loop'
    assert exc.args == ('kaboom',), 'the exception arguments survive the loop unchanged'
assert handled, 'the handler enclosing the for loop must run'
assert collected == [1], 'values yielded before the exception are unaffected'
assert looping['n'] == 2, 'the loop made exactly one call per step'
assert next(loop_iter) == 3, 'a for-driven exception does not exhaust the iterator either'
assert looping['n'] == 3, 'the step after the loop is a single further call'
for value in loop_iter:
    collected.append(value)
assert collected == [1, 4, 5], 'the same iterator keeps driving in a later for loop, up to the sentinel'
assert looping['n'] == 6, 'the second loop yielded two values plus one sentinel probe'

# An exception on the very FIRST step of a for loop is the sharpest form of the same guarantee:
# nothing inside the try has run yet, so the loop statement itself is the only thing that can have
# recorded where execution is. The next() call below deliberately pins the last recorded position
# outside the try, so if the loop fails to record its own position around the nested call, the
# handler search starts from that outside offset and this ValueError escapes uncaught.
sharp = {'n': 0}


def raise_at_once():
    sharp['n'] += 1
    raise ValueError('first step')


sharp_iter = iter(raise_at_once, 0)
pinned = next(iter(int, 5))
assert pinned == 0, 'the position-pinning call sits outside the try below'
caught_sharp = False
try:
    for value in sharp_iter:
        assert False, 'the callable raises before any value can be yielded'
except ValueError as exc:
    caught_sharp = True
    assert str(exc) == 'first step', 'the first-step exception reaches the handler unchanged'
assert caught_sharp, 'the handler must run for an exception raised on the first step of a for loop'
assert sharp['n'] == 1, 'exactly one call was made before the exception'

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
# The eager check accepts exactly what the interpreter can call, and every kind reachable from
# Python source is driven somewhere in this file: a plain function (the drive-until-sentinel and
# exception sections), a function carrying defaults (the `grow` section), a closure (the reader in
# the while/next section), a lambda and a builtin type (here). Two accepted kinds are deliberately
# NOT covered, because neither is expressible in a test case rather than because they are untested
# by oversight: module functions are not first-class values in monty at all (`os.getcwd` raises
# AttributeError before `iter()` could ever see it), and external functions are accepted by the
# check yet cannot complete a call from inside an iterator advance - a limitation inherited
# verbatim from map(), filter() and sorted(), and out of scope for this feature.
assert [v for v in iter(int, 0)] == [], 'int() produces 0, which equals the sentinel immediately'
assert [v for v in iter(list, [])] == [], 'list() produces [], which equals the sentinel immediately'
assert [v for v in iter(dict, {})] == [], 'dict() produces {}, which equals the sentinel immediately'
assert next(iter(int, 5)) == 0, 'a builtin type whose result is not the sentinel yields that result'
assert [v for v in iter(lambda: 7, 7)] == [], 'a lambda is callable'
builtin_function_iter = iter(len, 5)
assert iter(builtin_function_iter) is builtin_function_iter, 'a builtin function is accepted as the callable'

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
