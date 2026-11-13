//! `/api/items` handlers backed by `items.json`, with an mtime-keyed in-memory cache.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        Path, Query, State,
    },
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::common::{modified_time, ApiError};

const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 200;

/// One catalog entry, mirroring the objects in `items.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub price: f64,
}

struct CacheEntry {
    mtime: SystemTime,
    items: Arc<Vec<Item>>,
}

/// Shared state for the items routes.
pub struct Handler {
    data_path: PathBuf,
    cache: RwLock<Option<CacheEntry>>,
    /// Held for each whole read-modify-write cycle so concurrent writes can't lose updates.
    write_lock: Mutex<()>,
}

impl Handler {
    pub fn new(data_path: PathBuf) -> Self {
        Self {
            data_path,
            cache: RwLock::new(None),
            write_lock: Mutex::new(()),
        }
    }

    /// Returns all items, re-reading `items.json` only when its mtime has changed.
    ///
    /// Bug 1 fix: the original released the read lock right after the freshness check
    /// and re-parsed the file with no lock held, so several threads could all re-parse
    /// and overwrite each other's cache entry. Now a stale cache is re-checked and
    /// refreshed while holding the write lock, so the check-then-act is atomic: exactly
    /// one thread re-reads the file and the others reuse its result.
    fn read_data(&self) -> Result<Arc<Vec<Item>>, ApiError> {
        // Fast path: shared lock only.
        let mtime = modified_time(&self.data_path)?;
        let cached = fresh_items(
            &self.cache.read().unwrap_or_else(PoisonError::into_inner),
            mtime,
        );
        if let Some(items) = cached {
            return Ok(items);
        }

        // Slow path: check again and refresh under one exclusive lock. The mtime is
        // taken before reading, so a concurrent change can't be cached as "fresh".
        let mut cache = self.cache.write().unwrap_or_else(PoisonError::into_inner);
        let mtime = modified_time(&self.data_path)?;
        if let Some(items) = fresh_items(&cache, mtime) {
            return Ok(items);
        }

        let raw = fs::read_to_string(&self.data_path)
            .map_err(|e| ApiError::internal("read data file", e))?;
        let items: Vec<Item> =
            serde_json::from_str(&raw).map_err(|e| ApiError::internal("parse data file", e))?;
        let items = Arc::new(items);
        *cache = Some(CacheEntry {
            mtime,
            items: Arc::clone(&items),
        });
        Ok(items)
    }

    /// Applies `change` to the current items and saves the result, holding the write
    /// lock for the whole read-modify-write so concurrent requests can't lose each
    /// other's updates. Nothing is saved if `change` returns an error.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Vec<Item>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut items = self.read_data()?.to_vec();
        let result = change(&mut items)?;
        self.write_data(&items)?;
        Ok(result)
    }

    /// Writes to a temp file and renames it over `items.json`, so readers never see a
    /// half-written file, then drops the cached copy.
    fn write_data(&self, items: &[Item]) -> Result<(), ApiError> {
        let raw = serde_json::to_string_pretty(items)
            .map_err(|e| ApiError::internal("encode data", e))?;
        let tmp_path = self.data_path.with_extension("json.tmp");
        fs::write(&tmp_path, raw).map_err(|e| ApiError::internal("write temp data file", e))?;
        fs::rename(&tmp_path, &self.data_path)
            .map_err(|e| ApiError::internal("replace data file", e))?;
        *self.cache.write().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

fn fresh_items(cache: &Option<CacheEntry>, mtime: SystemTime) -> Option<Arc<Vec<Item>>> {
    cache
        .as_ref()
        .filter(|entry| entry.mtime == mtime)
        .map(|entry| Arc::clone(&entry.items))
}

/// Query parameters for `GET /api/items`.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    page: Option<i64>,
    #[serde(rename = "pageSize")]
    page_size: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResponse {
    items: Vec<Item>,
    total: usize,
    page: i64,
    page_size: i64,
    q: String,
}

/// GET /api/items?q=&page=1&pageSize=50
pub async fn list(
    State(handler): State<Arc<Handler>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<ListResponse>, ApiError> {
    let Query(params) = query?;
    let page = params.page.unwrap_or(1).max(1);
    let page_size = match params.page_size {
        Some(size) if size >= 1 => size.min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    };
    let q = params.q.unwrap_or_default().trim().to_lowercase();

    let all = handler.read_data()?;
    let matches: Vec<&Item> = all
        .iter()
        .filter(|item| q.is_empty() || item.name.to_lowercase().contains(&q))
        .collect();

    Ok(Json(ListResponse {
        items: page_slice(&matches, page, page_size)
            .iter()
            .map(|&item| item.clone())
            .collect(),
        total: matches.len(),
        page,
        page_size,
        q,
    }))
}

/// Returns the 1-based `page` of `items`; pages past the end are empty.
fn page_slice<T>(items: &[T], page: i64, page_size: i64) -> &[T] {
    let page_index = usize::try_from(page - 1).unwrap_or(0);
    let page_size = usize::try_from(page_size).unwrap_or(0);
    let start = page_index.saturating_mul(page_size).min(items.len());
    let end = start.saturating_add(page_size).min(items.len());
    &items[start..end]
}

/// GET /api/items/{id}
pub async fn get_one(
    State(handler): State<Arc<Handler>>,
    Path(id): Path<String>,
) -> Result<Json<Item>, ApiError> {
    let id = parse_id(&id)?;
    let items = handler.read_data()?;
    let item = items
        .iter()
        .find(|item| item.id == id)
        .cloned()
        .ok_or_else(item_not_found)?;
    Ok(Json(item))
}

/// Request body for POST and PUT. Unknown fields, including `id`, are ignored.
#[derive(Debug, Deserialize)]
pub struct ItemInput {
    name: Option<String>,
    category: Option<String>,
    price: Option<f64>,
}

/// An `ItemInput` that passed validation.
struct ValidInput {
    name: String,
    category: String,
    price: f64,
}

impl ItemInput {
    /// Bug 2 fix: validation shared by POST and PUT.
    fn validate(self) -> Result<ValidInput, ApiError> {
        let name = self.name.as_deref().unwrap_or_default().trim();
        if name.is_empty() {
            return Err(ApiError::BadRequest(
                "name is required and must not be blank".into(),
            ));
        }
        let price = self
            .price
            .ok_or_else(|| ApiError::BadRequest("price is required".into()))?;
        if price < 0.0 {
            return Err(ApiError::BadRequest("price must not be negative".into()));
        }
        Ok(ValidInput {
            name: name.to_owned(),
            category: self
                .category
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            price,
        })
    }
}

/// POST /api/items
pub async fn create(
    State(handler): State<Arc<Handler>>,
    body: Result<Json<ItemInput>, JsonRejection>,
) -> Result<(StatusCode, Json<Item>), ApiError> {
    let Json(input) = body?;
    let input = input.validate()?;
    let item = handler.mutate(|items| {
        // Assigned under the write lock, so IDs are unique and stay sequential.
        let id = items.iter().map(|item| item.id).max().unwrap_or(0) + 1;
        let item = Item {
            id,
            name: input.name,
            category: input.category,
            price: input.price,
        };
        items.push(item.clone());
        Ok(item)
    })?;
    Ok((StatusCode::CREATED, Json(item)))
}

/// PUT /api/items/{id}
pub async fn update(
    State(handler): State<Arc<Handler>>,
    Path(id): Path<String>,
    body: Result<Json<ItemInput>, JsonRejection>,
) -> Result<Json<Item>, ApiError> {
    let id = parse_id(&id)?;
    let Json(input) = body?;
    let input = input.validate()?;
    let item = handler.mutate(|items| {
        let item = items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(item_not_found)?;
        item.name = input.name;
        item.category = input.category;
        item.price = input.price;
        Ok(item.clone())
    })?;
    Ok(Json(item))
}

/// DELETE /api/items/{id}
pub async fn delete(
    State(handler): State<Arc<Handler>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    handler.mutate(|items| {
        let index = items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(item_not_found)?;
        items.remove(index);
        Ok(())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn parse_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::BadRequest("invalid id".into()))
}

fn item_not_found() -> ApiError {
    ApiError::NotFound("item not found".into())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::common::test_support::temp_data_file;

    const SAMPLE: &str = r#"[
        {"id": 1, "name": "Laptop Pro", "category": "Electronics", "price": 2499},
        {"id": 2, "name": "Standing Desk", "category": "Furniture", "price": 1199}
    ]"#;

    #[test]
    fn concurrent_reads_parse_the_file_once() {
        let handler = &Handler::new(temp_data_file(SAMPLE));
        let barrier = &Barrier::new(16);

        let results: Vec<Arc<Vec<Item>>> = thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(move || {
                        barrier.wait();
                        handler.read_data().unwrap()
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });

        // Every thread got the very same parsed list, so the file was parsed once.
        assert!(results.iter().all(|items| Arc::ptr_eq(items, &results[0])));
        assert_eq!(results[0].len(), 2);
    }

    #[test]
    fn page_slice_handles_boundaries() {
        let items = [1, 2, 3, 4, 5];
        assert_eq!(page_slice(&items, 1, 2), &[1, 2]);
        assert_eq!(page_slice(&items, 3, 2), &[5]);
        assert!(page_slice(&items, 4, 2).is_empty());
        assert!(page_slice(&items, i64::MAX, 200).is_empty());
    }

    #[test]
    fn validate_rejects_blank_name_and_negative_or_missing_price() {
        let input = |name: Option<&str>, price: Option<f64>| ItemInput {
            name: name.map(str::to_owned),
            category: None,
            price,
        };
        assert!(input(None, Some(1.0)).validate().is_err());
        assert!(input(Some("  \t "), Some(1.0)).validate().is_err());
        assert!(input(Some("Mouse"), Some(-0.01)).validate().is_err());
        assert!(input(Some("Mouse"), None).validate().is_err());

        let valid = input(Some("  Mouse "), Some(0.0)).validate().unwrap();
        assert_eq!((valid.name.as_str(), valid.price), ("Mouse", 0.0));
    }
}
