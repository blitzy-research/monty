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
# Each section releases the iterator through a different path: the ForIter
# exhaustion arm, a namespace rebind, and the eager callability check that
# rejects both arguments before any pair tuple exists.
#
# Lists are the heap-allocated values tracked here. Ints, string literals and
# plain module-level functions are immediates: they are not heap entries and so
# never appear in the reference counts.

# === Released by the ForIter exhaustion arm ===
# The iterator is deliberately never bound, so draining it in a for loop is the
# only thing that can free it: ForIter pops the exhausted iterator and drops it.
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
# next() advances the iterator through a different path than ForIter, and the
# iterator has to be bound to be driven that way, so it is released by rebinding
# the name to an immediate. A propagated exception must leave the iterator live,
# and exhaustion must be sticky, so no further value is ever produced.
attempts = [0]
retained = ['retained']


def raise_then_stop():
    attempts[0] = attempts[0] + 1
    if attempts[0] == 1:
        raise ValueError('boom')
    if attempts[0] > 2:
        return ['retained']
    return attempts[0]


probe = iter(raise_then_stop, retained)
try:
    next(probe)
    assert False, 'expected the callable to raise ValueError'
except ValueError as exc:
    assert str(exc) == 'boom', 'the exception from the callable propagates unchanged'
assert next(probe) == 2, 'a propagated exception must not exhaust the iterator'
assert next(probe, 'DEF') == 'DEF', 'the equal-by-value heap result exhausts the iterator'
assert next(probe, 'DEF') == 'DEF', 'exhaustion is sticky, so the default comes back again'
assert attempts == [3], 'the callable is never invoked again once the sentinel has been seen'
probe = None

# === Released by the eager callability check ===
# A non-callable first argument is rejected before any pair tuple is allocated,
# so init has to drop both arguments itself. A leak on that path would inflate
# both counts below, and an over-free would panic under ref-count-panic.
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
