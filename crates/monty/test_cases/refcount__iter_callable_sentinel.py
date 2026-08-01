# Tests reference counting for the two-argument iter(callable, sentinel) form.
#
# iter(f, s) takes ownership of BOTH arguments by moving them into an internal
# two-element (callable, sentinel) pair tuple and storing that tuple's reference as
# the iterator's single owned value; IterValue::CallableSentinel keeps only a
# non-owning mirror of the tuple's heap id. A live callable-sentinel iterator
# therefore occupies TWO heap entries - the iterator and its pair tuple - while only
# the iterator can ever be bound to a Python name.
#
# All heap objects must be directly referenced by variables for strict matching, so
# every iterator built below is fully released before the module ends. Releasing one
# cascades through its pair tuple to the callable and the sentinel, so strict matching
# passing proves the pair tuple was freed rather than orphaned, and each heap sentinel
# sitting back at its binding-only count of 1 proves neither owned value leaked.
#
# Both stop conditions are covered, because they release different things: sentinel
# equality has a produced value to drop before it reports exhaustion, while a callable-
# raised StopIteration ends the drive with no value produced at all.
#
# Lists are the heap-allocated values tracked here. Ints, string literals and plain
# module-level functions are immediates: they are not heap entries and so never appear
# in the reference counts.

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
# The iterator is bound so the same object can be advanced repeatedly, which is why
# releasing it takes an explicit rebind of the name to an immediate. A propagated
# exception leaves the iterator live and releases the clones the failed step took, so
# the equal-by-value heap result on the step after it is what exhausts the iterator
# here, and once the sentinel has been seen exhaustion is sticky.
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
assert next(probe) == 2, 'the propagated exception did not exhaust the iterator'
assert next(probe, 'DEF') == 'DEF', 'the equal-by-value heap result exhausts the iterator'
assert next(probe, 'DEF') == 'DEF', 'exhaustion is sticky, so the default comes back again'
assert attempts == [3], 'the callable is never invoked again once the sentinel has been seen'
probe = None

# === Released after a callable-raised StopIteration reads as exhaustion ===
# A StopIteration raised BY the callable is the second way iteration stops, and it is
# the only stop that produces no value at all: the step has to release the clones it
# took of the callable and the sentinel with nothing to hand back, and the swallowed
# exception must not carry a heap reference away with it. The sentinel here is a heap
# object that is never compared equal, so the raise - not equality - is what stopped
# the drive, and the value yielded before it is a heap list the consumer must release.
stops = [0]
unseen = ['never equal']


def value_then_stop():
    stops[0] = stops[0] + 1
    if stops[0] == 1:
        return ['produced']
    raise StopIteration('read as exhaustion')


stopper = iter(value_then_stop, unseen)
assert next(stopper) == ['produced'], 'the step before the raise yields its heap value'
assert next(stopper, 'DEF') == 'DEF', 'a callable-raised StopIteration is read as exhaustion'
assert next(stopper, 'DEF') == 'DEF', 'that exhaustion is sticky like sentinel equality'
assert stops == [2], 'the raising step is the last invocation of the callable'
stopper = None

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
# else, so every count is 1: every iterator, its pair tuple, and every value produced
# at a step that stopped or failed have been released.
# ref-counts={'calls': 1, 'sentinel': 1, 'collected': 1, 'attempts': 1, 'retained': 1, 'stops': 1, 'unseen': 1, 'uncallable': 1, 'dropped': 1}
