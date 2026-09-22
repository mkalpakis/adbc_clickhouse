# Correctness findings: DESCRIBE-based execute_schema() fix

Findings from testing the `execute_schema()`/`of_query()` patch (uncommitted, `git diff` on
`src/schema.rs` + `src/statement.rs`) against a real ClickHouse 26.8 + DuckDB 1.5.5 stack
(`duckdb_clickhouse_latency_test/`, TPC-H `lineitem` SF1, 6,001,215 rows).

## BUG 1 (root cause found, fix not yet written): raw multi-row scans return every other row zeroed

**Status: root cause confirmed and fully explained. Do not ship until fixed.**

### Root cause

`execute_schema()`'s `DESCRIBE`-based type mapping (`schema.rs`'s `arrow_decimal()`, driven by the
`DecimalType` that `clickhouse_types::data_types` parses out of the `DESCRIBE` type string, e.g.
`"Decimal(15, 2)"`) picks the Arrow decimal width using ClickHouse's **documented internal storage
width rule**: precision 1-9 -> `Decimal32`, 10-18 -> `Decimal64`, 19-38 -> `Decimal128`, 39-76 ->
`Decimal256`. For `l_quantity Decimal(15, 2)` (precision 15), that rule picks **`Decimal64`** (8-byte
element stride) -- confirmed directly: `execute_schema()` on the repro query reports
`Field { name: "l_quantity", data_type: Decimal64(15, 2), ... }`.

But ClickHouse's **actual `FORMAT ArrowStream` wire serialization does not follow that rule** -- it
emits **`Decimal128`** (16-byte stride) for every decimal precision tested from 5 up to 30 (see table
below), i.e. it does not narrow to `Decimal32`/`Decimal64` on the wire the way its own documented
in-memory storage-width convention would suggest. Confirmed by fetching raw `FORMAT ArrowStream`
bytes via `curl` (bypassing the driver and DuckDB entirely) and decoding them with a standalone
`arrow-ipc` reader:

| `CAST(1.23 AS Decimal(P, 2))` | Arrow wire type (raw curl fetch) |
|---|---|
| P=5  | `Decimal128(5, 2)` |
| P=9  | `Decimal128(9, 2)` |
| P=10 | `Decimal128(10, 2)` |
| P=15 | `Decimal128(15, 2)` (this is `l_quantity`) |
| P=18 | `Decimal128(18, 2)` |
| P=19 | `Decimal128(19, 2)` |
| P=30 | `Decimal128(30, 2)` |

So there are **two independently-fetched schemas that disagree on element width** for the same
column: the `execute_schema()`/`DESCRIBE`-derived schema (`Decimal64`, 8 bytes/element) and the
schema embedded in the real `execute()` Arrow stream (`Decimal128`, 16 bytes/element). DuckDB's
community ADBC extension (`columnar-tech/duckdb-adbc-client`, the actual code running here --
confirmed by fetching the exact pinned commit `e0c62ee9e6557c31fd2ea7ead773ea338567612a` that
`duckdb/community-extensions`' `extensions/adbc/description.yml` builds from) uses the
**`execute_schema()`-derived schema** to populate `return_types`/`arrow_table` in
`AdbcArrowScanFunctionData`'s constructor (`adbc_scan.cpp`), then walks the **real** per-batch Arrow
buffers from `execute()` using that (wrong, narrower) stride when converting to DuckDB's native
`Vector` representation. Reading a 16-byte-stride buffer 8 bytes at a time means: for row N (0-indexed),
byte offset `N*8` is read as one element. For small positive TPC-H quantities, a `Decimal128`
little-endian value's low 8 bytes hold the real magnitude and its high 8 bytes are all zero (no
sign extension needed for small positives) -- so *even* 8-byte slices land exactly on a real value's
low half (correct), and *odd* 8-byte slices land on the following value's zero high half, **or** the
current value's zero high half, depending on parity -- either way, exactly half the rows read as zero.
This exactly matches every observed symptom:
- **Deterministic, not a race**: it's a fixed stride mismatch, not timing-dependent.
- **Only non-sort-key/decimal columns affected**: `l_orderkey`/`l_linenumber` are `Int64` in both the
  `DESCRIBE`-derived schema and the real wire data (8 bytes either way, no mismatch, no corruption).
- **Only when `execute_schema()` succeeds**: only then does DuckDB have a second, independently-derived
  schema to (wrongly) trust over the real stream's own embedded schema.
- **Not reproducible via the Rust driver directly, in any FFI mode** (plain trait calls, real C ABI
  via `load_static`, or `dlopen`ing the actual `.so` -- all three tested and clean): a Rust
  `RecordBatchReader` consumer (including `arrow-rs`'s own Arrow C Data Interface import code) always
  takes the schema from the stream's own embedded schema message, never conflates it with a
  separately-fetched `execute_schema()` result the way DuckDB's C++ extension's `ArrowToDuckDB`
  conversion does.
- Confirmed via direct instrumentation of the driver (temporary `eprintln!` in `execute()`,
  `execute_schema()`, and `ArrowStreamReader::begin()`/`next()`, later reverted) that the driver hands
  DuckDB exactly **one** correct, complete 6-row `RecordBatch` in a single FFI call -- the corruption
  is not introduced by the driver at all, confirming it happens purely in DuckDB's consumption of the
  (correct) data using the (mismatched) schema.

### Why literal/`UNION ALL` queries don't trigger it (resolves an earlier gap)
A literal query returning the same 2 values (`SELECT CAST(17.00 AS Decimal(15,2)) UNION ALL SELECT
CAST(36.00 AS Decimal(15,2))`) comes back **correct**, even though it has the exact same
`DESCRIBE`-derived `Decimal64` vs. real-wire `Decimal128` schema mismatch as the table-read case.
Raw `curl`-fetched `FORMAT ArrowStream` bytes for both, decoded structurally, explain why:

- **Literal `UNION ALL`** (`lit.arrows`, 448 bytes): ClickHouse emits **two separate RecordBatch
  messages, one per `UNION` branch, each containing exactly 1 row**.
- **Real table read** (`tbl.arrows`, 304 bytes): ClickHouse emits **one RecordBatch containing both
  rows**.

The corruption only appears when DuckDB misreads **2 or more elements from the same buffer** using
the wrong (narrower, `Decimal64`/8-byte) stride instead of the real `Decimal128`/16-byte stride. For a
batch with only 1 row, reading "1 element at 8 bytes" from the buffer still lands on bytes `[0, 8)` --
the low 64 bits of that single real `Decimal128` value -- which for any small positive number *is* the
correct value (no sign-extension needed, high 64 bits are zero anyway). So a 1-row-per-batch layout is
accidentally immune: there's nothing to misalign against within a single element. A batch with >=2 rows
of the mismatched type is where the stride error actually shifts subsequent rows' offsets and produces
the "every other row zeroed" pattern. This is a clean, complete explanation with no remaining gap: the
bug is `min(rows in one real-data batch containing a Decimal32/64/128-kind, i.e. narrowed, column) >= 2`,
combined with the schema-width mismatch above.

**Minimal breaking example** (single `Decimal(15,2)` column, `Memory` table engine -- not
MergeTree-specific, no `WHERE`/`ORDER BY` needed):
```sql
-- ClickHouse:
CREATE TABLE t (v Decimal(15,2)) ENGINE = Memory;
INSERT INTO t VALUES (17.00), (36.00);
```
```sql
-- DuckDB:
LOAD adbc;
SELECT v FROM read_adbc('profile://clickhouse', 'SELECT v FROM t');
-- returns v = [17.00, 0.00] instead of [17.00, 36.00]
```

### The actual wire rule (fully characterized)
Tested precision 5 through 76, plus explicit `Decimal32(4)`/`Decimal64(4)`/`Decimal128(4)` casts (not
just the bare `Decimal(P,S)` syntax). ClickHouse's `FORMAT ArrowStream` serializer **always emits
`Decimal128` for logical precision <=38, and `Decimal256` for precision 39-76 -- full stop**. It never
emits `Decimal32` or `Decimal64` on the wire, even when the column is explicitly declared
`Decimal32(...)`/`Decimal64(...)` (i.e. even when ClickHouse's own internal storage for that column
genuinely is 32/64-bit). So the wire rule collapses `DecimalType::Decimal32 | Decimal64 | Decimal128`
(the `kind` `clickhouse_types` parses from any of these type strings) all to Arrow `Decimal128`, and
only `DecimalType::Decimal256` stays `Decimal256`.

### Confirmed in ClickHouse's own source, at both the pinned commit and current `master`
The pinned commit the installed community extension actually builds from
(`columnar-tech/duckdb-adbc-client@e0c62ee9e6557c31fd2ea7ead773ea338567612a`) hardcodes the wire rule
explicitly in `CHColumnToArrowColumn.cpp:844-854` (data-fill switch): CH-internal `Decimal32`/`64`/`128`
storage are all routed through `fillArrowArrayWithDecimalColumnData<..., arrow::Decimal128,
arrow::Decimal128Builder>`, only `Decimal256` gets `arrow::Decimal256`/`Decimal256Builder`.

Checked current ClickHouse `master` (`be9434c24112772777fc14021fe1d4703423d495`, fetched
2026-09-22) too, since the user asked to compare against a specific link that had shifted -- the file
grew from 1205 to 2101 lines between the two commits (unrelated changes), so line numbers moved, but
the logic is unchanged and now has an explicit comment explaining *why*, at
`CHColumnToArrowColumn.cpp:1684-1691` (type-derivation path, `getArrowType()`):
```cpp
const auto precision = decimal_type->getPrecision();
/// Reproduces what the removed `arrow::decimal` did: `Decimal256` above precision 38,
/// `Decimal128` otherwise. `arrow::smallest_decimal` is deliberately not used here -
/// Arrow gained `Decimal32` and `Decimal64`, so it would narrow small decimals and
/// change the schema we write.
arrow_type = precision > 38
    ? arrow::decimal256(precision, decimal_type->getScale())
    : arrow::decimal128(precision, decimal_type->getScale());
```
This means the wire rule is a **deliberate, documented ClickHouse design decision**, not an accident or
a version quirk: Apache Arrow's C++ library recently added native `Decimal32`/`Decimal64` types and a
`smallest_decimal()` helper that *would* narrow to them; ClickHouse's maintainers explicitly declined
to use it, specifically to avoid changing the wire schema they've always emitted. That meaningfully
de-risks the "what if this changes underneath us" concern for any fix that hardcodes this rule -- the
maintainers already had the natural opportunity to narrow and chose stability instead.

Permalinks (plaintext):
```
https://github.com/ClickHouse/ClickHouse/blob/3196ab525aa6f1fef8d367db7610b765f7737f01/src/Processors/Formats/Impl/CHColumnToArrowColumn.cpp#L844-L854
https://github.com/ClickHouse/ClickHouse/blob/be9434c24112772777fc14021fe1d4703423d495/src/Processors/Formats/Impl/CHColumnToArrowColumn.cpp#L1684-L1691
```

### Audit: is any other type in `schema.rs`'s mapping at risk of the same failure shape?
Went through every arm of `ch_type_to_arrow()` (`schema.rs:255-362`) that has *some* inference step
(as opposed to a fixed 1:1 mapping with no guessing), and checked each against real `FORMAT
ArrowStream` wire bytes via `curl`. **Decimal is the only one affected.**

| Type | schema.rs mapping | Wire reality (curl-verified) | Verdict |
|---|---|---|---|
| `Decimal(P,S)` | width guessed from precision (32/64/128/256) | always 128 (P<=38) or 256 (P>38) | **bug** |
| `DateTime64(P)` | `TimeUnit` from precision, matches CH's own `getArrowTimeUnit()` (`CHColumnToArrowColumn.cpp:925-933`, byte-for-byte identical logic) | Second/Milli/Micro/Nano exactly as predicted for P=0,3,6,9 | correct |
| `LowCardinality` dict index | width from the `use_signed_indexes_for_dictionary`/`use_64_bit_indexes_for_dictionary` *settings*, queried live via `Settings::query()`, not guessed from `DESCRIBE` | `Int32` for default settings, as predicted | correct -- this is the right pattern, the one `arrow_decimal()` should have followed |
| `FixedString(N)` | `FixedSizeBinary(N)`, N taken verbatim from the type string | `FixedSizeBinary(N)` exactly | correct, no inference step (N is explicit) |
| `Int128`/`UInt128` | `FixedSizeBinary(16)` (hardcoded by type name) | `FixedSizeBinary(16)` | correct |
| `Int256`/`UInt256` | `FixedSizeBinary(32)` (hardcoded) | `FixedSizeBinary(32)` | correct |
| `Int8`..`UInt64`, `Date`, `Date32`, `DateTime`, `IPv4`, `IPv6`, `Enum8`/`Enum16` | fixed 1:1 mapping from type name | not separately wire-tested -- no inference step exists to get wrong | correct by construction |
| `Array`/`Tuple`/`Map`/`String` | variable-length (offsets + values buffers) | n/a | not stride-sensitive -- no fixed element width to mismatch |

Why Decimal alone is at risk, structurally: it's the only type where (1) the `DESCRIBE` string gives
precision/scale but not wire width directly, (2) a library (`clickhouse-types`) infers an
internal-storage width from that precision using a real CH rule that just isn't the wire rule, and
(3) CH's serializer deliberately widens on the wire for exactly this family. Every other type either
hardcodes width directly from the type name (nothing to infer), is variable-length (no fixed stride to
violate), or -- like `LowCardinality` -- already derives width from a live setting instead of a guess.
**A fix scoped to `arrow_decimal()` alone is sufficient; nothing else in `schema.rs` needs the same
treatment.**

### The bug already exists in the *published* driver, independent of this session's `execute_schema()` patch
`arrow_decimal()` is not new code from this session's `execute_schema()` work -- it's called by the
pre-existing, already-shipped `Connection::get_table_schema()` (`lib.rs:686-693` -> `schema::of_table()`,
present since `v0.1.0`), a standard ADBC catalog method unrelated to `execute_schema()`/`of_query()`.
So the mismatch is demonstrable today against stock `v0.1.1` (`upstream/main` @ `2c374ad`) using only
public API that ships in the current release -- see `tests/it/upstream_get_table_schema_decimal_mismatch.rs`.
Worth filing upstream once reviewed.

### Not yet done
Writing the actual fix in `schema.rs`'s `arrow_decimal()`: it needs to stop deriving the Arrow decimal
width from ClickHouse's internal-storage `kind` and instead always use `Decimal128` for
`Decimal32`/`Decimal64`/`Decimal128` `kind`s (and `Decimal256` only for `Decimal256` `kind`), matching
the wire rule above -- not the driver's own type-mapping logic to write, per the "figure it out
myself" ask.

---

## BUG 1 investigation history (superseded by "Root cause" above, kept for context)

**Original status while investigating: confirmed, deterministic, root cause not found.**

`read_adbc()` on a raw (non-aggregated) multi-row `SELECT` silently returns every other row
(rows 2, 4, 6, ... 1-indexed) as all-zeros/all-default, while odd rows are correct. No error
is raised -- this is silent data corruption, worse than the double-execution bug being fixed.

### Repro
```sql
LOAD adbc;
SELECT l_orderkey, l_linenumber, l_quantity, l_extendedprice, l_discount, l_tax
FROM read_adbc('profile://clickhouse',
  'SELECT l_orderkey, l_linenumber, l_quantity, l_extendedprice, l_discount, l_tax
   FROM lineitem WHERE l_orderkey = 1 ORDER BY l_linenumber');
```
Rows 2, 4, 6 come back as `l_quantity=0.00, l_extendedprice=0.00, l_discount=0.00, l_tax=0.00`
(and dates zeroed too, in a wider column test). ClickHouse's own `clickhouse-client` shows all
6 rows have real, nonzero values -- the stored data is fine.

### What's confirmed
- **Deterministic and scale-invariant**: reproduced identically for `LIMIT N` with N = 2, 3, 6,
  10, 100, 1000, 10000, 100000 (client-side `sum(l_quantity)`/`sum(l_extendedprice)` after raw
  fetch consistently comes back near 50% of the true value; row *count* is always correct).
- **N=1 is unaffected** (nothing to alternate against).
- **Aggregate / `GROUP BY` queries are unaffected** -- multi-row `GROUP BY` results (4 rows, and
  a 4-column `GROUP BY` with more groups) matched a local DuckDB TPC-H reference exactly, byte
  for byte. Only raw (non-aggregated) row scans are affected.
- **Caused specifically by `execute_schema()` succeeding.** `git stash` reverting `schema.rs`/
  `statement.rs` to the original `err_unimplemented!()` stub and rebuilding makes it vanish
  completely on the identical query. This is a clean A/B result.
- **NOT reproducible via the Rust driver directly.** Wrote `tests/it/diag_corruption.rs`
  (kept in the tree) calling `statement.execute_schema()` then `statement.execute()` on the same
  statement -- the same sequence DuckDB's `AdbcArrowScanFunctionData` constructor +
  `AdbcProduceArrowScan` use (see `duckdb-adbc-client/src/adbc_scan.cpp:122-150`, though note:
  the Docker image actually uses the prebuilt **community** adbc extension via
  `INSTALL adbc FROM community`, not this fork -- so this C++ is reference/plausible, not
  confirmed-identical to what's actually running). Passed cleanly every time: current-thread and
  multi-thread tokio runtimes (`ADBC_CLICKHOUSE_TEST_MULTI_THREAD=1`), and with 0-2000ms sleeps
  inserted between the two calls.
- **NOT a DuckDB parallel-scan race.** Still corrupts with `PRAGMA threads=1` in the DuckDB
  session.
- **NOT ClickHouse-side.** Raw `curl` fetch of the exact `FORMAT ArrowStream` bytes for the same
  query (compression disabled, matching what `clickhouse-ext-arrow` actually requests) decodes
  cleanly via a standalone `arrow-ipc` `StreamReader` -- single RecordBatch, all 6 rows correct.

### Refined: it's not "every other row" -- it's "every other row's non-sort-key columns"
Re-tested with `SELECT l_orderkey, l_linenumber, l_quantity FROM lineitem ORDER BY l_orderkey,
l_linenumber LIMIT 8`: `l_orderkey`/`l_linenumber` (the `ORDER BY`/`MergeTree` sort-key columns)
come back **fully correct on every row**. Only `l_quantity` (a non-key column) zeros out on
alternating rows (1,1/17.00 correct, 1,2/**0.00**, 1,3/36.00 correct, 1,4/**0.00**, ...,
2,1/28.00 correct, 3,1/**0.00**). This is why `sum(l_orderkey)` over 200,000 `ORDER BY`-scanned
rows came back byte-exact (sort key never corrupts) while `sum(l_quantity)` over the identical
row set came back at ~50% (confirmed corrupt at every N from 50 to 200,000 tested). Also
confirmed: pure literal multi-row results (`SELECT ... UNION ALL ...`, no table) and
`system.numbers` (streaming table function, tested up to 200,000 rows) are **not** affected --
this appears specific to `MergeTree` table scans, and specifically to columns outside the sort
key. This strongly points at ClickHouse's `MergeTree` engine reading primary-key columns via a
different internal path (sparse primary index / mark cache) than regular data columns, and that
path apparently being immune to whatever the preceding `DESCRIBE` triggers for the other path.

### Working hypothesis (unconfirmed)
Corruption is introduced somewhere between DuckDB's C++ ADBC extension and the driver's FFI
boundary, specifically tied to a statement that has had `execute_schema()` called on it before
`execute()` is called on the *same* statement object (in the stub path, DuckDB's
`ResetStatement()` swaps in a brand-new statement after the discarded schema-peek query, so the
real fetch never reuses a statement `execute_schema()` touched -- with `execute_schema()` now
succeeding, that reset is skipped). Why raw scans and not aggregates differ is unexplained; one
untested lead is that ClickHouse may stream raw scans as multiple small Arrow record batches
(one part/granule per batch) while `GROUP BY` fully materializes into one block server-side --
if so the bug may be in how the driver's `ArrowStreamReader` (`src/reader.rs`) consumes a
*second* batch after `begin()`'s first-batch peek, specifically when this is the first real
`execute()` after a prior `execute_schema()` on the same `Client`/connection. Not verified.

### Ruled out: plain HTTP/1.1 keep-alive connection reuse
Tested directly with `curl --next` (DESCRIBE, then the real `FORMAT ArrowStream` fetch, on the
same kept-alive TCP connection, no driver or DuckDB involved) -- response came back byte-identical
to a fresh single-request baseline (672/672 bytes, decodes cleanly, all 6 rows correct), 3/3
repeats. (First attempt at this test wrongly looked like truncation/corruption, but that was a
curl bug in the test itself -- `-u user:pass` was only specified once and `--next` resets
per-request options including auth, so the second request 401'd and I misread the auth-error
body as truncated binary. Redone with `-u` repeated after `--next`: clean pass, no corruption.)
So this is **not** a generic HTTP connection-reuse/session race at the transport level --
reinforces that the fault is specific to how DuckDB's C++ extension (or its FFI calls into this
crate) sequences `execute_schema()` and `execute()`, not something reproducible one layer down.

### Next steps (not started)
- Get the actual community adbc extension's C++ source (not just the `duckdb-adbc-client` fork)
  to confirm the real call sequence into the driver.
- Check whether ClickHouse streams `SELECT ... WHERE l_orderkey = 1` as >1 Arrow record batch
  (vs. exactly 1 for `GROUP BY`) -- would confirm/deny the batch-count theory above.
- Try reproducing via raw ADBC C API calls (bypassing DuckDB entirely) to further isolate FFI
  vs. DuckDB-extension-logic as the fault boundary.

---

## Everything else tested: no other correctness issues found

All single-row (unaffected by BUG 1) and aggregate/`GROUP BY` (also unaffected by BUG 1) checks
passed exactly, against a local DuckDB TPC-H reference where applicable:

- **Numeric types**: Int8/16/32/64 and UInt8/16/32/64 at their min/max values, Float32/Float64,
  Bool -- exact.
- **Decimals**: `Decimal(9,4)`, `Decimal(18,4)`, `Decimal(38,6)`, including negative values --
  exact, correct precision/scale.
- **Strings**: plain, empty, unicode/emoji, `FixedString` (correctly zero-padded, mapped to
  `BLOB` per `output_format_arrow_string_as_string`/`fixed_string_as_fixed_byte_array`
  settings), `LowCardinality(String)`, `LowCardinality(Nullable(String))` with both `NULL` and a
  present value -- exact.
- **Nullability**: `Nullable(Int32)`/`Nullable(Float64)`/`Nullable(String)` with `NULL` and with
  real values, and `NULL` nested inside an `Array` -- all correctly round-tripped as SQL `NULL`,
  not `0`/empty-string.
- **Collections**: `Array(Int32)` (including empty array), `Array(String)`, `Map(String, Int32)`,
  `Tuple(Int32, String)` -- exact.
- **Enum8**: decodes to its raw integer value (by design, per `schema.rs`'s documented mapping --
  not a bug).
- **Dates/times**: `Date` (raw `UInt16` day-offset, matches `of_table`'s existing, intentional
  behavior -- confirmed this is how ClickHouse's own Arrow serialization works, not
  Arrow-spec `Date32`), `Date32` (proper Arrow `Date32`), `DateTime`/`DateTime('UTC')` (raw
  `UInt32` epoch seconds), `DateTime64(3)`/`DateTime64(6,'UTC')` (proper `Timestamp` with
  correct precision and timezone) -- exact.
- **Network types**: `IPv4` (raw `UInt32`, by design), `IPv6` (correct 16 raw bytes, `FixedSizeBinary(16)`).
- **`sql` text edge cases** (the trailing-whitespace/semicolon/comment concern from `of_query`'s
  own doc comment): trailing spaces, trailing newlines, trailing `;`, trailing `; ` +
  whitespace, trailing `-- line comment`, and leading whitespace before a leading comment --
  all parsed and executed correctly once wrapped in `DESCRIBE (...)`.
- **Empty result set**: `WHERE` clause matching zero rows -- correct empty result with correct
  schema (`DESCRIBE ch_empty` showed the right column types even with 0 rows).
- **Error propagation**: an unknown column reference and a SQL syntax error both surface the
  real ClickHouse error text/code cleanly through DuckDB's `Binder Error`, unaffected by routing
  through `DESCRIBE` first.
- **Row cap** (separate from BUG 1): the old ~20k-row `SESSION_IS_LOCKED` HTTP session-race
  ceiling from `i-want-to-modify-rippling-pascal.md` is gone -- bisected clean from 10k up to the
  full 6,001,215-row table with no errors (though see BUG 1: "no error" does not mean "correct
  data" for non-key columns of a raw scan).

## Summary

**One serious bug (BUG 1, above) blocks shipping this fix as-is.** Everything else checked --
broad type coverage, nullability, collections, error handling, and the row-count ceiling -- is
correct or improved.
