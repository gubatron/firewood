// Copyright (C) 2023, Ava Labs, Inc. All rights reserved.
// See the file LICENSE.md for licensing terms.

use std::{collections::VecDeque, path::Path};

use firewood::{
    db::{BatchOp, Db, DbConfig, DbError, Proposal, Revision, WalConfig},
    merkle::{Node, TrieHash},
    storage::StoreRevShared,
    v2::api::Proof,
};
use shale::compact::CompactSpace;

type SharedStore = CompactSpace<Node, StoreRevShared>;

/// cargo run --example rev
fn main() -> Result<(), DbError> {
    let cfg = DbConfig::builder().wal(WalConfig::builder().max_revisions(10).build());

    let db = Db::new("rev_db", &cfg.clone().truncate(true).build())
        .expect("db initiation should succeed");
    let items = vec![("dof", "verb"), ("doe", "reindeer"), ("dog", "puppy")];

    let mut revision_tracker = RevisionTracker::new(db);

    revision_tracker.create_revisions(items.into_iter())?;

    revision_tracker.db.kv_dump(&mut std::io::stdout())?;

    verify_root_hashes(&mut revision_tracker);

    let revision_tracker = revision_tracker.with_new_db("rev_db", &cfg.truncate(false).build());

    let revision = revision_tracker.get_revision(0);
    let revision_root_hash = revision
        .kv_root_hash()
        .expect("root-hash for revision-0 should exist");
    println!("{revision_root_hash:?}");

    let current_root_hash = revision_tracker
        .db
        .kv_root_hash()
        .expect("root-hash for current state should exist");
    // The following is true as long as the current state is fresh after replaying from Wals.
    assert_eq!(revision_root_hash, current_root_hash);

    let rev1 = revision_tracker.get_revision(1);
    let rev2 = revision_tracker.get_revision(2);

    std::thread::scope(move |scope| {
        scope.spawn(|| {
            let revision = rev1;
            revision.kv_dump(&mut std::io::stdout()).unwrap();

            let mut items_rev = vec![("dof", "verb"), ("doe", "reindeer")];
            items_rev.sort();

            let items = items_rev;
            std::thread::scope(|scope| {
                for _ in 0..3 {
                    scope.spawn(|| {
                        let (keys, vals) = items.iter().cloned().unzip();

                        let proof = build_proof(&revision, &items);
                        revision
                            .verify_range_proof(
                                proof,
                                items.first().unwrap().0,
                                items.last().unwrap().0,
                                keys,
                                vals,
                            )
                            .unwrap();
                    });
                }
            })
        });

        scope.spawn(|| {
            let revision = rev2;
            let revision_root_hash = revision
                .kv_root_hash()
                .expect("root-hash for revision-2 should exist");
            println!("{revision_root_hash:?}");
            revision.kv_dump(&mut std::io::stdout()).unwrap();

            let mut items_rev = vec![("dof", "verb")];
            items_rev.sort();
            let items = items_rev;

            let (keys, vals) = items.iter().cloned().unzip();

            let proof = build_proof(&revision, &items);
            revision
                .verify_range_proof(
                    proof,
                    items.first().unwrap().0,
                    items.last().unwrap().0,
                    keys,
                    vals,
                )
                .unwrap();
        });
    });
    Ok(())
}

struct RevisionTracker {
    hashes: VecDeque<TrieHash>,
    db: Db,
}

impl RevisionTracker {
    fn new(db: Db) -> Self {
        Self {
            hashes: VecDeque::new(),
            db,
        }
    }

    fn create_revisions<K, V>(
        &mut self,
        mut iter: impl Iterator<Item = (K, V)>,
    ) -> Result<(), DbError>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        iter.try_for_each(|(k, v)| self.create_revision(k, v))
    }

    fn create_revision<K, V>(&mut self, k: K, v: V) -> Result<(), DbError>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let batch = vec![BatchOp::Put {
            key: k,
            value: v.as_ref().to_vec(),
        }];
        self.db.new_proposal(batch)?.commit()?;

        let hash = self.db.kv_root_hash().expect("root-hash should exist");
        self.hashes.push_front(hash);
        Ok(())
    }

    fn commit_proposal(&mut self, proposal: Proposal) {
        proposal.commit().unwrap();
        let hash = self.db.kv_root_hash().expect("root-hash should exist");
        self.hashes.push_front(hash);
    }

    fn with_new_db(self, path: impl AsRef<Path>, cfg: &DbConfig) -> Self {
        let hashes = {
            // must name db variable to explictly drop
            let Self { hashes, db: _db } = self;
            hashes
        };

        let db = Db::new(path, cfg).expect("db initiation should succeed");

        Self { hashes, db }
    }

    fn get_revision(&self, index: usize) -> Revision<SharedStore> {
        self.db
            .get_revision(&self.hashes[index])
            .unwrap_or_else(|| panic!("revision-{index} should exist"))
    }
}

fn build_proof(revision: &Revision<SharedStore>, items: &[(&str, &str)]) -> Proof<Vec<u8>> {
    let mut proof = revision.prove(items[0].0).unwrap();
    let end = revision.prove(items.last().unwrap().0).unwrap();
    proof.concat_proofs(end);
    proof
}

fn verify_root_hashes(revision_tracker: &mut RevisionTracker) {
    let revision = revision_tracker.get_revision(0);
    let revision_root_hash = revision
        .kv_root_hash()
        .expect("root-hash for revision-0 should exist");
    println!("{revision_root_hash:?}");

    let current_root_hash = revision_tracker
        .db
        .kv_root_hash()
        .expect("root-hash for current state should exist");

    // The following should always hold.
    assert_eq!(revision_root_hash, current_root_hash);

    let revision = revision_tracker.get_revision(2);
    let revision_root_hash = revision
        .kv_root_hash()
        .expect("root-hash for revision-2 should exist");
    println!("{revision_root_hash:?}");

    // Get a revision while a batch is active.
    let revision = revision_tracker.get_revision(1);
    let revision_root_hash = revision
        .kv_root_hash()
        .expect("root-hash for revision-1 should exist");
    println!("{revision_root_hash:?}");

    let batch = vec![BatchOp::Put {
        key: "k",
        value: vec![b'v'],
    }];
    let proposal = revision_tracker.db.new_proposal(batch).unwrap();

    let actual_revision_root_hash = revision_tracker
        .get_revision(1)
        .kv_root_hash()
        .expect("root-hash for revision-1 should exist");
    assert_eq!(revision_root_hash, actual_revision_root_hash);

    // Uncommitted changes of proposal cannot be seen from db.
    assert!(revision_tracker.db.kv_get("k").is_err());

    revision_tracker.commit_proposal(proposal);

    let new_revision_root_hash = revision_tracker
        .get_revision(1)
        .kv_root_hash()
        .expect("root-hash for revision-1 should exist");
    assert_ne!(revision_root_hash, new_revision_root_hash);
    let val = revision_tracker.db.kv_get("k").unwrap();
    assert_eq!("v".as_bytes().to_vec(), val);

    // When reading a specific revision, after new commits the revision remains consistent.
    let val = revision.kv_get("k");
    assert_eq!(None, val);
    let val = revision.kv_get("dof").unwrap();
    assert_eq!("verb".as_bytes().to_vec(), val);
    let actual_revision_root_hash = revision
        .kv_root_hash()
        .expect("root-hash for revision-2 should exist");
    assert_eq!(revision_root_hash, actual_revision_root_hash);
}
