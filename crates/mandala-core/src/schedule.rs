//! Budgeting for inline playback.
//!
//! Playing every visible tile exhausts decoder memory and GPU decode slots, so
//! concurrent playback is capped and the slots go to the highest priority
//! tiles.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackCandidate {
    pub index: usize,
    /// Center of the tile on the content's y axis.
    pub center_y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduleParams {
    /// How many tiles may play at once.
    pub budget: usize,
    /// Bonus, in pixels of apparent distance, given to tiles that are already
    /// playing. Without it, scrolling makes tiles start and stop constantly.
    pub hysteresis: f32,
}

impl Default for ScheduleParams {
    fn default() -> Self {
        Self { budget: 12, hysteresis: 150.0 }
    }
}

/// Returns the indices that should be playing, most important first.
///
/// A hovered tile always wins; everything else is ranked by how close it is to
/// the center of the viewport.
pub fn plan_playback(
    candidates: &[PlaybackCandidate],
    viewport_center_y: f32,
    hovered: Option<usize>,
    currently_playing: &[usize],
    params: ScheduleParams,
) -> Vec<usize> {
    if params.budget == 0 {
        return Vec::new();
    }

    // Cost is distance from the viewport center, so lower sorts first.
    let mut ranked: Vec<(f32, usize)> = candidates
        .iter()
        .map(|c| {
            let cost = if hovered == Some(c.index) {
                f32::NEG_INFINITY
            } else if currently_playing.contains(&c.index) {
                (c.center_y - viewport_center_y).abs() - params.hysteresis
            } else {
                (c.center_y - viewport_center_y).abs()
            };
            (cost, c.index)
        })
        .collect();

    ranked.sort_by(|a, b| {
        a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal).then_with(|| a.1.cmp(&b.1))
    });
    ranked.truncate(params.budget);
    ranked.into_iter().map(|(_, index)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<PlaybackCandidate> {
        (0..5).map(|i| PlaybackCandidate { index: i, center_y: i as f32 * 100.0 }).collect()
    }

    const NO_HYSTERESIS: ScheduleParams = ScheduleParams { budget: 3, hysteresis: 0.0 };

    #[test]
    fn returns_everything_when_budget_is_generous() {
        let got = plan_playback(
            &candidates(),
            200.0,
            None,
            &[],
            ScheduleParams { budget: 99, ..NO_HYSTERESIS },
        );
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn prefers_tiles_nearest_the_viewport_center() {
        // Distances from center 200 are 200,100,0,100,200; ties break on index.
        let got = plan_playback(&candidates(), 200.0, None, &[], NO_HYSTERESIS);
        assert_eq!(got, vec![2, 1, 3]);
    }

    #[test]
    fn hovered_tile_always_wins_even_at_the_edge() {
        let got = plan_playback(
            &candidates(),
            200.0,
            Some(4),
            &[],
            ScheduleParams { budget: 1, ..NO_HYSTERESIS },
        );
        assert_eq!(got, vec![4]);

        let got = plan_playback(&candidates(), 200.0, Some(4), &[], NO_HYSTERESIS);
        assert_eq!(got, vec![4, 2, 1]);
    }

    #[test]
    fn hysteresis_keeps_an_already_playing_tile_alive() {
        // Index 0 sits 200 away and would normally lose, but the 150 bonus
        // brings it to an effective 50 and past the two tiles at 100.
        let params = ScheduleParams { budget: 2, hysteresis: 150.0 };
        let got = plan_playback(&candidates(), 200.0, None, &[0], params);
        assert_eq!(got, vec![2, 0]);
    }

    #[test]
    fn hysteresis_does_not_rescue_a_hopelessly_distant_tile() {
        let params = ScheduleParams { budget: 2, hysteresis: 50.0 };
        let got = plan_playback(&candidates(), 200.0, None, &[0], params);
        assert_eq!(got, vec![2, 1]);
    }

    #[test]
    fn zero_budget_plays_nothing_even_when_hovered() {
        let params = ScheduleParams { budget: 0, hysteresis: 0.0 };
        assert!(plan_playback(&candidates(), 200.0, Some(2), &[], params).is_empty());
    }

    #[test]
    fn ignores_hover_and_playing_indices_that_are_not_candidates() {
        // Indices that just scrolled out of the candidate set must not panic.
        let got = plan_playback(&candidates(), 200.0, Some(999), &[998], NO_HYSTERESIS);
        assert_eq!(got, vec![2, 1, 3]);
    }

    #[test]
    fn handles_an_empty_candidate_list() {
        assert!(plan_playback(&[], 0.0, Some(0), &[0], NO_HYSTERESIS).is_empty());
    }
}
