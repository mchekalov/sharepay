//! Reconciliation / validation logic (spec §1.4): compares the parsed
//! items' sum against the recognized subtotal/total, in integer cents
//! throughout (never floating point), producing either a hard-mismatch
//! outcome or a success outcome carrying a derived `tax_tip_amount` and a
//! confidence flag.

/// Bill-level confidence in the reconciled result (spec §1.4/§1.7) — never
/// fails validation on its own, purely a hint for the host-confirmation
/// step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverallConfidence {
    High,
    Low,
}

/// Inputs to [`reconcile`]. `total_cents` is mandatory here — a receipt
/// with no identifiable total at all fails earlier, at the parsing stage
/// (spec §1.3, step 4: "If nothing qualifies, `recognized_total = None`
/// and the receipt fails validation — never guess silently"), and never
/// reaches reconciliation.
#[derive(Debug, Clone)]
pub struct ReconcileInput {
    pub items_sum_cents: i64,
    pub num_items: usize,
    /// The `subtotal` line's price, if one was found and classified.
    pub subtotal_cents: Option<i64>,
    /// An explicit `tax`/`tip_service` line's price, if found — used only
    /// as a cross-check against the subtotal-derived tax/tip figure, never
    /// as the primary source (spec §1.4, step 1).
    pub tax_line_cents: Option<i64>,
    pub total_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Success {
        tax_tip_amount_cents: i64,
        overall_confidence: OverallConfidence,
        /// `true` when the derived tax/tip figure disagreed with an
        /// explicit OCR'd tax line by more than the tolerance (subtotal
        /// path), or when the no-subtotal shortfall gap exceeded 3% of the
        /// total (total-only path) — surfaced to the host-confirmation UI
        /// rather than failing validation (spec §1.4).
        tax_tip_unconfirmed: bool,
    },
    Mismatch {
        computed_sum_cents: i64,
        discrepancy_cents: i64,
    },
}

/// Tolerance for comparing `sum(items)` to an explicit subtotal line (spec
/// §1.4, step 1): `max(2, num_items)` cents — a little slack per item for
/// rounding, with a 2-cent floor.
pub fn subtotal_tolerance_cents(num_items: usize) -> i64 {
    (num_items as i64).max(2)
}

/// Explicit-tax-line disagreement tolerance (spec §1.4, step 1): "if an
/// explicit tax line disagrees by >2¢, flag low_confidence... rather than
/// failing."
const TAX_LINE_DISAGREEMENT_TOLERANCE_CENTS: i64 = 2;

/// No-subtotal path (spec §1.4, step 2): items may fall short of the total
/// by up to this fraction (covers unparsed tax/tip) before it's a hard
/// mismatch.
const NO_SUBTOTAL_MAX_SHORTFALL_RATIO: f64 = 0.25;

/// No-subtotal path: shortfall beyond this fraction of the total flags
/// `overall_confidence = Low` (still succeeds).
const NO_SUBTOTAL_LOW_CONFIDENCE_GAP_RATIO: f64 = 0.03;

/// Reconciles parsed items against the recognized subtotal/total (spec
/// §1.4). All money in integer cents.
pub fn reconcile(input: &ReconcileInput) -> ReconcileOutcome {
    match input.subtotal_cents {
        Some(subtotal) => reconcile_with_subtotal(input, subtotal),
        None => reconcile_total_only(input),
    }
}

fn reconcile_with_subtotal(input: &ReconcileInput, subtotal: i64) -> ReconcileOutcome {
    let tolerance = subtotal_tolerance_cents(input.num_items);
    let diff = input.items_sum_cents - subtotal;

    if diff.abs() > tolerance {
        return ReconcileOutcome::Mismatch {
            computed_sum_cents: input.items_sum_cents,
            discrepancy_cents: diff,
        };
    }

    // Rule 3 (spec §1.4): sum(items) > total beyond tolerance is always a
    // hard mismatch, even when items landed within subtotal tolerance —
    // guards against a bogus/misparsed total that's smaller than the real
    // subtotal.
    if input.items_sum_cents > input.total_cents + tolerance {
        return ReconcileOutcome::Mismatch {
            computed_sum_cents: input.items_sum_cents,
            discrepancy_cents: input.items_sum_cents - input.total_cents,
        };
    }

    let derived_tax_tip = (input.total_cents - subtotal).max(0);
    let mut tax_tip_unconfirmed = false;
    if let Some(tax_line) = input.tax_line_cents
        && (derived_tax_tip - tax_line).abs() > TAX_LINE_DISAGREEMENT_TOLERANCE_CENTS {
            tax_tip_unconfirmed = true;
        }

    ReconcileOutcome::Success {
        tax_tip_amount_cents: derived_tax_tip,
        overall_confidence: if tax_tip_unconfirmed {
            OverallConfidence::Low
        } else {
            OverallConfidence::High
        },
        tax_tip_unconfirmed,
    }
}

fn reconcile_total_only(input: &ReconcileInput) -> ReconcileOutcome {
    if input.items_sum_cents > input.total_cents {
        return ReconcileOutcome::Mismatch {
            computed_sum_cents: input.items_sum_cents,
            discrepancy_cents: input.items_sum_cents - input.total_cents,
        };
    }

    let shortfall = input.total_cents - input.items_sum_cents;
    let max_shortfall = (input.total_cents as f64 * NO_SUBTOTAL_MAX_SHORTFALL_RATIO).round() as i64;
    if shortfall > max_shortfall {
        return ReconcileOutcome::Mismatch {
            computed_sum_cents: input.items_sum_cents,
            discrepancy_cents: shortfall,
        };
    }

    let gap_ratio = if input.total_cents > 0 {
        shortfall as f64 / input.total_cents as f64
    } else {
        0.0
    };
    let tax_tip_unconfirmed = gap_ratio > NO_SUBTOTAL_LOW_CONFIDENCE_GAP_RATIO;

    ReconcileOutcome::Success {
        tax_tip_amount_cents: shortfall,
        overall_confidence: if tax_tip_unconfirmed {
            OverallConfidence::Low
        } else {
            OverallConfidence::High
        },
        tax_tip_unconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        items_sum_cents: i64,
        num_items: usize,
        subtotal_cents: Option<i64>,
        tax_line_cents: Option<i64>,
        total_cents: i64,
    ) -> ReconcileInput {
        ReconcileInput {
            items_sum_cents,
            num_items,
            subtotal_cents,
            tax_line_cents,
            total_cents,
        }
    }

    #[test]
    fn subtotal_path_success_derives_tax_tip() {
        // Burger 12.00 + Fries 5.00 + Salad 9.00 = 26.00 subtotal, total 30.90.
        let outcome = reconcile(&input(2600, 3, Some(2600), None, 3090));
        assert_eq!(
            outcome,
            ReconcileOutcome::Success {
                tax_tip_amount_cents: 490,
                overall_confidence: OverallConfidence::High,
                tax_tip_unconfirmed: false,
            }
        );
    }

    #[test]
    fn subtotal_path_within_tolerance_still_succeeds() {
        // 3 items, tolerance = max(2,3) = 3 cents; sum is 2 cents off subtotal.
        let outcome = reconcile(&input(2598, 3, Some(2600), None, 3090));
        assert!(matches!(outcome, ReconcileOutcome::Success { .. }));
    }

    #[test]
    fn subtotal_path_beyond_tolerance_is_hard_mismatch() {
        let outcome = reconcile(&input(2550, 3, Some(2600), None, 3090));
        assert_eq!(
            outcome,
            ReconcileOutcome::Mismatch {
                computed_sum_cents: 2550,
                discrepancy_cents: -50,
            }
        );
    }

    #[test]
    fn subtotal_path_items_overcounted_is_hard_mismatch_even_if_bogus_subtotal_agrees() {
        // sum(items) way exceeds the total, even though it happens to match
        // a (misparsed) subtotal candidate -- rule 3 must still catch it.
        let outcome = reconcile(&input(9000, 2, Some(9000), None, 3000));
        assert!(matches!(outcome, ReconcileOutcome::Mismatch { .. }));
    }

    #[test]
    fn subtotal_path_flags_low_confidence_on_tax_line_disagreement() {
        // derived tax/tip = 30.90 - 26.00 = 4.90; explicit tax line says 3.00
        // -> disagreement > 2 cents -> low confidence, but still succeeds.
        let outcome = reconcile(&input(2600, 3, Some(2600), Some(300), 3090));
        assert_eq!(
            outcome,
            ReconcileOutcome::Success {
                tax_tip_amount_cents: 490,
                overall_confidence: OverallConfidence::Low,
                tax_tip_unconfirmed: true,
            }
        );
    }

    #[test]
    fn subtotal_path_tax_line_agreement_within_tolerance_stays_high_confidence() {
        let outcome = reconcile(&input(2600, 3, Some(2600), Some(491), 3090));
        assert_eq!(
            outcome,
            ReconcileOutcome::Success {
                tax_tip_amount_cents: 490,
                overall_confidence: OverallConfidence::High,
                tax_tip_unconfirmed: false,
            }
        );
    }

    #[test]
    fn total_only_path_success_within_25_percent_shortfall() {
        // No subtotal line; items sum to 26.00, total 30.90 -> shortfall
        // 4.90, which is ~15.9% of total, within the 25% allowance.
        let outcome = reconcile(&input(2600, 3, None, None, 3090));
        match outcome {
            ReconcileOutcome::Success {
                tax_tip_amount_cents,
                overall_confidence,
                tax_tip_unconfirmed,
            } => {
                assert_eq!(tax_tip_amount_cents, 490);
                assert_eq!(overall_confidence, OverallConfidence::Low);
                assert!(tax_tip_unconfirmed, "gap ratio > 3% should flag unconfirmed");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn total_only_path_small_gap_under_3_percent_stays_high_confidence() {
        // items 2990, total 3000 -> shortfall 10 cents = 0.33%.
        let outcome = reconcile(&input(2990, 3, None, None, 3000));
        assert_eq!(
            outcome,
            ReconcileOutcome::Success {
                tax_tip_amount_cents: 10,
                overall_confidence: OverallConfidence::High,
                tax_tip_unconfirmed: false,
            }
        );
    }

    #[test]
    fn total_only_path_shortfall_beyond_25_percent_is_hard_mismatch() {
        // items sum to 1000, total 3000 -> shortfall 2000 = 66.7% of total.
        let outcome = reconcile(&input(1000, 2, None, None, 3000));
        assert_eq!(
            outcome,
            ReconcileOutcome::Mismatch {
                computed_sum_cents: 1000,
                discrepancy_cents: 2000,
            }
        );
    }

    #[test]
    fn total_only_path_items_exceeding_total_is_hard_mismatch() {
        let outcome = reconcile(&input(3500, 2, None, None, 3000));
        assert_eq!(
            outcome,
            ReconcileOutcome::Mismatch {
                computed_sum_cents: 3500,
                discrepancy_cents: 500,
            }
        );
    }

    #[test]
    fn worked_example_from_spec_3_3_reconciles_cleanly() {
        // Burger 12.00 + Fries 5.00 + Salad 9.00 = 26.00, tax/tip 4.90,
        // total 30.90 (spec §3.3's worked example numbers).
        let outcome = reconcile(&input(2600, 3, Some(2600), None, 3090));
        assert_eq!(
            outcome,
            ReconcileOutcome::Success {
                tax_tip_amount_cents: 490,
                overall_confidence: OverallConfidence::High,
                tax_tip_unconfirmed: false,
            }
        );
    }
}
