//! floki-core: pure index + query engine (no Win32 calls).
//!
//! SPEC sections 3-4. All of it is unit-testable on any OS.
//!
//! # Index events
//!
//! [`IndexEvent`] is the live-update input for [`Index::apply`]. The `ntfs`
//! crate has its own `UsnEvent`; the service maps between them:
//!
//! | `UsnEvent` variant | `IndexEvent` mapping |
//! |---|---|
//! | `Create(record)` | `Create { frn, parent_frn, name, flags }` |
//! | `Delete { frn }` | `Delete { frn }` |
//! | `RenameOld { .. }` | dropped (the `RenameNew` half carries the update) |
//! | `RenameNew(record)` | `Rename { frn, parent_frn, name }` |
//! | `Overwrite(record)` | `Update { frn, flags }` |
//!
//! # `prev` narrowing
//!
//! [`search`] narrows to `prev` whenever `prev` is `Some`. The caller must
//! only pass it when [`prev_reusable`] holds for the two query strings (new
//! query starts with the previous one, neither query has OR / NOT, and the
//! new query contains no `"`, `(`, `<` or `|`) and the previous result set
//! was complete (not truncated by `max_results`).
//!
//! `prev` holds [`EntryId`]s, which [`Index::compact`] renumbers. The index
//! carries an [`epoch`](Index::epoch), bumped on every `compact()` and on
//! [`load`](Index::load); callers caching hits (the service keeps last hits
//! per `client_id`) must record the epoch with them and drop the cache when
//! it changes. `compact()` also returns the remap (`old id -> new id`,
//! [`EntryId::MAX`](crate::EntryId) for removed entries) for callers that
//! prefer translating ids. [`Index::path`] is FRN-based and stays valid
//! across compaction; entry ids do not.
//!
//! `prev` is a snapshot: files created, renamed, or revived after the cached
//! query are missed by narrowing (a rename retires the old id and allocates a
//! new one, exactly like delete+create), same as creates already were. A
//! fresh (non-narrowed) query always sees them.
//!
//! # Sorting
//!
//! [`Sort::NameAsc`] / [`Sort::NameDesc`] order by folded name across all
//! matches. [`Sort::PathAsc`] / [`Sort::PathDesc`] rebuild one path per match
//! and sort all matches before paging, so cross-page ordering is exact.
//! [`Sort::ModifiedAsc`] / [`Sort::ModifiedDesc`] / [`Sort::CreatedAsc`] /
//! [`Sort::CreatedDesc`] do the same but key on a filesystem stat per match
//! (the index stores no timestamps); unstattable entries sort last.

pub mod arena;
pub mod entry;
pub mod error;
pub mod fold;
pub mod index;
pub mod persist;
pub mod query;
pub mod search;

pub use arena::NameArena;
pub use entry::{Entry, EntryId, DIRECTORY, HIDDEN, REPARSE, SYSTEM, TOMBSTONE};
pub use error::CoreError;
pub use index::{
    Index, IndexEvent, MemoryBreakdown, SortedSnapshot, Volume, VolumeRemoval, VolumeSwap,
    MAX_PATH_HOPS, PENDING_MAX,
};
pub use persist::TargetsConfig;
pub use query::{parse, Node, Query, Term, TermKind};
pub use search::{
    collect_matches, count, paths_of, prev_reusable, search, search_paged, set_search_threads, Hit,
    SearchOptions, SearchResult, Sort,
};
