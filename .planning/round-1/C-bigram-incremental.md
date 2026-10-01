# C — Incremental Bigram Index Update

## 1. Problem Statement

`run_post_scan` (`scan.rs:306`) calls `build_bigram_index` on every rescan,
rebuilding the full inverted index from scratch over all indexable files. On the
user's 690 K-file repo the build dominates post-scan time at ~77 s. Rescans are
triggered repeatedly by the watcher overflow loop described in axis B; each
rescan wastes 77 s rebuilding an index where the vast majority of content has
not changed. The watcher already maintains a per-file `handle_create_or_modify`
path, but that path has no way to feed new content into the base index — only the
`BigramOverlay` (which cannot shed its entries) is updated.

## 2. Code References

| Location | Role |
|---|---|
| `bigram_filter.rs:286–299` | `BigramFilter` struct: `lookup: Vec<u16>` (65536 entries, bigram key → dense column index), `dense_data: Vec<u64>` (flat bitset slab, stride = `words`), `words = ceil(file_count/64)`, `skip_index: Option<Box<BigramFilter>>` |
| `bigram_filter.rs:525–593` | `BigramOverlay`: `modified: AHashMap<usize, Vec<u16>>`, `tombstones: Vec<u64>` — already in use; updated by the watcher on every file event |
| `bigram_filter.rs:634–707` | `build_bigram_index`: full parallel rebuild via rayon + `BigramIndexBuilder`, then `compress()` |
| `bigram_filter.rs:210–278` | `compress()`: density filtering during compression — any column whose popcount < `words*2` OR >90% populated is dropped |
| `scan.rs:304–313` | `run_post_scan`: calls `build_bigram_index`, then holds writer lock while installing via `set_bigram_index` |
| `file_picker.rs:1223–1228` | `set_bigram_index`: stores `Arc<BigramFilter>`, creates fresh `BigramOverlay` |
| `file_picker.rs:1378–1439` | `handle_create_or_modify`: updates `FileItem` metadata and calls `overlay.write().modify_file(pos, &content)` |
| `file_picker.rs:1485–1503` | `tombstone_file`: calls `overlay.write().delete_file(index)` |
| `grep.rs:1855–2075` | `grep_search`/`multi_grep_search`: acquires `bigram_overlay.read()` inside `FilePicker::grep`, passes both base index and overlay read-guard to query engine; uses tombstones to mask deleted files and `query_modified`/`modified_indices` to add back changed files |
| `lib.rs:146` | `pub use bigram_filter::*` — full module is re-exported; any new public API becomes part of the crate surface |

## 3. Root Cause Analysis

The `BigramFilter` layout is a **compressed dense bitset matrix**: `lookup[bigram_key]` maps to a column index `c`; the posting list for that bigram is `dense_data[c*words .. (c+1)*words]`, a 64-bit-word bitset with one bit per file. This layout is ideal for parallel AND-queries (contiguous SIMD-friendly bitset\_and) but structurally hostile to in-place mutation:

1. **File IDs are positional.** Bit `i` in every column corresponds to file index `i` in the base `Vec<FileItem>`. Inserting a new file at the end of the base region would require extending every column by 1 bit (potentially touching all `dense_data`). Removing a file from the middle would require renumbering all higher file IDs — touching all columns again.

2. **Compression discards sparse columns.** `compress()` silently drops bigrams that appear in fewer than ~3% of files. A single modified file can change a bigram's density enough to cross the threshold, invalidating the compression decision.

3. **The skip-index is a second independent `BigramFilter`.** Any mutation must be mirrored across both, doubling complexity.

4. **The overlay already handles the incremental case correctly.** `BigramOverlay` is a sparse side-structure (AHashMap + tombstone bitset) that is already consulted at query time in `grep.rs:2046–2067`. It correctly handles add/modify (via `query_modified`) and delete (via tombstones). The problem is not that there is no incremental path — it is that the overlay **is never merged back** into the base index and **grows without bound** across rescans. The base index is only reset via a full rebuild.

The correct fix is therefore not to redesign the `BigramFilter` for in-place mutation, but to **amortize and rate-limit the full rebuild**, relying on the existing overlay correctness in the interim.

## 4. Proposed Change — Overlay-Driven Lazy Rebuild

**Pick option (b): generational index with delta overlay, periodic merge.**

True in-place mutation of the dense bitset matrix is not viable:

- Adding a file at position N is safe (extend `words` by 0 extra bits only when `N % 64 == 0`); but in practice `build_bigram_index` is given `indexable_files = &files[..indexable_count]` with a hard count baked in at build time. Extending means reallocating every column.
- Removing or replacing a file requires zeroing/re-setting one bit per column — 5000 × `words` write operations (up to 5000 × 10,781 = 54 M writes for a 690 K file repo). At 1 ns/write that is 54 ms of sequential work just to clear.
- The compression threshold adds a correctness hazard: a file deletion could now make a previously-dropped column significant.

The overlay approach already works at query time. The only missing pieces are:

1. A **merge trigger**: promote overlay into a new base index when the overlay grows large enough.
2. A **cooldown guard** to prevent a merge from running while a rescan is already in flight (this is the axis B domain, but axis C must not break it).

### Merge Cadence

Drive the merge by **overlay size** (number of entries in `modified` + non-zero tombstone bits), not elapsed time. Time-based merges would fire independently of load; size-based merges fire proportionally to actual edit activity.

Suggested threshold: `OVERLAY_MERGE_THRESHOLD = 512` files. Rationale: the overlay's `query_modified` is O(overlay_size × pattern_bigrams) per grep call. At 512 entries with a typical 6-bigram pattern that is 3072 comparisons — still fast. Above ~2000 entries it starts to dominate grep latency.

Add a **minimum interval floor** (`OVERLAY_MERGE_MIN_INTERVAL_SECS = 30`) so a burst of 512 events doesn't trigger back-to-back rebuilds during a compile run.

### New Code Path for `handle_create_or_modify`

The watcher already calls `handle_create_or_modify` → `handle_file_modify` → `overlay.write().modify_file(pos, &content)` (`file_picker.rs:1433–1434`). No change is needed there. The new logic goes into the post-modify accounting:

**Before (current):**
```
handle_file_modify:
    overlay.write().modify_file(pos, &content)
    // overlay grows unboundedly; only reset at next full rescan
```

**After (proposed):**
```
handle_file_modify:
    overlay.write().modify_file(pos, &content)
    let overlay_size = overlay.read().size()           // new method: modified.len() + tombstone popcount
    if overlay_size >= OVERLAY_MERGE_THRESHOLD
        && last_merge_instant.elapsed() >= MIN_INTERVAL
        && !rebuild_in_flight.load(Relaxed)
    {
        spawn_incremental_rebuild(shared_picker.clone())
    }

spawn_incremental_rebuild(shared_picker):
    // runs on the BACKGROUND_THREAD_POOL, not the caller thread
    rebuild_in_flight.store(true, Relaxed)
    let snapshot = {
        let guard = shared_picker.read()
        let picker = guard.as_ref()?
        PostScanUnsafeSnapshot::from_picker(picker)   // existing snapshot type
    }
    let new_index = build_bigram_index(
        &snapshot.files[..snapshot.indexable_count],
        &snapshot.base_path,
        snapshot.arena
    )
    {
        let mut guard = shared_picker.write()
        let picker = guard.as_mut()?
        picker.set_bigram_index(new_index)            // existing — resets overlay too
    }
    rebuild_in_flight.store(false, Relaxed)
```

Two new atomic fields on `FilePicker` (or a small `BigramRebuildState` struct):

```rust
rebuild_in_flight: Arc<AtomicBool>,
last_merge_instant: Arc<Mutex<Instant>>,   // or AtomicU64 with epoch seconds
```

`BigramOverlay` gets one new method:

```rust
pub(crate) fn size(&self) -> usize {
    self.modified.len()
        + self.tombstones.iter().map(|w| w.count_ones() as usize).sum::<usize>()
}
```

### What Happens to the Rescan Path

`run_post_scan` (`scan.rs:304–313`) continues to call `build_bigram_index` and `set_bigram_index` after every full rescan. `set_bigram_index` resets the overlay (`file_picker.rs:1226–1228`). No change needed there: the incremental path is additive, not a replacement. If axis B lands a cooldown on rescans, the incremental rebuild is the primary mechanism between rescans, and the full rescan rebuild is the occasional ground-truth refresh.

## 5. Compatibility and Risk

**Concurrency.** `grep_search` (`grep.rs:1855`) acquires `bigram_overlay.read()` via `o.read()` (`file_picker.rs:1157`) while holding the outer `SharedFilePicker` read lock. The incremental rebuild acquires the write lock to call `set_bigram_index`. This is safe: `parking_lot::RwLock` is write-preferring, so the rebuild will wait for in-flight greps to finish and new greps will queue behind it. The window where the index is unavailable is the time it takes `set_bigram_index` to swap `Arc` pointers — nanoseconds.

**Correctness during rebuild.** The rebuild reads files via `read_bigram_chunk` without holding the picker lock (`bigram_filter.rs:649–651`, existing design). A file modification concurrent with the rebuild may land in the new index, may land in the new overlay, or may be missed by both. The existing overlay serves as the safety net: the new overlay is created fresh by `set_bigram_index` immediately after install, and the next `handle_file_modify` call for any file event that arrives after `set_bigram_index` will correctly populate it.

**Rebuild cost per file event.** No per-file cost is added beyond the existing `modify_file` call (O(content_len) bigram extraction, already paid). The merge is triggered at most once per `MIN_INTERVAL`. If axis B caps rescans at e.g. one per 60 s, set `OVERLAY_MERGE_MIN_INTERVAL_SECS` to match or be a fraction of that floor.

**Regression on small repos.** On a small repo (< 1000 files) the overlay threshold will rarely be hit. The behavior is identical to today.

**Skip-index.** `build_bigram_index` rebuilds both the consecutive and skip-1 indexes together (`bigram_filter.rs:697–699`). The incremental rebuild calls `build_bigram_index` in full, so the skip-index is always consistent with the base index.

**New dependency.** None.

## 6. Rollout Plan

- Ship behind a compile-time feature flag `incremental-bigram` defaulting off for the first PR; flip default to on after one soak week.
- Can land independently of axes A, B, D. It is complementary to B (B reduces rescan frequency; C reduces the cost of each overlay-to-index transition between rescans).
- Estimated diff: ~80 LOC in `bigram_filter.rs` (new `size()` method, `BigramRebuildState` struct), ~50 LOC in `file_picker.rs` (trigger logic in `handle_file_modify` + `tombstone_file`), ~10 LOC in `scan.rs` (pass rebuild state to post-scan). Total ~140 LOC.

## 7. Open Questions

1. **Threshold tuning.** `OVERLAY_MERGE_THRESHOLD = 512` is a guess. Should Oystein instrument the overlay size distribution in production before picking a number?
2. **Interaction with axis B cooldown.** If axis B adds a 60 s rescan floor, does a 30 s overlay merge interval make sense, or should the merge defer to the next rescan instead? The overlay is already correct at query time, so deferring is safe — it just means larger overlays.
3. **`add_new_file` and overflow.** New files appended after `set_bigram_index` land in the overflow region and are searched unconditionally by `grep_search` (`grep.rs:2115–2119`). They are not in the overlay and not in the base index. The incremental rebuild will include them (they are in `files[..indexable_count]` by then). Is this the intended behavior, or should new files be explicitly added to the overlay's modified set so they appear in `query_modified`? Currently overflow files bypass bigram filtering entirely, which is correct but means bigram prefiltering degrades as overflow accumulates.
4. **Content read at rebuild time vs. event time.** The incremental rebuild re-reads files from disk at rebuild time, not at event time. For a file modified and then deleted between event and rebuild, `read_bigram_chunk` returns `None` and the file is simply not indexed — consistent with current behavior. Acceptable?
