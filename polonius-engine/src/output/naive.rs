// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A version of the Naive datalog analysis using Datafrog.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::output::Output;
use facts::{AllFacts, Atom};

use datafrog::{Iteration, Relation};

pub(super) fn compute<Region: Atom, Loan: Atom, Point: Atom>(
    dump_enabled: bool,
    mut all_facts: AllFacts<Region, Loan, Point>,
) -> Output<Region, Loan, Point> {
    let all_points: BTreeSet<Point> = all_facts
        .cfg_edge
        .iter()
        .map(|&(p, _)| p)
        .chain(all_facts.cfg_edge.iter().map(|&(_, q)| q))
        .collect();

    for &r in &all_facts.universal_region {
        for &p in &all_points {
            all_facts.region_live_at.push((r, p));
        }
    }

    let mut result = Output::new(dump_enabled);

    let timer = Instant::now();

    let errors = {
        // Create a new iteration context, ...
        let mut iteration = Iteration::new();

        // .. some variables, ..
        let subset = iteration.variable::<(Region, Region, Point)>("subset");
        let requires = iteration.variable::<(Region, Loan, Point)>("requires");
        let borrow_live_at = iteration.variable::<(Loan, Point)>("borrow_live_at");
        
        // `invalidates` facts, stored ready for joins
        let invalidates = iteration.variable::<((Loan, Point), ())>("invalidates");

        // different indices for `subset`.
        let subset_r1p = iteration.variable_indistinct("subset_r1p");
        let subset_r2p = iteration.variable_indistinct("subset_r2p");
        let subset_p = iteration.variable_indistinct("subset_p");

        // different indexes for `requires`.
        let requires_rp = iteration.variable_indistinct("requires_rp");
        let requires_bp = iteration.variable_indistinct("requires_bp");

        // temporaries as we perform a multi-way join.
        let subset_1 = iteration.variable_indistinct("subset_1");
        let subset_2 = iteration.variable_indistinct("subset_2");
        let requires_1 = iteration.variable_indistinct("requires_1");
        let requires_2 = iteration.variable_indistinct("requires_2");

        let killed = all_facts.killed.into();
        let region_live_at = iteration.variable::<((Region, Point), ())>("region_live_at");
        let cfg_edge_p = iteration.variable::<(Point, Point)>("cfg_edge_p");
        
        let errors = iteration.variable("errors");

        // load initial facts.
        subset.insert(all_facts.outlives.into());
        requires.insert(all_facts.borrow_region.into());
        region_live_at.insert(Relation::from(
            all_facts.region_live_at.iter().map(|&(r, p)| ((r, p), ())),
        ));
        invalidates.insert(Relation::from(
            all_facts.invalidates.iter().map(|&(p, b)| ((b, p), ())),
        ));
        cfg_edge_p.insert(all_facts.cfg_edge.clone().into());

        // .. and then start iterating rules!
        while iteration.changed() {
            // Cleanup step: remove symmetries
            // - remove regions which are `subset`s of themselves
            //
            // FIXME: investigate whether is there a better way to do that without complicating
            // the rules too much, because it would also require temporary variables and
            // impact performance. Until then, the big reduction in tuples improves performance
            // a lot, even if we're potentially adding a small number of tuples
            // per round just to remove them in the next round.
            subset
                .recent
                .borrow_mut()
                .elements
                .retain(|&(r1, r2, _)| r1 != r2);

            // remap fields to re-index by keys.
            subset_r1p.from_map(&subset, |&(r1, r2, p)| ((r1, p), r2));
            subset_r2p.from_map(&subset, |&(r1, r2, p)| ((r2, p), r1));
            subset_p.from_map(&subset, |&(r1, r2, p)| (p, (r1, r2)));

            requires_rp.from_map(&requires, |&(r, b, p)| ((r, p), b));
            requires_bp.from_map(&requires, |&(r, b, p)| ((b, p), r));

            // subset(R1, R2, P) :- outlives(R1, R2, P).
            // Already loaded; outlives is static.

            // subset(R1, R3, P) :-
            //   subset(R1, R2, P),
            //   subset(R2, R3, P).
            subset.from_join(&subset_r2p, &subset_r1p, |&(_r2, p), &r1, &r3| (r1, r3, p));

            // subset(R1, R2, Q) :-
            //   subset(R1, R2, P),
            //   cfg_edge(P, Q),
            //   region_live_at(R1, Q),
            //   region_live_at(R2, Q).

            subset_1.from_join(&subset_p, &cfg_edge_p, |&_p, &(r1, r2), &q| ((r1, q), r2));
            subset_2.from_join(&subset_1, &region_live_at, |&(r1, q), &r2, &()| {
                ((r2, q), r1)
            });
            subset.from_join(&subset_2, &region_live_at, |&(r2, q), &r1, &()| (r1, r2, q));

            // requires(R2, B, P) :-
            //   requires(R1, B, P),
            //   subset(R1, R2, P).
            requires.from_join(&requires_rp, &subset_r1p, |&(_r1, p), &b, &r2| (r2, b, p));

            // requires(R, B, Q) :-
            //   requires(R, B, P),
            //   !killed(B, P),
            //   cfg_edge(P, Q),
            //   region_live_at(R, Q).
            requires_1.from_antijoin(&requires_bp, &killed, |&(b, p), &r| (p, (b, r)));
            requires_2.from_join(&requires_1, &cfg_edge_p, |&_p, &(b, r), &q| ((r, q), b));
            requires.from_join(&requires_2, &region_live_at, |&(r, q), &b, &()| (r, b, q));

            // borrow_live_at(B, P) :- requires(R, B, P), region_live_at(R, P)
            borrow_live_at.from_join(&requires_rp, &region_live_at, |&(_r, p), &b, &()| (b, p));
            
            new_borrow_live_at.from_map(&borrow_live_at, |&(b, p)| ((b, p), ()));
            
            // .decl errors(B, P) :- invalidates(B, P), borrow_live_at(B, P).
            errors.from_join(&invalidates, &borrow_live_at, |&(b, p), &(), &()| (b, p));
        }

        if dump_enabled {
            let subset = subset.complete();
            assert!(
                subset.iter().filter(|&(r1, r2, _)| r1 == r2).count() == 0,
                "unwanted subset symmetries"
            );
            for (r1, r2, location) in &subset.elements {
                result
                    .subset
                    .entry(*location)
                    .or_insert(BTreeMap::new())
                    .entry(*r1)
                    .or_insert(BTreeSet::new())
                    .insert(*r2);
            }

            let requires = requires.complete();
            for (region, borrow, location) in &requires.elements {
                result
                    .restricts
                    .entry(*location)
                    .or_insert(BTreeMap::new())
                    .entry(*region)
                    .or_insert(BTreeSet::new())
                    .insert(*borrow);
            }

            let region_live_at = region_live_at.complete();
            for ((region, location), _) in &region_live_at.elements {
                result
                    .region_live_at
                    .entry(*location)
                    .or_insert(vec![])
                    .push(*region);
            }
            
            let borrow_live_at = borrow_live_at.complete();
            for ((borrow, location), ()) in &borrow_live_at.elements {
                result
                    .borrow_live_at
                    .entry(*location)
                    .or_insert(Vec::new())
                    .push(*borrow);
            }
        }

        errors.complete()
    };

    if dump_enabled {
        println!(
            "errors is complete: {} tuples, {:?}",
            errors.len(),
            timer.elapsed()
        );
    }

    for (borrow, location) in &errors.elements {
        result
            .errors
            .entry(*location)
            .or_insert(Vec::new())
            .push(*borrow);
    }

    result
}
