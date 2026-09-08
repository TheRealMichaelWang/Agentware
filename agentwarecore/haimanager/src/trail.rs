//! The agent's cursor, and the queue of places it still has to be seen.
//!
//! **Nothing here performs anything.** An intent is validated, performed and
//! answered by [`crate::screen`] the moment it arrives; what this module owns
//! is the picture of work that has already happened. So no agent waits on it,
//! no application waits on it, and the model is free to think about its next
//! step while the cursor is still travelling towards the last one.
//!
//! It exists for one reason, which decides every rule below: a human has to
//! be able to watch an agent work on their machine and follow what it did.
//!
//! ## Three rules, and what each is for
//!
//! **In order, and none dropped.** Every action queues a stop and every stop
//! is drawn, in the order the actions happened, however far behind the
//! machine that leaves the cursor. Someone watching has to be able to trust
//! that what they saw is what happened; a queue that skips is one that hides
//! steps.
//!
//! **Only for the workspace on screen.** Switching to another agentdesk
//! [`Trail::follow`]s it, which throws the whole queue away. An agent working
//! where nobody is looking draws nothing at all, which costs the compositor
//! nothing and lets that agent run at the speed of the machine rather than
//! the speed of an animation nobody is watching. Switching back begins a
//! fresh queue from whatever happens next; the backlog is not replayed,
//! because a person returning to a workspace wants to see what it is doing
//! now, not a recording of what they missed.
//!
//! **Faster the further behind it is.** One action with nothing behind it
//! gets the full pace the human asked for. Twenty queued mean the machine has
//! raced ahead and the interesting information is the sequence rather than
//! each step of it, so the pace shrinks towards the minimum. The curve is
//! [`awproto::pace::Pace::of`]; both ends of it are settings.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use awproto::pace::Pace;

/// Somewhere the cursor should be seen, because an action happened there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stop {
    desk: u32,
    at: (i32, i32),
}

/// The cursor on its way to one stop.
#[derive(Clone, Copy, Debug)]
struct Flight {
    desk: u32,
    from: (i32, i32),
    to: (i32, i32),
    started: Instant,
    duration: Duration,
}

impl Flight {
    /// Where the cursor is now, and whether it has arrived.
    ///
    /// Eased, because a pointer that moves at a constant speed and stops dead
    /// does not read as a pointer.
    fn at(&self, now: Instant) -> ((i32, i32), bool) {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.duration {
            return (self.to, true);
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = 1.0 - (1.0 - t).powi(3);
        let step = |from: i32, to: i32| from + ((to - from) as f32 * eased) as i32;
        ((step(self.from.0, self.to.0), step(self.from.1, self.to.1)), false)
    }
}

/// The queue of stops, and the flight currently drawing one of them.
pub struct Trail {
    flight: Option<Flight>,
    pending: VecDeque<Stop>,
    /// The workspace on screen. Actions anywhere else are not drawn.
    showing: Option<u32>,
    /// Where the cursor was left, so the next flight starts from there rather
    /// than jumping.
    cursor: Option<(u32, (i32, i32))>,
    pace: Pace,
    /// Where a flight starts when the cursor has not been anywhere yet.
    home: (i32, i32),
    /// How wide the workspace is, so a flight's duration is a fraction of
    /// crossing it rather than a count of pixels.
    span: i32,
}

impl Trail {
    pub fn new() -> Trail {
        Trail {
            flight: None,
            pending: VecDeque::new(),
            showing: None,
            cursor: None,
            pace: Pace::default(),
            home: (0, 0),
            span: 1,
        }
    }

    /// The range the human set, re-read whenever the settings file changes.
    pub fn set_pace(&mut self, pace: Pace) {
        self.pace = pace;
    }

    /// Where a cursor with no history starts from, and how wide the room it
    /// crosses is. Both come from the workspace's geometry, which changes
    /// with the display.
    pub fn set_stage(&mut self, home: (i32, i32), span: i32) {
        self.home = home;
        self.span = span.max(1);
    }

    /// Watch this workspace, and forget everything queued for the last one.
    ///
    /// Called whenever the workspace on screen changes, including to nothing.
    /// The backlog goes rather than being replayed: someone arriving at a
    /// workspace wants to see what it is doing, not what they missed.
    pub fn follow(&mut self, desk: Option<u32>) {
        if self.showing == desk {
            return;
        }
        self.showing = desk;
        self.flight = None;
        self.pending.clear();
        self.cursor = None;
    }

    /// An action happened here. Queue the picture of it.
    ///
    /// Ignored for any workspace but the one on screen, which is what makes
    /// an agent nobody is watching cost nothing to draw.
    ///
    /// Queues only. Flights begin in [`Trail::tick`], so every time this
    /// module reads comes from its caller and none of it from the clock,
    /// which is what makes the pacing testable rather than something that has
    /// to be watched to be believed.
    pub fn push(&mut self, desk: u32, at: (i32, i32)) {
        if self.showing != Some(desk) {
            return;
        }
        self.pending.push_back(Stop { desk, at });
        self.hurry();
    }

    /// Shorten the flight in the air to match a queue that just grew.
    ///
    /// Without this the first action of a burst keeps the pace it was given
    /// when it was the only one there was, so a cursor handed twelve actions
    /// at once spends the full unhurried time on the first of them while
    /// eleven wait. The pace is a function of how far behind the cursor is,
    /// and it just fell further behind.
    ///
    /// Only ever shorter. Lengthening a flight already being drawn would
    /// read as the cursor stalling halfway.
    fn hurry(&mut self) {
        let Some(flight) = self.flight else { return };
        let wanted = self.duration(flight.from, flight.to);
        if let Some(flight) = &mut self.flight
            && wanted < flight.duration
        {
            flight.duration = wanted;
        }
    }

    /// Whether anything is left to draw. The compositor asks so it knows to
    /// keep producing frames.
    pub fn busy(&self) -> bool {
        self.flight.is_some() || !self.pending.is_empty()
    }

    /// Advance to `now`, and answer with where the cursor should be drawn.
    ///
    /// `None` means there is nothing to draw: no agent has acted in this
    /// workspace since it came on screen.
    pub fn tick(&mut self, now: Instant) -> Option<(u32, (i32, i32))> {
        self.take_off(now);
        let flight = self.flight?;
        let (at, arrived) = flight.at(now);
        self.cursor = Some((flight.desk, at));
        if arrived {
            self.flight = None;
            // Straight on to the next, if the machine got further ahead while
            // this one was being drawn.
            self.take_off(now);
        }
        self.cursor
    }

    /// Begin drawing the next stop, if the cursor is free and one is waiting.
    fn take_off(&mut self, now: Instant) {
        if self.flight.is_some() {
            return;
        }
        let Some(stop) = self.pending.pop_front() else { return };
        let from = match self.cursor {
            Some((desk, at)) if desk == stop.desk => at,
            _ => self.home,
        };
        self.flight = Some(Flight {
            desk: stop.desk,
            from,
            to: stop.at,
            started: now,
            // The pace is read after the pop, so a stop with nothing behind
            // it is unhurried and one at the head of a queue is not.
            duration: self.duration(from, stop.at),
        });
    }

    /// How long one flight takes: the distance it covers, against the pace
    /// the queue has earned.
    fn duration(&self, from: (i32, i32), to: (i32, i32)) -> Duration {
        let ceiling = self.pace.of(self.pending.len());
        let distance = (((to.0 - from.0) as f32).powi(2)
            + ((to.1 - from.1) as f32).powi(2))
        .sqrt();
        // Proportional to the width of the room it is crossing, so the pacing
        // reads the same at every resolution and interface scale without a
        // constant saying so. Crossing the whole workspace costs the ceiling;
        // going nowhere still costs the minimum, because a flight is also how
        // a human sees that an action happened at all.
        let share = (distance / self.span as f32).clamp(0.0, 1.0);
        ceiling.mul_f32(share).max(self.pace.min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching() -> Trail {
        let mut trail = Trail::new();
        trail.set_stage((0, 0), 1000);
        trail.set_pace(Pace::new(Duration::from_millis(40), Duration::from_millis(600)));
        trail.follow(Some(1));
        trail
    }

    #[test]
    fn nothing_is_drawn_for_a_workspace_nobody_is_watching() {
        // The rule that lets an agent in a background agentdesk run at the
        // speed of the machine: its actions cost no animation at all.
        let mut trail = watching();
        trail.push(2, (100, 100));
        trail.push(2, (200, 200));
        assert!(!trail.busy(), "a workspace off screen queued something");

        // And the one on screen still does.
        trail.push(1, (100, 100));
        assert!(trail.busy());
    }

    #[test]
    fn switching_away_throws_the_queue_away() {
        let mut trail = watching();
        for n in 1..=5 {
            trail.push(1, (n * 100, 0));
        }
        assert!(trail.busy());

        // Someone looked at another workspace. The backlog is not theirs.
        trail.follow(Some(2));
        assert!(!trail.busy());

        // Coming back begins a fresh queue rather than replaying what was
        // missed: a person wants to see what it is doing now.
        trail.follow(Some(1));
        assert!(!trail.busy());
        trail.push(1, (10, 10));
        assert!(trail.busy());
    }

    #[test]
    fn following_the_same_workspace_again_disturbs_nothing() {
        // `follow` is called whenever the screen is examined, not only when
        // it changes, so repeating it must not throw work away.
        let mut trail = watching();
        trail.push(1, (500, 0));
        trail.follow(Some(1));
        assert!(trail.busy());
    }

    #[test]
    fn every_stop_is_drawn_in_order_and_none_dropped() {
        // The queue never skips. Fifty actions produce fifty flights, in the
        // order they happened, however far behind that leaves the cursor.
        let mut trail = watching();
        let places: Vec<(i32, i32)> = (1..=50).map(|n| (n * 17 % 1000, n * 23 % 700)).collect();
        for at in &places {
            trail.push(1, *at);
        }

        let mut now = Instant::now();
        let mut reached = Vec::new();
        for _ in 0..100_000 {
            if !trail.busy() {
                break;
            }
            let before = trail.flight.map(|f| f.to);
            now += Duration::from_millis(2);
            trail.tick(now);
            let after = trail.flight.map(|f| f.to);
            // A flight ending is a stop reached.
            if before != after && let Some(to) = before {
                reached.push(to);
            }
        }
        assert_eq!(reached, places, "stops were dropped or reordered");
    }

    #[test]
    fn a_deep_queue_is_drawn_faster_than_a_lone_action() {
        // The whole point of the range. Same distance, different backlog.
        let start = Instant::now();

        let mut alone = watching();
        alone.push(1, (1000, 0));
        alone.tick(start);
        let unhurried = alone.flight.expect("a flight").duration;

        let mut behind = watching();
        for _ in 0..12 {
            behind.push(1, (1000, 0));
        }
        behind.tick(start);
        let hurried = behind.flight.expect("a flight").duration;

        assert!(hurried < unhurried, "{hurried:?} was not quicker than {unhurried:?}");
        // Approached, never reached: it is still drawing, not skipping.
        assert!(hurried >= behind.pace.min);
    }

    #[test]
    fn a_flight_costs_what_it_covers() {
        let start = Instant::now();
        let mut trail = watching();

        // Crossing the whole workspace alone is the maximum.
        trail.push(1, (1000, 0));
        trail.tick(start);
        assert_eq!(trail.flight.expect("a flight").duration, trail.pace.max);

        // Going nowhere is the minimum rather than nothing: a flight is also
        // how a human sees that an action happened at all.
        let mut trail = watching();
        trail.push(1, (0, 0));
        trail.tick(start);
        assert_eq!(trail.flight.expect("a flight").duration, trail.pace.min);
    }

    #[test]
    fn the_cursor_arrives_where_it_was_sent() {
        let start = Instant::now();
        let mut trail = watching();
        trail.push(1, (300, 400));
        trail.tick(start);
        let end = start + trail.flight.expect("a flight").duration;
        assert_eq!(trail.tick(end), Some((1, (300, 400))));
        assert!(!trail.busy());
    }
}
