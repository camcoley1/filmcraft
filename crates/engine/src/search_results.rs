//! The result of the last `ai.search`, kept on the session so the Project panel can show it
//! (always compiled: the panel does not depend on the `sidecar` feature).

use filmcraft_project::ItemId;
use filmcraft_time::Tick;
use serde::Serialize;

/// One place a query was found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    pub item: ItemId,
    /// Media time of the match.
    pub time: Tick,
    /// Visual hits: cosine similarity of the query and the frame (about 0.2 to 0.35 for a match);
    /// transcript hits: 1.
    pub score: f32,
    /// `"visual"` or `"transcript"`.
    pub kind: &'static str,
    /// The media changed since it was indexed (the time may be wrong): index it again.
    pub stale: bool,
}

/// The last query and what it found.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SearchResults {
    pub query: String,
    pub hits: Vec<SearchHit>,
    /// The visual search is still running (transcript hits are already in `hits`).
    pub pending: bool,
    /// Why the visual search failed or was left out.
    pub note: Option<String>,
    /// Media items in the index, and how many of them changed since.
    pub indexed: usize,
    pub stale_items: usize,
}
