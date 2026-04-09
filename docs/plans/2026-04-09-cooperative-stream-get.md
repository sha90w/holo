# Non-blocking Cooperative StreamGet

## Overview

StreamGet currently runs inline in the protocol event loop (`process_stream_get()` is awaited
inside `process_northbound_msg()`). When a gRPC client stops consuming (e.g., output piped into
a pager), the mpsc buffer fills, `tx.send().await` blocks, and the entire protocol event loop
freezes. This prevents keepalive processing, BGP updates, timer events, and other gRPC requests
from being handled. BGP sessions eventually drop due to hold timer expiry.

**Solution:** Cooperative streaming inside the event loop using `tokio::select!`. Instead of
iterating the entire list inline, we register an `ActiveStream` and advance it one entry at a
time between normal protocol messages. The event loop never blocks on client consumption.

**Key properties:**
- Universal: works for any protocol that marks lists as `STREAMABLE = true`
- No `Arc<RwLock<Instance>>`: Instance stays exclusively owned by the event loop
- No upfront data collection: entries are built one at a time from live data
- Natural backpressure: slow clients are simply skipped, not blocked on
- Multiple concurrent streams supported independently

## Context (from discovery)

**Files/components involved:**
- `holo-northbound/src/state.rs` — YangList trait, YangListOps, process_stream_get, iteration logic
- `holo-protocol/src/lib.rs` — generic event_loop<P>(), event_aggregator
- `holo-northbound/src/lib.rs` — process_northbound_msg dispatches StreamGet
- `holo-northbound/src/yang_codegen/mod.rs` — code generation for YangListOps tables
- `holo-bgp/src/northbound/state.rs` — BGP Provider impl, streamable list iter() implementations

**Data structures:**
- BGP prefixes: `PrefixMap` (trie from `prefix-trie` crate) — ordered
- Attribute sets: `BTreeMap` — ordered, supports range()
- Communities: `BTreeSet` — ordered, supports range()
- All ordered structures support efficient cursor-based iteration

**Rust ownership constraint:** Iterators from `(list_ops.iter)(provider, &list_entry)` borrow
`&Instance`. The event loop needs `&mut Instance` for normal message processing. These borrows
are mutually exclusive. Solution: cursor-based iteration — create iterator, get one entry, save
position as YANG keys string, drop iterator, process messages, resume from cursor.

**Related:** Previous plan `2026-04-03-grpc-stream-get.md` implemented the initial StreamGet
(inline blocking). Its "Future Hardening" section identified this exact problem.

## Development Approach
- **testing approach**: Regular (code first, then tests)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes during implementation**
- run `cargo build` and `cargo test` after each change
- maintain backward compatibility — existing `process_stream_get` remains available

## Testing Strategy
- **unit tests**: test cursor-based iteration, stream advancement, disconnection detection
- **integration tests**: test cooperative streaming with slow/fast consumers
- no e2e/UI tests (CLI tool, no UI)

## Progress Tracking
- mark completed items with `[x]` immediately when done
- add newly discovered tasks with + prefix
- document issues/blockers with ! prefix
- update plan if implementation deviates from original scope

## Implementation Steps

### Task 1: Add `iter_from` cursor support to YangList trait and YangListOps

**Files:**
- Modify: `holo-northbound/src/state.rs`

- [ ] add `YangListIterFromFn<P>` type alias: `for<'a> fn(&'a P, &P::ListEntry<'a>, &str) -> Option<ListIterator<'a, P>>`
- [ ] add `iter_from` field to `YangListOps<P>` struct (alongside existing `iter`, `new`, `streamable`)
- [ ] add default `iter_from` method to `YangList<'a, P>` trait — iterates using `Self::iter()`, calls `Self::new()` + `list_keys()` on each entry, skips until cursor key is found, returns remaining iterator
- [ ] verify `cargo build` succeeds (will fail until codegen is updated in Task 2)

### Task 2: Update codegen to wire `iter_from` into YangListOps

**Files:**
- Modify: `holo-northbound/src/yang_codegen/mod.rs`

- [ ] update the `YangListOps` generation to include `iter_from: |p, le, c| {name}::iter_from(p, le, c)` field
- [ ] run `cargo build` to verify all generated YangListOps compile with the new field
- [ ] write tests for default `iter_from` behavior (skip_while fallback with a simple test provider)
- [ ] run tests — must pass before next task

### Task 3: Add `ActiveStream` type and `setup_stream_get` function

**Files:**
- Modify: `holo-northbound/src/state.rs`
- Modify: `holo-northbound/src/lib.rs`

- [ ] define `ActiveStream` struct: `path: String`, `snode_path: String`, `parent_dnode_path: String`, `cursor: Option<String>`, `filter: GetFilter`, `tx: mpsc::Sender<DataTree<'static>>`, `done: bool`
- [ ] implement `setup_stream_get<P>()` function — validates path, checks streamable, creates mpsc::channel(1), spawns `stream_forwarder` task, pushes `ActiveStream` to vec, returns immediately
- [ ] implement `stream_forwarder` async function — bridges `rx` to `grpc_tx`, exits on disconnect
- [ ] add `active_streams: &mut Vec<ActiveStream>` parameter to `process_northbound_msg` — pass it through for StreamGet, call `setup_stream_get` instead of `process_stream_get`
- [ ] write tests for `setup_stream_get` (stream registration, forwarder spawning)
- [ ] run tests — must pass before next task

### Task 4: Add `advance_streams` function

**Files:**
- Modify: `holo-northbound/src/state.rs`

- [ ] implement `advance_streams<P: Provider>(provider: &P, streams: &mut Vec<ActiveStream>)` — sync function, no `.await`
- [ ] for each stream: check `tx.try_reserve()` — if buffer full or closed, skip or mark done
- [ ] reconstruct parent `ListEntry` via `lookup_list_entry` (or equivalent from stored path)
- [ ] get iterator via `iter` (cursor=None) or `iter_from` (cursor=Some) from `YangListOps`
- [ ] call `.next()` on iterator, build DataTree for the entry (path + keys + `into_data_node` + `iterate_children`)
- [ ] update `stream.cursor` with `obj.list_keys()`, send via permit
- [ ] handle child provider relay for entries that delegate to child tasks
- [ ] mark `stream.done = true` when iterator returns None or tx is closed
- [ ] write tests for `advance_streams` (single entry advancement, cursor progression, done detection, buffer-full skip)
- [ ] run tests — must pass before next task

### Task 5: Refactor event loop to use `select!` with stream advancement

**Files:**
- Modify: `holo-protocol/src/lib.rs`

- [ ] add `active_streams: Vec<ActiveStream>` local variable in `event_loop<P>()`
- [ ] add `streams_ready` async function — uses `futures::future::select_all` on `tx.reserve()` for all active streams, stays pending when all buffers full
- [ ] change main loop from `agg_channels.rx.recv().await` to `tokio::select! { biased; ... }`
- [ ] first branch (biased priority): `msg = agg_channels.rx.recv()` — existing message processing, pass `&mut active_streams` to `process_northbound_msg`
- [ ] second branch: `_ = streams_ready(&active_streams), if !active_streams.is_empty()` — call `advance_streams(instance, &mut active_streams)`, then `active_streams.retain(|s| !s.done)`
- [ ] add necessary imports (`tokio::select`, `futures`, ActiveStream, etc.)
- [ ] run `cargo build` to verify compilation
- [ ] write tests for event loop behavior (stream advancement interleaved with messages)
- [ ] run tests — must pass before next task

### Task 6: Override `iter_from` for BGP streamable lists with efficient cursors

**Files:**
- Modify: `holo-bgp/src/northbound/state.rs`

- [ ] implement `iter_from` for IPv4 loc-rib routes — parse prefix key from cursor string, use PrefixMap ordered iteration to skip past cursor
- [ ] implement `iter_from` for IPv6 loc-rib routes — same pattern with Ipv6Network
- [ ] implement `iter_from` for attr_sets (BTreeMap) — parse index from cursor, use `range((Excluded(key), Unbounded))`
- [ ] implement `iter_from` for community lists (BTreeSet) — similar range-based cursor
- [ ] implement `iter_from` for adj-rib route variants (in/out/pre/post for both v4 and v6)
- [ ] write tests for each `iter_from` override (cursor advancement, missing cursor handling, empty collection)
- [ ] run tests — must pass before next task

### Task 7: Verify acceptance criteria

- [ ] verify StreamGet with slow client (pager) does not block protocol event loop
- [ ] verify BGP keepalives continue processing during active StreamGet
- [ ] verify concurrent StreamGet requests work independently
- [ ] verify client disconnection is detected and stream cleaned up
- [ ] verify normal Get RPC still works unchanged
- [ ] verify child provider relay still works for cross-protocol lists
- [ ] run full test suite: `cargo test`
- [ ] run `cargo clippy` for lint check

### Task 8: [Final] Update documentation

- [ ] update `docs/plans/2026-04-03-grpc-stream-get.md` "Future Hardening" section — mark stream timeout issue as addressed
- [ ] move this plan to `docs/plans/completed/`

## Technical Details

### Data flow (new)

```
gRPC client ← stream_forwarder task ← mpsc(1) ← event loop advance_streams()
                                                        ↑
                                                   select! {
                                                     agg_rx.recv() → process_msg(&mut instance)
                                                     streams_ready() → advance_streams(&instance)
                                                   }
```

### Cursor mechanism

Each `ActiveStream` stores `cursor: Option<String>` — the YANG list keys string from the last
iterated entry (e.g., `[prefix='10.0.0.0/24'][path-id='0']`).

- `cursor = None` → start from beginning, use `(list_ops.iter)(provider, &list_entry)`
- `cursor = Some(keys)` → resume, use `(list_ops.iter_from)(provider, &list_entry, &keys)`

Default `iter_from`: iterates with `iter()`, calls `new()` + `list_keys()` on each entry,
skips until cursor found, returns remaining iterator. O(N) per call.

Optimized `iter_from` for ordered collections: O(log N) via `range()` / trie skip.

### Backpressure chain

```
gRPC client pauses → grpc_tx.send() blocks in forwarder →
forwarder stops consuming from rx → mpsc(1) buffer full →
tx.try_reserve() fails in advance_streams → stream skipped →
event loop continues processing protocol messages
```

### Client disconnection detection

```
gRPC client drops → grpc_tx.send() returns Err → forwarder exits →
rx dropped → tx.try_reserve() returns Err → stream.done = true → removed
```

### `advance_streams` is sync

`advance_streams()` is NOT async. It borrows `&instance` (immutable) for the brief time to:
1. Reconstruct list entry context
2. Create iterator from cursor
3. Call `.next()` once
4. Build one DataTree
5. Send via `permit.send()` (non-blocking, permit already reserved)

Then the borrow is released. The next `select!` iteration can take `&mut instance` for
normal message processing. No borrow conflict.

## Post-Completion

**Manual verification:**
- Run holo-daemon with BGP and a populated RIB (100K+ routes)
- `holo-cli | head -20` on StreamGet — verify daemon stays responsive
- Kill holo-cli mid-stream — verify stream cleanup and no resource leak
- Run two concurrent StreamGet requests — verify both complete
- Monitor BGP session keepalives during streaming — verify no drops

**Future optimization:**
- Stream timeout for permanently stalled clients
- Per-list `iter_from` optimizations for protocols beyond BGP
- HTTP/2 window tuning for high-throughput streaming
