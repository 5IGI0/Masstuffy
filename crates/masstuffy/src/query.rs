//! Index-backed queries: `search`, `find_by_url`, `find_by_id` and their `get_by_*` variants
//! (spec 04 4.3).
//!
//! Every query here is eventually consistent: it reflects the secondary index, which may lag
//! acknowledged puts. Call [`Repository::sync_index`](crate::Repository::sync_index) first for
//! read-your-writes. [`Scan`](crate::Scan) reads the CDX and is not affected.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use masstuffy_collection::CollectionId;
use masstuffy_index::{CollectionKey, Hit, PatternQuery, SecondaryIndex};
use masstuffy_urlkey::UrlPattern;
use masstuffy_warc::WarcDate;
use tokio::sync::OwnedSemaphorePermit;

use crate::collection::{CollectionHandle, Shared};
use crate::error::Error;
use crate::read::{Fetch, Fetched, read_blocking, snapshot_for};
use crate::repository::{Core, Repository};
use crate::types::Located;

/// Largest `limit` of [`SearchQuery`].
pub const MAX_SEARCH_LIMIT: usize = masstuffy_index::MAX_LIMIT;
/// Default `limit` of [`SearchQuery`] (spec 4.3).
pub const DEFAULT_SEARCH_LIMIT: usize = masstuffy_index::DEFAULT_LIMIT;
/// Longest `record_id` or URL accepted by the lookups (bytes).
pub const MAX_QUERY_LEN: usize = masstuffy_cdx::DEFAULT_MAX_LINE;

/// Opaque pagination cursor of [`CollectionHandle::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCursor(masstuffy_index::Cursor);

impl SearchCursor {
    /// The textual form of this cursor.
    #[must_use]
    pub fn to_token(&self) -> String {
        self.0.to_token()
    }

    /// Parses a token produced by [`to_token`](Self::to_token).
    ///
    /// # Errors
    ///
    /// `InvalidQuery("search cursor")`.
    pub fn from_token(token: &str) -> Result<SearchCursor, Error> {
        masstuffy_index::Cursor::from_token(token)
            .map(SearchCursor)
            .map_err(|_| Error::InvalidQuery("search cursor"))
    }
}

/// Paging options of [`CollectionHandle::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Page size, `1..=MAX_SEARCH_LIMIT`. Default [`DEFAULT_SEARCH_LIMIT`].
    pub limit: usize,
    /// Continue strictly after this cursor.
    pub after: Option<SearchCursor>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        SearchQuery {
            limit: DEFAULT_SEARCH_LIMIT,
            after: None,
        }
    }
}

/// One page of a search.
#[derive(Debug)]
pub struct SearchPage {
    /// In (massaged key, date, position) order, every record type.
    pub hits: Vec<Located>,
    /// `Some` iff more hits exist.
    pub next: Option<SearchCursor>,
}

pub(crate) fn located_of(hit: Hit) -> Located {
    Located {
        collection: CollectionId::from_u128(hit.collection.get()),
        generation: hit.generation,
        entry: hit.entry,
    }
}

fn map_index_err(e: masstuffy_index::Error) -> Error {
    match e {
        masstuffy_index::Error::InvalidLimit { .. } => Error::InvalidQuery("search limit"),
        masstuffy_index::Error::InvalidCursor => Error::InvalidQuery("search cursor"),
        e => Error::IndexQuery(e),
    }
}

pub(crate) fn check_key(s: &str, what: &'static str) -> Result<(), Error> {
    if s.is_empty() || s.len() > MAX_QUERY_LEN {
        return Err(Error::InvalidQuery(what));
    }
    Ok(())
}

/// One read permit, taken after the shutdown check.
pub(crate) async fn read_permit(core: &Core) -> Result<OwnedSemaphorePermit, Error> {
    if core.closed.load(Ordering::Acquire) {
        return Err(Error::ShuttingDown);
    }
    Arc::clone(&core.read_sem)
        .acquire_owned()
        .await
        .map_err(|_| Error::ShuttingDown)
}

/// Runs `f` on the index in `spawn_blocking`, holding `permit` (and an `Arc` of the index) for the
/// job's duration; the permit is handed back so a lookup and its read can share it.
async fn index_query<T, F>(
    core: &Core,
    permit: OwnedSemaphorePermit,
    f: F,
) -> Result<(T, OwnedSemaphorePermit), Error>
where
    T: Send + 'static,
    F: FnOnce(&dyn SecondaryIndex) -> Result<T, masstuffy_index::Error> + Send + 'static,
{
    let index = core.index()?;
    tokio::task::spawn_blocking(move || {
        let r = f(&*index).map_err(map_index_err);
        r.map(|v| (v, permit))
    })
    .await
    .map_err(Error::Join)?
}

/// Reads the decoded record of `located` in `spawn_blocking` under the caller's permit.
async fn read_hit(
    core: &Arc<Core>,
    shared: &Arc<Shared>,
    located: &Located,
    permit: OwnedSemaphorePermit,
) -> Result<Vec<u8>, Error> {
    let snap = snapshot_for(shared, located.generation)?;
    let core2 = Arc::clone(core);
    let published = Arc::clone(&shared.published);
    let entry = located.entry.clone();
    let collection = located.collection;
    let fetched = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut ctx = core2.read_ctxs.take();
        let r = read_blocking(
            &snap,
            &published,
            collection,
            &entry,
            Fetch::Decoded,
            &mut ctx,
        );
        core2.read_ctxs.give(ctx);
        r
    })
    .await
    .map_err(Error::Join)??;
    match fetched {
        Fetched::Decoded(v) => Ok(v),
        Fetched::Raw(_) => Err(Error::Internal("fetch mode")),
    }
}

impl CollectionHandle {
    /// Pages through the visible entries of this collection whose massaged URL key matches
    /// `pattern` (domain with or without subdomains, exact port, exact path or prefix), in
    /// (key, date, position) order, every record type. Returns locators; fetch records with
    /// [`Repository::get`](crate::Repository::get) or
    /// [`get_raw`](crate::Repository::get_raw).
    ///
    /// Eventually consistent: reflects the index, which may lag acknowledged puts; call
    /// [`Repository::sync_index`](crate::Repository::sync_index) first for read-your-writes.
    ///
    /// # Errors
    ///
    /// `InvalidQuery` (limit outside `1..=MAX_SEARCH_LIMIT`), `ShuttingDown`, `IndexQuery`,
    /// `Join`.
    pub async fn search(
        &self,
        pattern: &UrlPattern,
        query: &SearchQuery,
    ) -> Result<SearchPage, Error> {
        if query.limit == 0 || query.limit > MAX_SEARCH_LIMIT {
            return Err(Error::InvalidQuery("search limit"));
        }
        self.shared.check_live()?;
        let permit = read_permit(&self.core).await?;
        let key = CollectionKey::new(self.shared.id.as_u128());
        let pattern = pattern.clone();
        let mut q = PatternQuery::new(query.limit);
        if let Some(c) = &query.after {
            q = q.after(c.0.clone());
        }
        let (page, _permit) = index_query(&self.core, permit, move |index| {
            index.by_pattern(key, &pattern, &q)
        })
        .await?;
        Ok(SearchPage {
            hits: page.hits.into_iter().map(located_of).collect(),
            next: page.next.map(SearchCursor),
        })
    }

    /// Index-only: the non-`request` record of this collection for `url` nearest to `date`
    /// (latest if `None`). http(s) URLs are compared by massaged key, others verbatim.
    ///
    /// Eventually consistent: reflects the index, which may lag acknowledged puts; call
    /// [`Repository::sync_index`](crate::Repository::sync_index) first for read-your-writes.
    ///
    /// # Errors
    ///
    /// `InvalidQuery` (empty or longer than `MAX_QUERY_LEN`), `ShuttingDown`, `IndexQuery`,
    /// `Join`.
    pub async fn find_by_url(
        &self,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<Located>, Error> {
        check_key(url, "url")?;
        self.shared.check_live()?;
        let permit = read_permit(&self.core).await?;
        let (hit, _permit) = self.lookup_url(url, date, permit).await?;
        Ok(hit.map(located_of))
    }

    /// [`find_by_url`](Self::find_by_url) followed by a decoded read, under one read permit.
    ///
    /// # Errors
    ///
    /// As `find_by_url`, plus `StaleLocator` (the index lags a generation switch: retry after
    /// `sync_index`), `NotDecodable` (a `HeaderOnly` raw import whose payload disagrees with its
    /// metadata: use `find_by_url` and `get_raw`), `Read`.
    pub async fn get_by_url(
        &self,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<(Located, Vec<u8>)>, Error> {
        check_key(url, "url")?;
        self.shared.check_live()?;
        let permit = read_permit(&self.core).await?;
        let (hit, permit) = self.lookup_url(url, date, permit).await?;
        let Some(located) = hit.map(located_of) else {
            return Ok(None);
        };
        let bytes = read_hit(&self.core, &self.shared, &located, permit).await?;
        Ok(Some((located, bytes)))
    }

    async fn lookup_url(
        &self,
        url: &str,
        date: Option<WarcDate>,
        permit: OwnedSemaphorePermit,
    ) -> Result<(Option<Hit>, OwnedSemaphorePermit), Error> {
        let key = CollectionKey::new(self.shared.id.as_u128());
        let url = url.to_owned();
        index_query(&self.core, permit, move |index| {
            index.by_url_nearest(key, &url, date)
        })
        .await
    }
}

impl Repository {
    /// Index-only: the most recently committed visible copy of `record_id` (spec 04 §5), in
    /// `collection` if given, else across collections (latest `WARC-Date`, then the greater
    /// collection key). The result may name a collection that is not loaded (skipped at open).
    ///
    /// Eventually consistent: reflects the index, which may lag acknowledged puts; call
    /// [`sync_index`](Self::sync_index) first for read-your-writes.
    ///
    /// # Errors
    ///
    /// `InvalidQuery` (empty or longer than `MAX_QUERY_LEN`), `UnknownCollection` (scope not
    /// loaded), `ShuttingDown`, `IndexQuery`, `Join`.
    pub async fn find_by_id(
        &self,
        record_id: &str,
        collection: Option<CollectionId>,
    ) -> Result<Option<Located>, Error> {
        check_key(record_id, "record_id")?;
        self.check_open()?;
        if let Some(id) = collection {
            self.shared(id)?;
        }
        let core = self.core();
        let permit = read_permit(core).await?;
        let (hit, _permit) = lookup_id(core, record_id, collection, permit).await?;
        Ok(hit.map(located_of))
    }

    /// [`find_by_id`](Self::find_by_id) followed by a decoded read, under one read permit.
    ///
    /// # Errors
    ///
    /// As `find_by_id`, plus `UnknownCollection` (the hit is in a collection that is not
    /// loaded), `StaleLocator` (the index lags a generation switch: retry after `sync_index`),
    /// `NotDecodable` (a `HeaderOnly` raw import whose payload disagrees with its metadata: use
    /// `find_by_id` and `get_raw`), `Read`.
    pub async fn get_by_id(
        &self,
        record_id: &str,
        collection: Option<CollectionId>,
    ) -> Result<Option<(Located, Vec<u8>)>, Error> {
        check_key(record_id, "record_id")?;
        self.check_open()?;
        if let Some(id) = collection {
            self.shared(id)?;
        }
        let core = self.core();
        let permit = read_permit(core).await?;
        let (hit, permit) = lookup_id(core, record_id, collection, permit).await?;
        let Some(located) = hit.map(located_of) else {
            return Ok(None);
        };
        let shared = self.shared(located.collection)?;
        let bytes = read_hit(core, &shared, &located, permit).await?;
        Ok(Some((located, bytes)))
    }
}

pub(crate) async fn lookup_id(
    core: &Core,
    record_id: &str,
    collection: Option<CollectionId>,
    permit: OwnedSemaphorePermit,
) -> Result<(Option<Hit>, OwnedSemaphorePermit), Error> {
    let scope = collection.map(|c| CollectionKey::new(c.as_u128()));
    let id = record_id.to_owned();
    index_query(core, permit, move |index| index.by_id(scope, &id)).await
}
