# Pre-reset entry points

[`main.rs`](main.rs) and [`lib.rs`](lib.rs) are snapshots from commit `7c734a3`, before the entry points were stripped. They preserve the former CLI dispatch, library exports, and embedded CLI tests for reference. They are not compiled and refer to backend APIs that are no longer present.

[`sqlite/`](sqlite/) preserves the removed SQLite backend. These files are archival references, not compiled.

See the [architecture overview](../../README.md#architecture) for the current design.
