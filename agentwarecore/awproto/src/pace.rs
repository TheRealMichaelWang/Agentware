//! How long the agent's cursor is allowed to take to show one action.
//!
//! The agent's actions are performed and answered the instant they arrive;
//! the cursor flying to a control is a picture of something that has already
//! happened. So nothing in the system waits on these numbers, and they buy
//! exactly one thing: whether a human watching can follow along.
//!
//! That makes the right value a preference rather than a constant. Someone
//! learning what an agent does to their machine wants to see every step;
//! someone who has watched it a hundred times wants it out of the way. Both
//! are right, so both ends of the range are set on the Settings app's Agent
//! page, under Computer Use.
//!
//! ## Why a range rather than a duration
//!
//! One number cannot answer this, because the useful pace depends on how far
//! behind the cursor already is. A single action with nothing behind it
//! should be unhurried: it is the only thing to look at. Twenty actions
//! queued up mean the machine has raced ahead and the cursor is showing
//! history, where the useful information is "these twenty things happened, in
//! this order" and drawing each at full ceremony turns that into a wait for
//! something already finished.
//!
//! So the pace is a function of the queue, running between the two ends the
//! human set: [`Pace::of`].

use std::time::Duration;

/// The shortest and longest a single action's animation may be set to.
///
/// Bounds on the *setting*, not on the pace itself: a file edited by hand
/// cannot ask for a cursor that teleports or one that takes a minute to cross
/// the screen.
pub const FLOOR: Duration = Duration::from_millis(10);
pub const CEILING: Duration = Duration::from_millis(3_000);

/// How much of the remaining distance to the floor each queued action closes.
///
/// The shrink is exponential in the queue depth, so the first few waiting
/// actions matter a great deal and the twentieth barely moves anything: by
/// then the cursor is already going as fast as the human said it may.
const DECAY: f32 = 0.55;

/// The two ends of the range, as the human set them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pace {
    /// What one action costs when the cursor is far behind. Approached, never
    /// quite reached, as the queue grows.
    pub min: Duration,
    /// What one action costs when it is the only one there is.
    pub max: Duration,
}

impl Default for Pace {
    fn default() -> Pace {
        Pace {
            min: Duration::from_millis(40),
            max: Duration::from_millis(600),
        }
    }
}

impl Pace {
    /// The two ends, put in order and inside the bounds.
    ///
    /// A minimum above the maximum is a file somebody typed into rather than
    /// a preference anybody holds, so the two are swapped rather than
    /// refused: every other malformed setting falls back to something that
    /// works, and an agentdesk that will not draw a cursor because two
    /// numbers are the wrong way round would be a worse answer than a cursor
    /// that moves at some speed.
    pub fn new(min: Duration, max: Duration) -> Pace {
        let (min, max) = if min > max { (max, min) } else { (min, max) };
        Pace {
            min: min.clamp(FLOOR, CEILING),
            max: max.clamp(FLOOR, CEILING),
        }
    }

    /// How long one action may take to draw, with `waiting` more already
    /// queued behind it.
    ///
    /// `waiting` of zero is [`Pace::max`] exactly: one action, nothing after
    /// it, all the time the human asked for. Each one waiting closes
    /// [`DECAY`] of what is left down to [`Pace::min`], which is approached
    /// and never reached, so a cursor with a hundred actions behind it is
    /// still drawing them rather than skipping them.
    pub fn of(&self, waiting: usize) -> Duration {
        let span = self.max.saturating_sub(self.min);
        if span.is_zero() {
            return self.min;
        }
        // Saturating rather than wrapping: a queue longer than an i32 is not
        // a real queue, and `powi` on a huge exponent is zero anyway, which
        // is the answer this would want.
        let shrink = DECAY.powi(waiting.min(i32::MAX as usize) as i32);
        self.min + span.mul_f32(shrink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn one_action_alone_costs_the_maximum() {
        // The whole point of the maximum: nothing else is happening, so the
        // human gets the pace they asked for.
        let pace = Pace::new(ms(40), ms(600));
        assert_eq!(pace.of(0), ms(600));
    }

    #[test]
    fn a_growing_queue_shrinks_towards_the_minimum() {
        let pace = Pace::new(ms(40), ms(600));
        let steps: Vec<Duration> = (0..12).map(|waiting| pace.of(waiting)).collect();

        // Every step is quicker than the one before it, and none of them
        // reaches the floor: the cursor keeps drawing rather than skipping.
        for pair in steps.windows(2) {
            assert!(pair[1] < pair[0], "{pair:?} did not shrink");
            assert!(pair[1] > pace.min, "{:?} reached the minimum", pair[1]);
        }

        // Exponential, not linear: the first waiting action takes most of the
        // step, and by a dozen there is almost nothing left to give.
        let first = steps[0] - steps[1];
        let last = steps[10] - steps[11];
        assert!(first > last * 50, "expected a sharp curve, got {first:?} then {last:?}");

        // And far enough along it is indistinguishable from the minimum.
        assert!(pace.of(64) < pace.min + ms(1));
    }

    #[test]
    fn the_range_is_put_in_order_and_bounded() {
        // Backwards is a typo, not a refusal.
        let swapped = Pace::new(ms(600), ms(40));
        assert_eq!(swapped, Pace::new(ms(40), ms(600)));

        // A hand edit cannot ask for a cursor that teleports, or one that
        // takes a minute to cross the screen.
        let silly = Pace::new(ms(0), ms(600_000));
        assert_eq!(silly.min, FLOOR);
        assert_eq!(silly.max, CEILING);
    }

    #[test]
    fn a_range_of_nothing_is_one_speed() {
        // Both ends the same is a legitimate choice: always this fast.
        let fixed = Pace::new(ms(100), ms(100));
        assert_eq!(fixed.of(0), ms(100));
        assert_eq!(fixed.of(50), ms(100));
    }
}
