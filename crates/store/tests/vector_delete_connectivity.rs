//! Deleting vectors must not strand the vectors that were only reachable through them
//! (audit 2026-10-30, finding S-2).

use fl_store::{FerroStore, VectorMetric, VectorStorage};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f32 {
        ((self.next() >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }
}

fn point(rng: &mut Rng) -> Vec<f32> {
    (0..6).map(|_| rng.unit()).collect()
}

/// Insert `total` vectors, delete every `delete_every`th, and count the survivors that a search
/// for their own value does not return first. The search width exceeds the collection, so a miss
/// means the vector is unreachable in the graph, not a recall shortfall.
fn unreachable_after_deleting(
    store: &FerroStore,
    total: usize,
    delete_every: usize,
    m: u32,
) -> (usize, usize) {
    store
        .create_vector_collection("c", 6, VectorStorage::F32, VectorMetric::L2, m, 32, 16)
        .unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut points = Vec::new();
    for i in 0..total {
        let id = format!("v{i:04}");
        let values = point(&mut rng);
        store.upsert_vector("c", &id, &values, None).unwrap();
        points.push((id, values));
    }
    let mut kept = Vec::new();
    for (i, (id, values)) in points.into_iter().enumerate() {
        if i % delete_every == 0 {
            store.delete_vector("c", &id).unwrap();
        } else {
            kept.push((id, values));
        }
    }
    let mut missing = 0;
    for (id, values) in &kept {
        let hits = store.search_vectors("c", values, 1, Some(1000)).unwrap();
        if hits.first().map(|h| h.0.as_str()) != Some(id.as_str()) {
            missing += 1;
        }
    }
    (missing, kept.len())
}

/// Dropping a deleted node's links without reconnecting its neighbours stranded the vectors that
/// were only reachable through it: at m=2, deleting every second of 500 vectors left 243 of the
/// 250 survivors unfindable. Reachability after deleting must stay close to that of a collection
/// built fresh from the survivors (which, at small `m`, is itself not perfect).
#[test]
fn vectors_stay_findable_after_neighbours_are_deleted() {
    for (total, every, m) in [
        (300, 2, 4),
        (300, 3, 4),
        (300, 2, 8),
        (500, 2, 2),
        (800, 4, 4),
    ] {
        let (missing, kept) = unreachable_after_deleting(&FerroStore::in_memory(), total, every, m);
        let (control, _) =
            unreachable_after_deleting(&FerroStore::in_memory(), kept, 100_000_000, m);
        let allowed = control + kept / 20;
        assert!(
            missing <= allowed,
            "{missing} of {kept} survivors are unreachable after deletes (a fresh build of the \
             same size loses {control}, allowed {allowed}); n={total}, delete every {every}th, m={m}"
        );
    }
}

/// The repaired graph must still satisfy every invariant a checkpoint re-validates on load
/// (symmetric links, at most `max_links`, sorted neighbours, a canonical entry point).
#[test]
fn a_collection_with_deletes_checkpoints_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.fls");
    let (missing_before, kept) = {
        let store = FerroStore::open(&path).unwrap();
        let out = unreachable_after_deleting(&store, 60, 2, 3);
        store.checkpoint().unwrap();
        out
    };
    let reopened =
        FerroStore::open(&path).expect("a graph repaired after deletes failed validation");
    let mut rng = Rng(7);
    let hits = reopened
        .search_vectors("c", &point(&mut rng), 5, Some(1000))
        .unwrap();
    assert_eq!(hits.len(), 5);
    assert!(missing_before <= kept / 5, "{missing_before} of {kept}");
}

#[test]
fn deleting_everything_then_reinserting_works() {
    let store = FerroStore::in_memory();
    store
        .create_vector_collection("c", 6, VectorStorage::F32, VectorMetric::Cosine, 4, 32, 16)
        .unwrap();
    let mut rng = Rng(11);
    let values: Vec<Vec<f32>> = (0..40).map(|_| point(&mut rng)).collect();
    for round in 0..2 {
        for (i, v) in values.iter().enumerate() {
            store
                .upsert_vector("c", &format!("v{i:02}"), v, None)
                .unwrap();
        }
        for i in (0..values.len()).rev() {
            store.delete_vector("c", &format!("v{i:02}")).unwrap();
            // The collection stays searchable at every size, down to empty.
            let hits = store.search_vectors("c", &values[0], 3, None).unwrap();
            assert!(hits.len() <= i.min(3), "round {round}, {i} left, {hits:?}");
        }
        assert!(
            store
                .search_vectors("c", &values[0], 3, None)
                .unwrap()
                .is_empty()
        );
    }
}
