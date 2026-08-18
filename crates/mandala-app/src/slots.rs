//! Matching playback slots to the tiles that should be playing.
//!
//! Slots are the scarce resource: each one owns a decoder and a worker thread.
//! Reassigning a slot means tearing down a decoder and opening another, so the
//! whole point here is to leave a slot alone when it already holds a video that
//! should keep playing -- scrolling by one row must not restart the tiles that
//! stayed on screen.

/// What to do with the slot pool this frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SlotPlan {
    /// Slots to stop, freeing their decoder.
    pub stop: Vec<usize>,
    /// Slot and the tile it should begin playing.
    pub start: Vec<(usize, usize)>,
}

impl SlotPlan {
    pub fn is_empty(&self) -> bool {
        self.stop.is_empty() && self.start.is_empty()
    }
}

/// Works out the smallest set of slot changes that gets from `current` to
/// `wanted`.
///
/// `current[slot]` is the tile that slot is playing, if any. `wanted` is the
/// scheduler's list, most important first, so when there are fewer slots than
/// wanted tiles the tail of the list is what gets dropped.
pub fn plan_slots(current: &[Option<usize>], wanted: &[usize]) -> SlotPlan {
    let wanted = &wanted[..wanted.len().min(current.len())];
    let mut plan = SlotPlan::default();

    // Slots already playing something wanted stay untouched.
    let mut free: Vec<usize> = Vec::new();
    for (slot, holding) in current.iter().enumerate() {
        match holding {
            Some(tile) if wanted.contains(tile) => {}
            Some(_) => {
                plan.stop.push(slot);
                free.push(slot);
            }
            None => free.push(slot),
        }
    }

    // Whatever is left over goes to the wanted tiles that nobody holds, in
    // priority order, so if slots run out it is the least important tile that
    // misses out.
    let mut free = free.into_iter();
    for &tile in wanted {
        if current.contains(&Some(tile)) {
            continue;
        }
        let Some(slot) = free.next() else { break };
        plan.start.push((slot, tile));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_empty_slots_in_priority_order() {
        let plan = plan_slots(&[None, None, None], &[7, 8]);
        assert_eq!(plan.start, vec![(0, 7), (1, 8)]);
        assert!(plan.stop.is_empty());
    }

    #[test]
    fn leaves_a_slot_alone_when_it_already_holds_a_wanted_tile() {
        // Scrolling one row: tile 5 stays visible and must not restart.
        let plan = plan_slots(&[Some(5), Some(9)], &[5, 6]);
        assert_eq!(plan.stop, vec![1], "only the slot holding a dropped tile stops");
        assert_eq!(plan.start, vec![(1, 6)]);
    }

    #[test]
    fn nothing_changes_when_the_wanted_set_is_unchanged() {
        let plan = plan_slots(&[Some(1), Some(2)], &[2, 1]);
        assert!(plan.is_empty(), "got {plan:?}");
    }

    #[test]
    fn stops_every_slot_when_nothing_should_play() {
        let plan = plan_slots(&[Some(1), None, Some(3)], &[]);
        assert_eq!(plan.stop, vec![0, 2]);
        assert!(plan.start.is_empty());
    }

    #[test]
    fn drops_the_lowest_priority_tiles_when_slots_run_out() {
        // Two slots, three wanted tiles: the third simply does not play.
        let plan = plan_slots(&[None, None], &[1, 2, 3]);
        assert_eq!(plan.start, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn reuses_a_slot_freed_in_the_same_pass() {
        // Slot 0 gives up tile 9 and immediately takes tile 4.
        let plan = plan_slots(&[Some(9)], &[4]);
        assert_eq!(plan.stop, vec![0]);
        assert_eq!(plan.start, vec![(0, 4)]);
    }

    #[test]
    fn handles_having_no_slots_at_all() {
        assert!(plan_slots(&[], &[1, 2]).is_empty());
    }
}
