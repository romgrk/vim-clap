//! This crate provides the feature of filtering a stream of lines.
//!
//! Given a stream of lines:
//!
//! 1. apply the matcher algorithm on each of them.
//! 2. sort the all lines with a match result.
//! 3. print the top rated filtered lines to stdout.

mod dynamic;
mod source;

use anyhow::Result;
use matcher::Algo;
use rayon::prelude::*;
use types::SourceItem;

pub use dynamic::dyn_run;
pub use matcher;
pub use source::Source;
#[cfg(feature = "enable_dyn")]
pub use subprocess;

/// Tuple of (matched line text, filtering score, indices of matched elements)
pub type FilterResult = (SourceItem, i64, Vec<usize>);

/// Input of filter (display line and optional string to filter)
/// Returns the ranked results after applying the matcher algo
/// given the query String and filtering source.
pub fn sync_run<I: Iterator<Item = SourceItem>>(
    query: &str,
    source: Source<I>,
    algo: Algo,
) -> Result<Vec<FilterResult>> {
    let mut ranked = source.filter(algo, query)?;

    ranked.par_sort_unstable_by(|(_, v1, _), (_, v2, _)| v2.partial_cmp(&v1).unwrap());

    Ok(ranked)
}
