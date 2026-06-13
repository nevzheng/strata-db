# filesystem

strata-db's **storage foundation** — pure storage *mechanism* (store and fetch
bytes), never query *policy*. Everything between the backing file and the rest
of the engine lives here.

The guiding rule: this crate is handed an address and moves bytes. It never
decides *what* to fetch or *why* — no keys, versions, indexes, or query intent.
Those are the engine's job. The day something here starts making query-shaped
decisions, it has leaked down a layer.

## What you reach for

The crate is organized as a small set of capability modules. Pick the one that
matches what you're doing:

| You are… | Module | What you get |
|---|---|---|
| an **execution engine** | [`memory`] | `MemoryPool` hands out `Slab`s (raw byte spans) under one global cap — ask for a `Slab`, impose a view (scratch, a hash table, a heap), drop it when done |
| the **storage engine** | [`tuple`] | the `Heap` access method (records on pages) + the `TupleLoc` an index stores — open it with `Heap::open`, which owns its block store and page cache internally |
| an **index / format owner** | [`codec`] | the `Encode`/`Decode` vocabulary — reach for it *when* you serialize an on-disk format |
| …wiring the above | [`cache`], [`block`], [`page`] | the read-through `Cache` + `PageCache` buffer pool, the `BlockStore` device + `BlockJournal`, and the typed page format. Plumbing the first three sit on. |

## Quickstart

**Scratch memory** (the execution engine):

```rust
use filesystem::{MemoryPool, Slab};

let pool = MemoryPool::new(64 << 20); // 64 MiB global cap
let mut scratch: Slab = pool.allocate(1 << 20)?; // 1 MiB
scratch.as_mut_slice().fill(0);
// …impose a hash table / sort buffer over `scratch`, use it…
drop(scratch); // bytes return to the pool's budget
```

**A tuple heap** (the storage engine):

```rust
use filesystem::Heap;

let heap = Heap::open(dir, 1024)?; // dir/tuples.db + dir/tuples.journal, 1024-frame pool
let loc = heap.insert(b"row bytes")?; // -> TupleLoc; an index records key -> loc
let view = heap.get(loc)?;            // zero-copy borrow of the tuple's bytes
```

**A read-through cache** (an index):

```rust
use filesystem::{Cache, Budget};
use filesystem::policies::Lru;

let cache: Cache<u64, MyValue, Lru<u64>> = Cache::new(Budget::Bytes(8 << 20), Lru::new());
let v = cache.get_or_load(key, || load_from_disk(key))?; // owned, read-only handle
```

## Layering

```
MemoryPool ──hands out──▶ Slab            (raw memory; consumers impose structure)

backing file ──▶ BlockStore (File/Mem) ──▶ Page          (typed, self-describing)
                      ▲                         ▲
                 BlockJournal               PageCache (buffer pool) ──▶ Heap (tuples)
                                            Cache (read-through memo) ──▶ the LSM index
```

`MemoryPool`/`Slab` are the memory primitive (once sketched as `Buffer`); wiring
the caches onto the pool, and a `ScanBuffer` adapter, are the next steps.

The v1 caches are single-threaded (`Rc`/`RefCell`); concurrency is deferred.

[`memory`]: src/memory.rs
[`tuple`]: src/tuple/
[`codec`]: src/codec.rs
[`cache`]: src/cache/
[`block`]: src/block/
[`page`]: src/page/
