//! `GET /api/stats`, cached in memory until `items.json` changes.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::SystemTime;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::common::{modified_time, ApiError};

/// Only the field the stats need; the rest of each item is ignored.
#[derive(Deserialize)]
struct PricedItem {
    price: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    total: usize,
    average_price: f64,
}

/// Shared state for the stats route.
///
/// Bug 3 fix: the computed stats are cached together with the file's mtime and only
/// recomputed when `items.json` has been modified since.
pub struct Handler {
    data_path: PathBuf,
    cache: RwLock<Option<(SystemTime, Stats)>>,
}

impl Handler {
    pub fn new(data_path: PathBuf) -> Self {
        Self {
            data_path,
            cache: RwLock::new(None),
        }
    }

    fn stats(&self) -> Result<Stats, ApiError> {
        // Fast path: shared lock only.
        let mtime = modified_time(&self.data_path)?;
        let cached = *self.cache.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(stats) = fresh_stats(cached, mtime) {
            return Ok(stats);
        }

        // Slow path: same double-checked pattern as the items cache, so only one
        // thread recomputes after a change.
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        let mtime = modified_time(&self.data_path)?;
        if let Some(stats) = fresh_stats(*cache, mtime) {
            return Ok(stats);
        }

        let raw = fs::read_to_string(&self.data_path)
            .map_err(|e| ApiError::internal("read data file", e))?;
        let items: Vec<PricedItem> =
            serde_json::from_str(&raw).map_err(|e| ApiError::internal("parse data file", e))?;
        let stats = compute(&items);
        *cache = Some((mtime, stats));
        Ok(stats)
    }
}

fn fresh_stats(cached: Option<(SystemTime, Stats)>, mtime: SystemTime) -> Option<Stats> {
    cached
        .filter(|(cached_mtime, _)| *cached_mtime == mtime)
        .map(|(_, stats)| stats)
}

/// GET /api/stats
pub async fn get_stats(State(handler): State<Arc<Handler>>) -> Result<Json<Stats>, ApiError> {
    let stats = handler.stats()?;
    Ok(Json(stats))
}

fn compute(items: &[PricedItem]) -> Stats {
    if items.is_empty() {
        return Stats::default();
    }
    let sum: f64 = items.iter().map(|item| item.price).sum();
    Stats {
        total: items.len(),
        average_price: sum / items.len() as f64,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::common::test_support::temp_data_file;

    fn set_mtime(path: &Path, mtime: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn stats_are_cached_until_the_file_changes() {
        let path = temp_data_file(
            r#"[{"id":1,"name":"A","category":"","price":10},{"id":2,"name":"B","category":"","price":30}]"#,
        );
        let handler = Handler::new(path.clone());
        let before = Stats {
            total: 2,
            average_price: 20.0,
        };
        assert_eq!(handler.stats().unwrap(), before);

        // Rewrite the file but restore its old mtime: the cached stats are served.
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, r#"[{"id":1,"name":"A","category":"","price":100}]"#).unwrap();
        set_mtime(&path, mtime);
        assert_eq!(handler.stats().unwrap(), before);

        // Once the mtime moves, the stats are recomputed.
        set_mtime(&path, mtime + Duration::from_secs(1));
        assert_eq!(
            handler.stats().unwrap(),
            Stats {
                total: 1,
                average_price: 100.0
            }
        );
    }

    #[test]
    fn empty_list_gives_zero_stats() {
        let handler = Handler::new(temp_data_file("[]"));
        assert_eq!(handler.stats().unwrap(), Stats::default());
    }
}
