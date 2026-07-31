# === iter(callable, sentinel) survives garbage collection during an advance ===
# Advancing a callable-driven iterator re-enters the interpreter, which makes it the only kind of
# iterator whose advance can reach a collection point. The collector marks from the operand stack,
# globals and the exception stack and frees everything else, consulting neither reference counts nor
# active readers, so anything reachable only from the runtime's own call stack is invisible to it -
# and `next()` pops its argument before running, so an iterator that is never bound to a name,
# together with the callable and sentinel it owns, is exactly that. The step therefore suspends
# collection for its duration; the deferred collection runs at the next instruction boundary outside
# it. Every section below deliberately allocates enough reference-holding containers inside the
# callable to make a collection due part-way through a step, so that removing the suspension turns
# each one back into a use-after-free rather than a silent pass.

# `calls` counts invocations so laziness can be asserted; `churn_on` selects which step allocates
# heavily, so a collection can be forced on the first step or on a later one.
state = {'calls': 0, 'churn_on': 0}


def churn():
    # Nested lists hold references, so these allocations are cycle-tracked and count towards the
    # collector's threshold. The total is returned so the loop cannot be treated as dead, and every
    # caller asserts it, which stops the fixture from silently degrading into a no-op if the loop
    # ever stops allocating.
    total = 0
    for _ in range(12000):
        pair = [[1], [2], [3], [4], [5], [6], [7], [8], [9], [10]]
        total += len(pair)
    return total


def step():
    # Records the call, then allocates heavily on exactly the selected step.
    state['calls'] += 1
    if state['calls'] == state['churn_on']:
        assert churn() == 120000, 'the churn helper must allocate on every iteration'
    return state['calls']


def int_stepper():
    return 0 if step() > 3 else state['calls']


def list_stepper():
    return [] if step() > 2 else [state['calls']]


def name_stepper():
    return 'STOP' if step() > 2 else 'k' + str(state['calls'])


def int_or_stop():
    # Yields plain integers, so a caller that accumulates outside the interpreter accumulates
    # nothing collectable, then a freshly built list that stops the iterator by value. Used where
    # the point is to isolate the survival of the iterator and its own sentinel.
    step()
    return state['calls'] if state['calls'] <= 3 else ['STOP']


def text_stepper():
    # Yields freshly built strings, so each value is a heap object that only the consumer's own
    # accumulator references once it has been handed over. Used where the point is that those
    # accumulated values survive a collection reached later in the same drive.
    step()
    return 'k' + str(state['calls']) if state['calls'] <= 5 else 'STOP'


# === Unbound temporary iterator ===
# Nothing on the operand stack references this iterator while its callable runs, so it is reachable
# from the runtime's Rust locals alone.
state['calls'] = 0
state['churn_on'] = 1
assert next(iter(int_stepper, 0)) == 1, 'a temporary iterator survives a collection during its first step'
assert state['calls'] == 1, 'the callable is invoked exactly once per step'

# === Collection part-way through a drive, with a heap sentinel ===
# Step state is written back through the iterator once the nested call returns, so the iterator
# itself - not merely the values it owns - has to survive. The sentinel is a heap object compared by
# value, so the collector has to keep reaching it and the callable while the callable is running.
state['calls'] = 0
state['churn_on'] = 2
lists = iter(list_stepper, [])
gathered = []
while True:
    try:
        gathered.append(next(lists))
    except StopIteration:
        break
assert gathered == [[1], [2]], 'iterator state, callable and heap sentinel all survive a collection'
assert state['calls'] == 3, 'two yielded values plus one sentinel probe'
assert next(lists, 'DEFAULT') == 'DEFAULT', 'exhaustion is still sticky after a collection'
assert state['calls'] == 3, 'an exhausted iterator never calls back'

# === Driven by a dict-view operator ===
# Dict-view set operations consume iterators directly from Rust. A binary operator has already popped
# both of its operands, so the iterator it is driving lives in a Rust local and nothing on the value
# stack references it - and the sentinel it owns is a list that only the iterator references. Both
# have to survive a collection reached part-way through the drive.
counts = {1: 'x'}
keys = counts.keys()
state['calls'] = 0
state['churn_on'] = 2
assert sorted(keys | iter(int_or_stop, ['STOP'])) == [1, 2, 3], (
    'a dict-view union keeps driving a temporary iterator, and its heap sentinel, across a collection'
)
assert state['calls'] == 4, 'three yielded values plus one sentinel probe'

# The same drive, but the yielded values are freshly built strings rather than plain integers, so the
# temporary `set` the drive accumulates in Rust is itself full of heap objects that nothing else
# references. Values handed to a consumer are the third graph a step has to protect, alongside the
# iterator entry and the `next()` default, and this is the only one that lives in another module
# entirely - which is why the step suspends collection rather than publishing roots: a set living in
# the runtime's call stack cannot be enumerated from the iterator at all. Losing that protection is
# not a leak but a use-after-free, so this assertion is load-bearing.
#
# The hazard is a property of the pattern rather than of this feature: every builtin that keeps heap
# values in a runtime-Rust local across a call back into the interpreter shares it - `map`, `filter`,
# `sorted`, `min`, `max` and `list.sort` included - and those paths are unchanged here.
letters = {'z': 1}
letter_keys = letters.keys()
state['calls'] = 0
state['churn_on'] = 3
assert sorted(letter_keys | iter(text_stepper, 'STOP')) == ['k1', 'k2', 'k3', 'k4', 'k5', 'z'], (
    'every heap value the drive already accumulated in Rust survives a collection mid-drive'
)
assert state['calls'] == 6, 'five yielded values plus one sentinel probe'

# `isdisjoint` reaches the same advance path from a different caller; kept collection-free so the
# fixture stays quick while still covering that entry point.
state['calls'] = 0
state['churn_on'] = 0
assert {'a': 1}.keys().isdisjoint(iter(name_stepper, 'STOP')) is True, (
    'isdisjoint drives the iterator through the same advance path'
)
assert state['calls'] == 3, 'two probed keys plus one sentinel probe'


# === The default handed to next() survives too ===
# When the terminal step is the one that collects, the value `next()` will return is a heap object
# reachable only from the runtime's own locals for the duration of that step - so it has to survive
# as well, and it has to come back as the very same object rather than a copy.
def stop_stepper():
    # Stops on its first call, so a temporary iterator's only step is the terminal one.
    step()
    return []


state['calls'] = 0
state['churn_on'] = 1
assert next(iter(stop_stepper, []), ['FALLBACK']) == ['FALLBACK'], (
    'a default built inline survives a collection during the terminal step'
)
assert state['calls'] == 1, 'the terminal step is a single call'

state['calls'] = 0
state['churn_on'] = 1
holder = ['DEFAULT']
assert next(iter(stop_stepper, []), holder) is holder, 'the exact default object is returned after a collection'
assert holder == ['DEFAULT'], 'and it is handed back unchanged'
assert state['calls'] == 1, 'the terminal step is still a single call'
