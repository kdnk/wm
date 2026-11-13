// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AxisConstraints {
    pub min: f64,
    pub fixed: Option<f64>,
    pub max: Option<f64>,
    pub weight: f64,
    pub can_grow: bool,
}

fn sanitize(v: f64) -> f64 { if v.is_finite() { v.max(0.0) } else { 0.0 } }

/// Solve 1D segment lengths for a container axis.
///
/// Rules:
/// - never negative
/// - `min` is a lower bound unless physically infeasible (then minima are scaled down proportionally)
/// - `fixed` values are enforced before distributing remainder
/// - `max` caps growth when positive
/// - growable nodes preserve their final weighted proportions whenever their bounds allow it
/// - zero-weight growable nodes share equally when no positive weights are present
/// - if nothing can grow, remainder becomes blank space
pub(crate) fn solve_axis_lengths(items: &[AxisConstraints], usable: f64) -> Vec<f64> {
    if items.is_empty() {
        return Vec::new();
    }
    let usable = sanitize(usable);
    let n = items.len();
    let mut mins: Vec<f64> = items.iter().map(|i| sanitize(i.min)).collect();
    let mut fixed: Vec<Option<f64>> =
        items.iter().map(|i| i.fixed.map(sanitize).filter(|v| v.is_finite())).collect();
    let maxs: Vec<Option<f64>> = items
        .iter()
        .map(|i| i.max.map(sanitize).filter(|v| *v > 0.0 && v.is_finite()))
        .collect();
    let weights: Vec<f64> = items.iter().map(|i| sanitize(i.weight)).collect();
    let can_grow: Vec<bool> = items.iter().map(|i| i.can_grow).collect();

    for (idx, f) in fixed.iter_mut().enumerate() {
        if let Some(v) = f {
            if *v < mins[idx] {
                *v = mins[idx];
            }
            if let Some(max) = maxs[idx] {
                if *v > max {
                    *v = max;
                }
            }
        }
    }

    let fixed_sum: f64 = fixed.iter().flatten().copied().sum();
    if fixed_sum > usable && fixed_sum > 0.0 {
        let scale = usable / fixed_sum;
        let mut lengths = vec![0.0; n];
        for idx in 0..n {
            if let Some(v) = fixed[idx] {
                lengths[idx] = v * scale;
            }
        }
        return lengths;
    }

    let min_indices: Vec<usize> = (0..n).filter(|&idx| fixed[idx].is_none()).collect();
    let min_sum: f64 = min_indices.iter().map(|&idx| mins[idx]).sum();
    let remaining_for_mins = (usable - fixed_sum).max(0.0);
    if min_sum > remaining_for_mins && min_sum > 0.0 {
        let scale = remaining_for_mins / min_sum;
        for &idx in &min_indices {
            mins[idx] *= scale;
        }
    }

    let mut lengths = vec![0.0; n];
    let mut remaining = usable;
    for idx in 0..n {
        if let Some(v) = fixed[idx] {
            let assigned = v.min(remaining);
            lengths[idx] = assigned;
            remaining = (remaining - assigned).max(0.0);
        }
    }

    // Non-growable segments stay at their minimum. Growable segments are solved from their
    // *final* weighted sizes, clamped to their bounds. Seeding every segment with its minimum and
    // distributing only the remainder would skew equal-weight splits whenever the minima differ.
    let mut growable: Vec<usize> = Vec::new();
    for idx in 0..n {
        if fixed[idx].is_none() {
            if can_grow[idx] {
                growable.push(idx);
            } else {
                let assigned = mins[idx].min(remaining);
                lengths[idx] = assigned;
                remaining = (remaining - assigned).max(0.0);
            }
        }
    }

    while !growable.is_empty() && remaining > f64::EPSILON {
        let total_weight: f64 = growable.iter().map(|&idx| weights[idx]).sum();
        let use_equal_weights = total_weight <= f64::EPSILON;
        let divisor = if use_equal_weights {
            growable.len() as f64
        } else {
            total_weight
        };

        let mut projected = Vec::with_capacity(growable.len());
        let mut lower_bounds = Vec::new();
        let mut upper_bounds = Vec::new();
        for &idx in &growable {
            let weight = if use_equal_weights { 1.0 } else { weights[idx] };
            let proposed = remaining * weight / divisor;
            let max = maxs[idx].map(|value| value.max(mins[idx]));
            let bounded = proposed.max(mins[idx]);
            projected.push((idx, max.map_or(bounded, |max| bounded.min(max))));
            if proposed < mins[idx] {
                lower_bounds.push((idx, mins[idx]));
            } else if let Some(max) = max.filter(|&max| proposed > max) {
                upper_bounds.push((idx, max));
            }
        }

        let projected_sum: f64 = projected.iter().map(|&(_, length)| length).sum();
        if (lower_bounds.is_empty() && upper_bounds.is_empty()) || projected_sum == remaining {
            for (idx, length) in projected {
                lengths[idx] = length;
            }
            remaining = 0.0;
            break;
        }

        let clamped =
            if upper_bounds.is_empty() || (!lower_bounds.is_empty() && projected_sum > remaining) {
                lower_bounds
            } else {
                upper_bounds
            };
        let clamped_sum: f64 = clamped.iter().map(|&(_, length)| length).sum();
        for &(idx, length) in &clamped {
            lengths[idx] = length;
        }
        remaining = (remaining - clamped_sum).max(0.0);
        growable.retain(|idx| !clamped.iter().any(|&(bound_idx, _)| bound_idx == *idx));
    }

    if remaining <= f64::EPSILON {
        let used: f64 = lengths.iter().sum();
        let dwm = usable - used;
        if dwm.abs() > f64::EPSILON {
            if let Some(idx) = (0..n).rfind(|&idx| lengths[idx] > 0.0) {
                lengths[idx] = (lengths[idx] + dwm).max(0.0);
            }
        }
    }

    lengths
}

#[cfg(test)]
mod tests {
    use super::{AxisConstraints, solve_axis_lengths};

    #[test]
    fn axis_constraints_mixed_bounds_redistribute_without_gaps_or_overflow() {
        for (mins, maxs, weights, usable, expected) in [
            (
                [80.0, 0.0, 0.0],
                [None, Some(10.0), None],
                [1.0, 9.0, 0.0],
                100.0,
                [90.0, 10.0, 0.0],
            ),
            (
                [600.0, 0.0, 200.0],
                [None, Some(600.0), None],
                [1.0, 9.0, 1.0],
                1000.0,
                [600.0, 200.0, 200.0],
            ),
            (
                [40.0, 0.0, 0.0],
                [None, Some(10.0), None],
                [1.0, 9.0, 1.0],
                100.0,
                [45.0, 10.0, 45.0],
            ),
            (
                [0.0, 20.0, 0.0],
                [Some(10.0), None, None],
                [9.0, 0.0, 0.0],
                100.0,
                [10.0, 45.0, 45.0],
            ),
        ] {
            let items: Vec<_> = (0..3)
                .map(|idx| AxisConstraints {
                    min: mins[idx],
                    max: maxs[idx],
                    weight: weights[idx],
                    can_grow: true,
                    ..Default::default()
                })
                .collect();
            let solved = solve_axis_lengths(&items, usable);
            for (actual, expected) in solved.iter().zip(expected) {
                assert!((actual - expected).abs() < 0.001, "{items:?}: {solved:?}");
            }
        }
    }

    #[test]
    fn axis_constraints_feasible_bounds_preserve_space_for_all_rows() {
        let options: Vec<_> = [0.0, 20.0, 60.0]
            .into_iter()
            .flat_map(|min| {
                [None, Some(30.0), Some(80.0)]
                    .into_iter()
                    .filter(move |max| max.is_none_or(|max| max >= min))
                    .flat_map(move |max| {
                        [0.0, 1.0, 9.0].into_iter().map(move |weight| AxisConstraints {
                            min,
                            max,
                            weight,
                            can_grow: true,
                            ..Default::default()
                        })
                    })
            })
            .collect();
        for &a in &options {
            for &b in &options {
                for &c in &options {
                    let items = [a, b, c];
                    if items.iter().map(|item| item.min).sum::<f64>() > 100.0 {
                        continue;
                    }
                    let solved = solve_axis_lengths(&items, 100.0);
                    let capacity = items.iter().map(|item| item.max.unwrap_or(100.0)).sum::<f64>();
                    assert!(
                        (solved.iter().sum::<f64>() - capacity.min(100.0)).abs() < 0.001,
                        "{items:?}: {solved:?}"
                    );
                    for (item, length) in items.iter().zip(solved) {
                        assert!(length + 0.001 >= item.min, "{items:?}: {length}");
                        assert!(
                            length <= item.max.unwrap_or(100.0) + 0.001,
                            "{items:?}: {length}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn scales_non_fixed_minima_after_reserving_fixed_segments() {
        let solved = solve_axis_lengths(
            &[
                AxisConstraints {
                    min: 0.0,
                    fixed: Some(600.0),
                    max: None,
                    weight: 1.0,
                    can_grow: false,
                },
                AxisConstraints {
                    min: 300.0,
                    fixed: None,
                    max: None,
                    weight: 1.0,
                    can_grow: true,
                },
                AxisConstraints {
                    min: 300.0,
                    fixed: None,
                    max: None,
                    weight: 1.0,
                    can_grow: true,
                },
            ],
            1000.0,
        );

        assert_eq!(solved.len(), 3);
        assert!((solved[0] - 600.0).abs() < 0.001);
        assert!((solved[1] - 200.0).abs() < 0.001);
        assert!((solved[2] - 200.0).abs() < 0.001);
    }

    #[test]
    fn scales_overcommitted_fixed_segments_symmetrically() {
        let solved = solve_axis_lengths(
            &[
                AxisConstraints {
                    min: 0.0,
                    fixed: Some(900.0),
                    max: None,
                    weight: 1.0,
                    can_grow: false,
                },
                AxisConstraints {
                    min: 0.0,
                    fixed: Some(900.0),
                    max: None,
                    weight: 1.0,
                    can_grow: false,
                },
            ],
            1400.0,
        );

        assert_eq!(solved.len(), 2);
        assert!((solved[0] - 700.0).abs() < 0.001);
        assert!((solved[1] - 700.0).abs() < 0.001);
    }

    #[test]
    fn max_caps_participate_in_growth_distribution() {
        let solved = solve_axis_lengths(
            &[
                AxisConstraints {
                    min: 0.0,
                    fixed: None,
                    max: Some(600.0),
                    weight: 1.0,
                    can_grow: true,
                },
                AxisConstraints {
                    min: 0.0,
                    fixed: None,
                    max: None,
                    weight: 1.0,
                    can_grow: true,
                },
            ],
            1600.0,
        );

        assert_eq!(solved.len(), 2);
        assert!((solved[0] - 600.0).abs() < 0.001);
        assert!((solved[1] - 1000.0).abs() < 0.001);
    }

    #[test]
    fn non_binding_minima_do_not_skew_equal_weight_segments() {
        let solved = solve_axis_lengths(
            &[
                AxisConstraints {
                    min: 0.0,
                    fixed: None,
                    max: None,
                    weight: 1.0,
                    can_grow: true,
                },
                AxisConstraints {
                    min: 400.0,
                    fixed: None,
                    max: None,
                    weight: 1.0,
                    can_grow: true,
                },
            ],
            1200.0,
        );

        assert_eq!(solved.len(), 2);
        assert!((solved[0] - 600.0).abs() < 0.001);
        assert!((solved[1] - 600.0).abs() < 0.001);
    }
}
