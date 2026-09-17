//! The largest-remainder distribution algorithm (spec §3.3).
//!
//! Pure, DB-free functions operating entirely on integer cents so results
//! are exact and testable via `==` — never floating point. This module
//! knows nothing about SQL; `src/pricing/api.rs` is responsible for
//! loading rows and feeding them in here.

use std::collections::BTreeMap;

/// One participant's mark on an item, as needed to determine remainder-cent
/// distribution order (`marked_at ASC, participant_id ASC` — spec §3.3,
/// "Step A").
#[derive(Debug, Clone)]
pub struct MarkerInput {
    /// Unguessable 128-bit participant token (spec §2.4), not a sequential
    /// integer — see `src/pricing/api.rs` module docs.
    pub participant_id: String,
    /// ISO-8601 timestamp string (as stored — `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`).
    /// Lexicographic string ordering is correct for this format.
    pub marked_at: String,
}

/// One item's price plus who marked it, as needed for the per-item split
/// step.
#[derive(Debug, Clone)]
pub struct ItemInput {
    pub item_id: i64,
    pub price_cents: i64,
    pub markers: Vec<MarkerInput>,
}

/// A participant's computed split, before merging with participants who
/// have zero markers (that merge is the caller's job — this module only
/// knows about participants that actually appear on at least one marker).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParticipantSplit {
    pub dish_subtotal: i64,
    pub tax_tip_share: i64,
    pub total: i64,
}

/// Full result of [`compute_split`].
#[derive(Debug, Clone, Default)]
pub struct SplitResult {
    /// `item_id -> (participant_id -> share cents)`. Items with zero
    /// markers appear with an empty inner map.
    pub per_item_shares: BTreeMap<i64, BTreeMap<String, i64>>,
    /// `participant_id -> split totals`, for participants who marked at
    /// least one item.
    pub participants: BTreeMap<String, ParticipantSplit>,
    /// Sum of prices of items with `marker_count > 0`.
    pub assigned_total: i64,
    /// Sum of prices of items with zero markers, plus the entire
    /// `tax_tip_amount` when `assigned_total == 0` (spec §3.3: "If
    /// `assigned_total == 0`, tax/tip isn't distributed — the entire
    /// amount is reported unassigned").
    pub unassigned_amount: i64,
}

/// Computes the full split for a bill's items against a given
/// `tax_tip_amount`, per spec §3.3.
///
/// Invariant (verified in tests below): `sum(participant totals) +
/// unassigned_amount == sum(item prices) + tax_tip_amount`, exactly.
pub fn compute_split(items: &[ItemInput], tax_tip_amount: i64) -> SplitResult {
    let mut per_item_shares: BTreeMap<i64, BTreeMap<String, i64>> = BTreeMap::new();
    let mut dish_subtotal: BTreeMap<String, i64> = BTreeMap::new();
    let mut assigned_total: i64 = 0;
    let mut unassigned_amount: i64 = 0;

    // --- Step A: per-item split ---
    for item in items {
        let marker_count = item.markers.len() as i64;

        if marker_count == 0 {
            unassigned_amount += item.price_cents;
            per_item_shares.insert(item.item_id, BTreeMap::new());
            continue;
        }

        assigned_total += item.price_cents;

        let base = item.price_cents / marker_count;
        let remainder = item.price_cents - base * marker_count;

        let mut ordered = item.markers.clone();
        ordered.sort_by(|a, b| {
            a.marked_at
                .cmp(&b.marked_at)
                .then(a.participant_id.cmp(&b.participant_id))
        });

        let mut shares = BTreeMap::new();
        for (i, marker) in ordered.iter().enumerate() {
            let share = base + if (i as i64) < remainder { 1 } else { 0 };
            shares.insert(marker.participant_id.clone(), share);
            *dish_subtotal.entry(marker.participant_id.clone()).or_insert(0) += share;
        }
        per_item_shares.insert(item.item_id, shares);
    }

    // --- Step B: tax/tip proportional distribution ---
    let mut participants: BTreeMap<String, ParticipantSplit> = dish_subtotal
        .iter()
        .map(|(pid, &subtotal)| {
            (
                pid.clone(),
                ParticipantSplit {
                    dish_subtotal: subtotal,
                    tax_tip_share: 0,
                    total: subtotal,
                },
            )
        })
        .collect();

    if assigned_total == 0 {
        // Nobody's marked anything yet: tax/tip is entirely unassigned.
        unassigned_amount += tax_tip_amount;
    } else if tax_tip_amount != 0 {
        // Exact-rational largest-remainder distribution: for each
        // participant, numerator = dish_subtotal * tax_tip_amount, over a
        // common denominator of `assigned_total`. Comparing remainders
        // directly (same denominator) avoids floating point entirely.
        struct Candidate {
            participant_id: String,
            base: i64,
            remainder_numerator: i64,
        }

        let mut candidates: Vec<Candidate> = dish_subtotal
            .iter()
            .filter(|&(_, &subtotal)| subtotal > 0)
            .map(|(pid, &subtotal)| {
                let numerator = subtotal * tax_tip_amount;
                let base = numerator / assigned_total;
                let remainder_numerator = numerator - base * assigned_total;
                Candidate {
                    participant_id: pid.clone(),
                    base,
                    remainder_numerator,
                }
            })
            .collect();

        let sum_base: i64 = candidates.iter().map(|c| c.base).sum();
        let mut leftover_cents = tax_tip_amount - sum_base;

        // Largest fractional part first, tie-broken by participant_id ASC.
        candidates.sort_by(|a, b| {
            b.remainder_numerator
                .cmp(&a.remainder_numerator)
                .then(a.participant_id.cmp(&b.participant_id))
        });

        for candidate in &candidates {
            let mut share = candidate.base;
            if leftover_cents > 0 {
                share += 1;
                leftover_cents -= 1;
            }
            if let Some(p) = participants.get_mut(&candidate.participant_id) {
                p.tax_tip_share = share;
                p.total = p.dish_subtotal + share;
            }
        }
        debug_assert_eq!(leftover_cents, 0);
    }
    // tax_tip_amount == 0 with assigned_total > 0: every share is
    // naturally 0, already the default.

    SplitResult {
        per_item_shares,
        participants,
        assigned_total,
        unassigned_amount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the exact worked example from spec §3.3:
    /// Burger $12.00/{Alice,Bob,Carol}, Fries $5.00/{Alice,Bob},
    /// Salad $9.00/{} (unassigned), tax_tip $4.90.
    /// Expected: Alice $8.38, Bob $8.37, Carol $5.15, unassigned $9.00.
    #[test]
    fn worked_example_from_spec() {
        // participant_id ordering: Alice, Bob, Carol (join order); string
        // tokens rather than sequential integers (spec §2.4).
        let alice = "alice".to_string();
        let bob = "bob".to_string();
        let carol = "carol".to_string();

        let items = vec![
            ItemInput {
                item_id: 1,
                price_cents: 1200,
                markers: vec![
                    MarkerInput {
                        participant_id: alice.clone(),
                        marked_at: "2026-08-12T10:00:00.000Z".into(),
                    },
                    MarkerInput {
                        participant_id: bob.clone(),
                        marked_at: "2026-08-12T10:00:01.000Z".into(),
                    },
                    MarkerInput {
                        participant_id: carol.clone(),
                        marked_at: "2026-08-12T10:00:02.000Z".into(),
                    },
                ],
            },
            ItemInput {
                item_id: 2,
                price_cents: 500,
                markers: vec![
                    MarkerInput {
                        participant_id: alice.clone(),
                        marked_at: "2026-08-12T10:00:03.000Z".into(),
                    },
                    MarkerInput {
                        participant_id: bob.clone(),
                        marked_at: "2026-08-12T10:00:04.000Z".into(),
                    },
                ],
            },
            ItemInput {
                item_id: 3,
                price_cents: 900,
                markers: vec![],
            },
        ];

        let result = compute_split(&items, 490);

        assert_eq!(result.assigned_total, 1700);
        assert_eq!(result.unassigned_amount, 900);

        let alice_split = result.participants[&alice];
        let bob_split = result.participants[&bob];
        let carol_split = result.participants[&carol];

        assert_eq!(alice_split.dish_subtotal, 650);
        assert_eq!(bob_split.dish_subtotal, 650);
        assert_eq!(carol_split.dish_subtotal, 400);

        assert_eq!(alice_split.tax_tip_share, 188);
        assert_eq!(bob_split.tax_tip_share, 187);
        assert_eq!(carol_split.tax_tip_share, 115);

        assert_eq!(alice_split.total, 838, "Alice should owe $8.38");
        assert_eq!(bob_split.total, 837, "Bob should owe $8.37");
        assert_eq!(carol_split.total, 515, "Carol should owe $5.15");

        // Invariant: sum(totals) + unassigned == sum(items) + tax_tip.
        let receipt_total = 1200 + 500 + 900 + 490;
        let sum_totals: i64 = result.participants.values().map(|p| p.total).sum();
        assert_eq!(sum_totals + result.unassigned_amount, receipt_total);
        assert_eq!(sum_totals, 2190, "sum of totals should be $21.90");
    }

    #[test]
    fn item_with_no_markers_is_fully_unassigned() {
        let items = vec![ItemInput {
            item_id: 1,
            price_cents: 999,
            markers: vec![],
        }];
        let result = compute_split(&items, 100);
        assert_eq!(result.unassigned_amount, 999 + 100);
        assert_eq!(result.assigned_total, 0);
        assert!(result.participants.is_empty());
    }

    #[test]
    fn per_item_remainder_distributed_to_earliest_markers_first() {
        // 100 cents / 3 people = 33 base, 1 cent remainder -> earliest
        // marked_at gets the extra cent.
        let items = vec![ItemInput {
            item_id: 1,
            price_cents: 100,
            markers: vec![
                MarkerInput {
                    participant_id: "p3".into(),
                    marked_at: "2026-08-12T10:00:02.000Z".into(),
                },
                MarkerInput {
                    participant_id: "p1".into(),
                    marked_at: "2026-08-12T10:00:00.000Z".into(),
                },
                MarkerInput {
                    participant_id: "p2".into(),
                    marked_at: "2026-08-12T10:00:01.000Z".into(),
                },
            ],
        }];
        let result = compute_split(&items, 0);
        let shares = &result.per_item_shares[&1];
        assert_eq!(shares["p1"], 34); // earliest marker gets the extra cent
        assert_eq!(shares["p2"], 33);
        assert_eq!(shares["p3"], 33);
        assert_eq!(shares.values().sum::<i64>(), 100);
    }

    #[test]
    fn invariant_holds_for_various_amounts_and_participant_counts() {
        // A handful of arbitrary combinations, checking the sum-exactness
        // invariant holds every time (not just the one worked example).
        type ItemSpec = (i64, i64, usize); // (item_id, price_cents, marker_count)
        let cases: Vec<(Vec<ItemSpec>, i64)> = vec![
            // (item specs), tax_tip_amount
            (vec![(1, 1000, 3), (2, 333, 7), (3, 1, 2)], 250),
            (vec![(1, 1, 1)], 0),
            (vec![(1, 0, 3)], 10),
            (vec![(1, 12345, 4), (2, 6789, 1)], 999),
        ];

        for (item_specs, tax_tip_amount) in cases {
            let mut next_participant_id = 1u32;
            let mut items = Vec::new();
            let mut receipt_total = tax_tip_amount;
            for (item_id, price_cents, marker_count) in item_specs {
                receipt_total += price_cents;
                let markers = (0..marker_count)
                    .map(|i| {
                        let pid = format!("p{next_participant_id}");
                        next_participant_id += 1;
                        MarkerInput {
                            participant_id: pid,
                            marked_at: format!("2026-08-12T10:{:02}:00.000Z", i),
                        }
                    })
                    .collect();
                items.push(ItemInput {
                    item_id,
                    price_cents,
                    markers,
                });
            }

            let result = compute_split(&items, tax_tip_amount);
            let sum_totals: i64 = result.participants.values().map(|p| p.total).sum();
            assert_eq!(
                sum_totals + result.unassigned_amount,
                receipt_total,
                "invariant violated for case with tax_tip={tax_tip_amount}"
            );

            // Also verify each item's shares sum exactly to its price.
            for item in &items {
                if item.markers.is_empty() {
                    continue;
                }
                let sum: i64 = result.per_item_shares[&item.item_id].values().sum();
                assert_eq!(sum, item.price_cents);
            }
        }
    }
}
