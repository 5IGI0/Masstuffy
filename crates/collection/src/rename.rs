//! Slug rewrite (rename): the manifest is replaced atomically, nothing else is touched.

use std::sync::Arc;

use crate::error::{Error, ManifestError};
use crate::layout::MANIFEST_NAME;
use crate::manifest::Manifest;
use crate::snapshot::Snapshot;

/// Rewrites `manifest.json` of `source`'s collection with `slug` (same uuid, split threshold,
/// active generation, next_gen_id) and returns a snapshot of the same generation carrying the
/// new manifest (same `GenContext` `Arc`, a clone of the same `seglog::Reader`: no file is
/// reopened).
///
/// Preconditions (engine: the writer task runs it, under the collection's admin mutex): no
/// rebuild, `Collection::open`, other manifest write or `sweep_tmp` of this directory runs
/// concurrently.
///
/// Order: `Manifest::load(dir)` must equal `**source.manifest()` (else `StaleSource("manifest")`);
/// build the new `Manifest` with `Manifest::new` (slug validation: 1..=256 bytes, no control
/// character); if it equals the on-disk one, return a clone of `source` without any write;
/// else `to_json()` + `b'\n'`, then `masstuffy_durable::atomic_write(dir, MANIFEST_NAME, &json)`:
/// the commit point. The warcinfo records are not touched (the first generation's warcinfo keeps
/// the initial manifest, spec 02; the next rebuild writes a warcinfo with the new slug).
///
/// # Errors
///
/// `StaleSource("manifest")`, `ManifestMissing`, `Io`, `Manifest { path, source: InvalidSlug }`
/// (nothing written), `Durable` (from `atomic_write`: with op `SyncDir` the new manifest is in
/// the namespace but maybe not durable; with any other op nothing was replaced).
pub fn set_slug(source: &Snapshot, slug: &str) -> Result<Snapshot, Error> {
    let dir = source.reader().dir();
    if Manifest::load(dir)? != **source.manifest() {
        return Err(Error::StaleSource("manifest"));
    }
    let old = source.manifest();
    let new = Manifest::new(
        old.uuid(),
        slug,
        old.split_threshold(),
        old.active(),
        old.next_gen_id(),
    )
    .map_err(|source: ManifestError| Error::Manifest {
        path: dir.join(MANIFEST_NAME),
        source,
    })?;
    if new == **old {
        return Ok(source.with_manifest(Arc::clone(old)));
    }
    let mut json = new.to_json().map_err(|source| Error::Manifest {
        path: dir.join(MANIFEST_NAME),
        source,
    })?;
    json.push(b'\n');
    // Commit point.
    masstuffy_durable::atomic_write(dir, MANIFEST_NAME, &json)?;
    Ok(source.with_manifest(Arc::new(new)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::Collection;
    use crate::manifest::{Codec, Generation};
    use crate::prepare::Prepared;
    use crate::testutil::{
        MapResolver, TestDir, create_none, create_zstd, listing, names, record, uuid,
    };
    use crate::{DEFAULT_SPLIT_THRESHOLD, Options, RecoveryOptions};
    use crate::{ReadCtx, RebuildOptions};
    use std::fs;
    use std::path::Path;

    type Create = fn(&Path) -> (Collection, masstuffy_cdx::Entry<'static>);

    fn creators() -> [Create; 2] {
        [create_none, create_zstd]
    }

    fn put(c: &mut Collection, n: usize) -> Vec<masstuffy_cdx::Entry<'static>> {
        let mut p = c.preparer().unwrap();
        let batch: Vec<Prepared> = (0..n).map(|i| p.prepare(record(i, 50)).unwrap()).collect();
        c.writer().commit(&batch).unwrap()
    }

    fn reopen(dir: &Path) -> Collection {
        Collection::open(
            dir,
            &Options::default(),
            &RecoveryOptions::default(),
            &MapResolver::standard(),
        )
        .unwrap()
        .0
    }

    #[test]
    fn set_slug_roundtrip() {
        for create in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, wi) = create(&dir);
            let entries = put(&mut c, 3);
            let before: Vec<_> = listing(&dir)
                .into_iter()
                .filter(|(n, _)| n != "manifest.json")
                .collect();
            let snap = set_slug(c.snapshot(), "b").unwrap();
            assert_eq!(snap.manifest().slug(), "b");
            assert_eq!(snap.generation(), c.snapshot().generation());
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            for e in &entries {
                snap.read(e, &mut ctx, &mut buf).unwrap();
            }
            assert_eq!(Manifest::load(&dir).unwrap().slug(), "b");
            let after: Vec<_> = listing(&dir)
                .into_iter()
                .filter(|(n, _)| n != "manifest.json")
                .collect();
            assert_eq!(before, after);
            assert!(names(&dir).iter().all(|n| !n.starts_with(".tmp-")));
            drop(c);
            let c = reopen(&dir);
            assert_eq!(c.manifest().slug(), "b");
            // The warcinfo of generation 1 keeps the initial manifest.
            let rec = c.snapshot().read(&wi, &mut ctx, &mut buf).unwrap();
            let body = std::str::from_utf8(rec.body()).unwrap();
            assert!(body.contains("\"slug\":\"test\""), "{body}");
        }
    }

    #[test]
    fn set_slug_same_is_noop() {
        let d = TestDir::new();
        let dir = d.sub();
        let (c, _) = create_none(&dir);
        let before = fs::read(dir.join("manifest.json")).unwrap();
        let snap = set_slug(c.snapshot(), "test").unwrap();
        assert_eq!(snap.manifest().slug(), "test");
        assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), before);
    }

    #[test]
    fn set_slug_invalid() {
        let d = TestDir::new();
        let dir = d.sub();
        let (c, _) = create_none(&dir);
        let before = fs::read(dir.join("manifest.json")).unwrap();
        for bad in ["", "a\nb"] {
            let r = set_slug(c.snapshot(), bad);
            assert!(matches!(
                r.err(),
                Some(Error::Manifest {
                    source: ManifestError::InvalidSlug(_),
                    ..
                })
            ));
        }
        assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), before);
    }

    #[test]
    fn set_slug_stale_source() {
        let d = TestDir::new();
        let dir = d.sub();
        let (c, _) = create_none(&dir);
        let other = Manifest::new(
            uuid(0),
            "other",
            DEFAULT_SPLIT_THRESHOLD,
            Generation {
                id: 1,
                codec: Codec::None,
            },
            2,
        )
        .unwrap();
        let mut json = other.to_json().unwrap();
        json.push(b'\n');
        masstuffy_durable::atomic_write(&dir, MANIFEST_NAME, &json).unwrap();
        assert!(matches!(
            set_slug(c.snapshot(), "b").err(),
            Some(Error::StaleSource("manifest"))
        ));
    }

    #[test]
    fn rebuild_after_set_slug() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        put(&mut c, 3);
        c.set_slug("b").unwrap();
        let (_report, old) = c
            .rebuild(
                Codec::None,
                &Options::default(),
                RebuildOptions::default(),
                &MapResolver::standard(),
            )
            .unwrap();
        old.remove().unwrap();
        let m = Manifest::load(&dir).unwrap();
        assert_eq!(m.slug(), "b");
        assert_eq!(m.active().id, 2);
        let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
        let mut es = c.entries().unwrap();
        let wi = es.next_entry().unwrap().unwrap().1.into_owned();
        let rec = c.snapshot().read(&wi, &mut ctx, &mut buf).unwrap();
        let body = std::str::from_utf8(rec.body()).unwrap();
        assert!(body.contains("\"slug\":\"b\""), "{body}");
    }
}
