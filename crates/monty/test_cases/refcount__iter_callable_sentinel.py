# Tests reference counting for the two-argument iter(callable, sentinel) form.
#
# iter(f, s) takes ownership of BOTH arguments by moving them into an internal
# two-element (callable, sentinel) pair tuple and storing that tuple's reference
# as the iterator's single owned value; IterValue::CallableSentinel keeps only a
# non-owning mirror of the tuple's heap id. That indirection is what lets the
# collector reach both owned values through the pre-existing HeapData::Iter and
# HeapData::Tuple traversal arms without touching heap.rs.
#
# A live callable-sentinel iterator therefore occupies TWO heap entries - the
# iterator and its pair tuple - while only the iterator can ever be bound to a
# Python name. All heap objects must be directly referenced by variables for
# strict matching, so every iterator below is fully released before the module
# ends. Releasing one cascades through its pair tuple to the callable and the
# sentinel, so strict matching passing proves the pair tuple was freed rather
# than orphaned, and each heap sentinel sitting back at its binding-only count
# of 1 proves neither owned value leaked.
#
# The three sections cover the release paths that this ownership model turns on:
# normal ForIter exhaustion of an unbound temporary, an explicit rebind of a
# retained iterator after repeated next() calls, and the eager callability check,
# which rejects both arguments before any pair tuple or iterator exists and so
# has no iterator to release at all.
#
# Lists are the heap-allocated values tracked here. Ints, string literals and
# plain module-level functions are immediates: they are not heap entries and so
# never appear in the reference counts.
#
# The trailing reference-count directive also makes the CPython runner skip this
# file, which is why the second section is where the one deliberate divergence
# from CPython - a callable-raised StopIteration propagating rather than being
# converted into exhaustion - is asserted. It could not live in the sibling
# iter__callable_sentinel.py fixture, which runs under both interpreters.

# === Released by the ForIter exhaustion arm ===
# The iterator is deliberately never bound, so the for loop is its only owner,
# holding it on the operand stack; draining it to exhaustion is what releases it
# here, because the ForIter exhaustion arm pops the iterator and drops it.
# The stopping value is a freshly allocated list equal to - but not the same
# object as - the sentinel, so the step has to drop it instead of yielding it.
calls = [0]
sentinel = ['stop']
collected = []


def drive():
    calls[0] = calls[0] + 1
    if calls[0] > 2:
        return ['stop']
    return calls[0]


for produced in iter(drive, sentinel):
    collected.append(produced)

assert collected == [1, 2], 'iteration stops on the equal-by-value sentinel without yielding it'
assert calls == [3], 'two yields plus one sentinel probe invoke the callable three times'

# === Released by rebinding the name after a next() driven drive ===
# next() advances the iterator through a different path than ForIter, and needs
# no binding to do it - next(iter(f, s)) is valid. The iterator is bound here so
# that the same object can be advanced repeatedly, which is why releasing it
# takes an explicit rebind of the name to an immediate. Sentinel equality is the
# only thing that exhausts the iterator, so a propagated exception must leave it
# live whatever that exception is, and once the sentinel has been seen exhaustion
# must be sticky, so no further value is ever produced.
attempts = [0]
retained = ['retained']


def raise_then_stop():
    attempts[0] = attempts[0] + 1
    if attempts[0] == 1:
        raise ValueError('boom')
    if attempts[0] == 2:
        raise StopIteration('not exhaustion')
    if attempts[0] > 3:
        return ['retained']
    return attempts[0]


probe = iter(raise_then_stop, retained)
try:
    next(probe)
    assert False, 'expected the callable to raise ValueError'
except ValueError as exc:
    assert str(exc) == 'boom', 'the exception from the callable propagates unchanged'
# Transparency is unconditional, so StopIteration gets no special treatment: it
# arrives with its own message rather than as an empty exhaustion signal, and it
# leaves the iterator usable like any other exception. CPython's calliter_iternext
# clears such a StopIteration and exhausts the iterator instead, which makes this
# the one assertion in the feature the two interpreters disagree on - so it lives
# in this ref-counts fixture, which the CPython runner skips, rather than in
# iter__callable_sentinel.py, which runs under both.
try:
    next(probe)
    assert False, 'expected the callable-raised StopIteration to propagate'
except StopIteration as exc:
    assert str(exc) == 'not exhaustion', 'a callable-raised StopIteration keeps its own message'
assert next(probe) == 3, 'neither propagated exception exhausted the iterator'
assert next(probe, 'DEF') == 'DEF', 'the equal-by-value heap result exhausts the iterator'
assert next(probe, 'DEF') == 'DEF', 'exhaustion is sticky, so the default comes back again'
assert attempts == [4], 'the callable is never invoked again once the sentinel has been seen'
probe = None

# === Both arguments released by the eager callability check ===
# A non-callable first argument is rejected before any pair tuple is allocated,
# so no iterator is ever built here and init has to drop both arguments itself.
# A leak on that path would inflate both counts below, and an over-free would
# panic under ref-count-panic.
uncallable = [1, 2]
dropped = ['dropped']

try:
    iter(uncallable, dropped)
    assert False, 'expected iter(non_callable, sentinel) to raise TypeError'
except TypeError as exc:
    assert str(exc) == 'iter(v, w): v must be callable', 'the eager check reports the CPython message'

# Every heap object still alive is bound to exactly one name and held by nothing
# else, so every count is 1: both iterators, their pair tuples and every value
# produced at a stopping step have been released.
# ref-counts={'calls': 1, 'sentinel': 1, 'collected': 1, 'attempts': 1, 'retained': 1, 'uncallable': 1, 'dropped': 1}
