//! Sharing a power budget among EV chargers.

use crate::Kw;

/// Max-min fair ("water-filling") allocation: small demands are met in full and the rest
/// split what's left equally. Returns allocations in the *same order* as `demands`.
pub fn water_fill(budget: Kw, demands: &[Kw]) -> Vec<Kw> {
    let mut order: Vec<usize> = (0..demands.len()).collect();
    order.sort_by(|&a, &b| demands[a].total_cmp(&demands[b])); // f64 isn't Ord (NaN)
    let mut out = vec![0.0; demands.len()];
    let mut remaining = budget.max(0.0);
    for (i, &idx) in order.iter().enumerate() {
        let share = remaining / (order.len() - i) as f64;
        let give = demands[idx].max(0.0).min(share);
        out[idx] = give;
        remaining -= give;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_everyone_when_budget_suffices() {
        assert_eq!(
            water_fill(100.0, &[10.0, 20.0, 30.0]),
            vec![10.0, 20.0, 30.0]
        );
    }

    #[test]
    fn is_max_min_fair_and_keeps_order() {
        assert_eq!(water_fill(30.0, &[22.0, 5.0, 22.0]), vec![12.5, 5.0, 12.5]);
    }

    #[test]
    fn never_exceeds_budget() {
        let out = water_fill(7.0, &[3.0, 3.0, 3.0, 3.0]);
        assert!(out.iter().sum::<f64>() <= 7.0 + 1e-9);
    }
}
