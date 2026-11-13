# Engineering Assessment (Rust) – Solution

Rust backend for the item catalog in `data/items.json`.
It completes the three bug fixes and the two new endpoints from the assessment brief.

## Running

Requires Rust 1.85+ (stable, via rustup).

```bash
cd backend-rust
cargo run      # serves on :4001 and reads ../data/items.json
cargo test     # unit and HTTP-level tests
```

`PORT` and `DATA_PATH` environment variables override the defaults.

The original frontend is not included in this repository. The API it uses is unchanged:
`GET /api/items` returns `{ items, total, page, pageSize, q }`, and `GET /api/items/:id` returns one item.

## API

| Method | Path              | Success                     | Errors                           |
| ------ | ----------------- | --------------------------- | -------------------------------- |
| GET    | `/api/items`      | 200 paginated list          | 400 non-numeric `page`/`pageSize` |
| GET    | `/api/items/{id}` | 200 item                    | 400 invalid id, 404 not found     |
| POST   | `/api/items`      | 201 created item            | 400 invalid body                  |
| PUT    | `/api/items/{id}` | 200 updated item            | 400 invalid id/body, 404 not found |
| DELETE | `/api/items/{id}` | 204, no body                | 400 invalid id, 404 not found     |
| GET    | `/api/stats`      | 200 `{ total, averagePrice }` |                                  |

Every error response has a JSON body: `{ "error": "<message>" }`.
This includes 404 for unknown routes and 405 for unsupported methods (with an `Allow` header).

## Task 1: bug fixes

**Bug 1: cache race in `items::Handler::read_data`.**
The original code dropped the read lock after checking whether the cache was fresh.
It then re-parsed the file with no lock held, so many threads could re-parse at once and overwrite each other's cache entry.
The fix uses double-checked locking. A fresh cache is served under the shared read lock.
A stale cache is checked again and refreshed while holding the write lock.
This makes the check-then-act sequence atomic: exactly one thread re-reads the file, and the others reuse its result.
The test `concurrent_reads_parse_the_file_once` checks this: 16 threads read at the same moment and all receive the same parsed list.

**Bug 2: no input validation in `create`.**
`ItemInput::validate` is shared by POST and PUT. It returns 400 with a JSON error when:

- `name` is missing or blank (whitespace only counts as blank)
- `price` is negative
- `price` is missing (stricter than the brief, so an item can't be saved without a price)

Malformed or empty JSON bodies also return 400 with a JSON error. Names and categories are trimmed.

**Bug 3: stats recomputed on every request.**
`stats::Handler` caches the computed `Stats` together with the file's modified time.
It recomputes only when `items.json` has changed since, using the same double-checked `RwLock` pattern.
The test `stats_are_cached_until_the_file_changes` rewrites the file but restores its old mtime, and checks that the cached value is served.

## Task 2: new endpoints

- **`PUT /api/items/{id}`** validates the body with the same rules as POST and returns 404 for an unknown id.
  It updates `name`, `category` and `price`; an `id` in the body is ignored. It saves the change and returns the updated item.
- **`DELETE /api/items/{id}`** returns 404 for an unknown id. Otherwise it removes the item, saves the list and returns 204 with no body.

## Other improvements

- **No lost updates.** POST, PUT and DELETE each run their whole read-modify-write while holding a write mutex.
  Without it, two concurrent writes could read the same list and one change would be lost.
  The test `concurrent_creates_do_not_lose_updates` sends 50 parallel POSTs and checks they are all kept.
- **No torn reads.** Writes go to a temporary file that is then renamed over `items.json`, so a reader never sees a half-written file.
- **Sequential IDs.** New IDs are the current maximum id + 1, assigned under the write lock.
  The original used a millisecond timestamp, which could collide under concurrent requests.
- **Pagination edge cases.** `page < 1` becomes 1. `pageSize` above 200 is capped at 200, and below 1 falls back to 50.
  A page past the end returns an empty `items` list with the correct `total`. Offsets use saturating arithmetic, so huge page numbers can't overflow.
- **Error handling.** Handlers return `Result<_, ApiError>`, and `ApiError` maps to the right status code with a JSON body.
  There is no `unwrap()` on request paths. Internal errors are logged on the server but not leaked to clients.
