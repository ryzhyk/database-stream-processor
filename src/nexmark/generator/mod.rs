//! Generators for the models usd in the Nexmark benchmark suite.
//!
//! Based on the equivalent [Nexmark Flink generator API](https://github.com/nexmark/nexmark/blob/v0.2.0/nexmark-flink/src/main/java/com/github/nexmark/flink/generator).

use self::config::Config;
use super::model::Event;
use anyhow::Result;
use bids::CHANNELS_NUMBER;
use cached::SizedCache;
use rand::Rng;

mod auctions;
mod bids;
pub mod config;
mod people;
mod price;
mod strings;

pub struct NexmarkGenerator<R: Rng> {
    /// Configuration to generate events against. Note that it may be replaced
    /// by a call to `splitAtEventId`.
    config: Config,
    rng: R,

    /// The memory cache used when creating bid channels.
    bid_channel_cache: SizedCache<u32, (String, String)>,

    /// Number of events generated by this generator.
    events_count_so_far: u64,

    /// Wallclock time at which we emit the first event (ms since epoch).
    /// Set when generator created.
    wallclock_base_time: u64,
}

impl<R: Rng> NexmarkGenerator<R> {
    pub fn has_next(&self) -> bool {
        self.events_count_so_far < self.config.max_events
    }

    pub fn next_event(&mut self) -> Result<Option<NextEvent>> {
        if !self.has_next() {
            return Ok(None);
        }

        // When, in event time, we should generate the event. Monotonic.
        let event_timestamp = self
            .config
            .timestamp_for_event(self.config.next_event_number(self.events_count_so_far));
        // When, in event time, the event should say it was generated. Depending on
        // outOfOrderGroupSize may have local jitter.
        let adjusted_event_timestamp = self.config.timestamp_for_event(
            self.config
                .next_adjusted_event_number(self.events_count_so_far),
        );
        // The minimum of this and all future adjusted event timestamps. Accounts for
        // jitter in the event timestamp.
        let watermark = self.config.timestamp_for_event(
            self.config
                .next_event_number_for_watermark(self.events_count_so_far),
        );
        // When, in wallclock time, we should emit the event.
        let wallclock_timestamp = self.wallclock_base_time + event_timestamp;

        let (auction_proportion, person_proportion, total_proportion) = (
            self.config.nexmark_config.auction_proportion as u64,
            self.config.nexmark_config.person_proportion as u64,
            self.config.nexmark_config.total_proportion() as u64,
        );

        let new_event_id = self.get_next_event_id();
        let rem = new_event_id % total_proportion;

        let event = if rem < person_proportion {
            Event::Person(self.next_person(new_event_id, adjusted_event_timestamp))
        } else if rem < person_proportion + auction_proportion {
            Event::Auction(self.next_auction(
                self.events_count_so_far,
                new_event_id,
                adjusted_event_timestamp,
            )?)
        } else {
            Event::Bid(self.next_bid(new_event_id, adjusted_event_timestamp))
        };

        self.events_count_so_far += 1;
        Ok(Some(NextEvent {
            wallclock_timestamp,
            event_timestamp,
            event,
            watermark,
        }))
    }

    pub fn new(config: Config, rng: R, wallclock_base_time: u64) -> NexmarkGenerator<R> {
        NexmarkGenerator {
            config,
            rng,
            bid_channel_cache: SizedCache::with_size(CHANNELS_NUMBER as usize),
            events_count_so_far: 0,
            wallclock_base_time,
        }
    }

    fn get_next_event_id(&self) -> u64 {
        self.config.first_event_id
            + self
                .config
                .next_adjusted_event_number(self.events_count_so_far)
    }
}

/// The next event and its various timestamps. Ordered by increasing wallclock
/// timestamp, then (arbitrary but stable) event hash order.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NextEvent {
    /// When, in wallclock time, should this event be emitted?
    pub wallclock_timestamp: u64,

    /// When, in event time, should this event be considered to have occured?
    pub event_timestamp: u64,

    /// The event itself.
    pub event: Event,

    /// The minimum of this and all future event timestamps.
    pub watermark: u64,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::nexmark::model::{Auction, Bid, Person};
    use rand::{rngs::mock::StepRng, thread_rng};

    pub fn make_test_generator() -> NexmarkGenerator<StepRng> {
        NexmarkGenerator::new(Config::default(), StepRng::new(0, 1), 0)
    }

    pub fn make_person() -> Person {
        Person {
            id: 1,
            name: String::from("AAA BBBB"),
            email_address: String::from("AAABBB@example.com"),
            credit_card: String::from("1111 2222 3333 4444"),
            city: String::from("Phoenix"),
            state: String::from("OR"),
            date_time: 0,
            extra: String::from(""),
        }
    }

    pub fn make_bid() -> Bid {
        Bid {
            auction: 1,
            bidder: 1,
            price: 99,
            channel: String::from("my-channel"),
            url: String::from("https://example.com"),
            date_time: 0,
            extra: String::new(),
        }
    }

    pub fn make_auction() -> Auction {
        Auction {
            id: 1,
            item_name: String::from("item-name"),
            description: String::from("description"),
            initial_bid: 5,
            reserve: 10,
            date_time: 0,
            expires: 2000,
            seller: 1,
            category: 1,
        }
    }

    pub fn make_next_event() -> NextEvent {
        NextEvent {
            wallclock_timestamp: 0,
            event_timestamp: 0,
            event: Event::Bid(make_bid()),
            watermark: 0,
        }
    }

    /// Generates a specified number of next events using the default test
    /// generator.
    pub fn generate_expected_next_events(
        wallclock_base_time: u64,
        num_events: usize,
    ) -> Vec<Option<NextEvent>> {
        let mut ng = make_test_generator();
        ng.wallclock_base_time = wallclock_base_time;

        (0..num_events).map(|_| ng.next_event().unwrap()).collect()
    }

    #[test]
    fn test_has_next() {
        let mut ng = make_test_generator();
        ng.config.max_events = 2;

        assert!(ng.has_next());
        ng.next_event().unwrap();
        assert!(ng.has_next());
        ng.next_event().unwrap();

        assert!(!ng.has_next());
    }

    #[test]
    fn test_next_event_id() {
        let mut ng = make_test_generator();

        assert_eq!(ng.get_next_event_id(), 0);
        ng.next_event().unwrap();
        assert_eq!(ng.get_next_event_id(), 1);
        ng.next_event().unwrap();
        assert_eq!(ng.get_next_event_id(), 2);
    }

    // Tests the first five expected events without relying on any test
    // helper for the data.
    #[test]
    fn test_next_event() {
        let mut ng = NexmarkGenerator::new(Config::default(), thread_rng(), 0);

        // The first event with the default config is the person
        let next_event = ng.next_event().unwrap();
        assert!(next_event.is_some());
        let next_event = next_event.unwrap();

        assert!(
            matches!(next_event.event, Event::Person(_)),
            "got: {:?}, want: Event::NewPerson(_)",
            next_event.event
        );
        assert_eq!(next_event.event_timestamp, 0);

        // The next 3 events with the default config are auctions
        for event_num in 1..=3 {
            let next_event = ng.next_event().unwrap();
            assert!(next_event.is_some());
            let next_event = next_event.unwrap();

            assert!(
                matches!(next_event.event, Event::Auction(_)),
                "got: {:?}, want: Event::NewAuction(_)",
                next_event.event
            );
            assert_eq!(next_event.event_timestamp, event_num / 10);
        }

        // And the rest of the events in the first epoch are bids.
        for event_num in 4..=49 {
            let next_event = ng.next_event().unwrap();
            assert!(next_event.is_some());
            let next_event = next_event.unwrap();

            assert!(
                matches!(next_event.event, Event::Bid(_)),
                "got: {:?}, want: Event::NewBid(_)",
                next_event.event
            );
            assert_eq!(next_event.event_timestamp, event_num / 10);
        }

        // The next epoch begins with another person etc.
        let next_event = ng.next_event().unwrap();
        assert!(next_event.is_some());
        let next_event = next_event.unwrap();

        assert_eq!(next_event.event_timestamp, 5);
        assert!(
            matches!(next_event.event, Event::Person(_)),
            "got: {:?}, want: Event::NewPerson(_)",
            next_event.event
        );
    }

    // Verifies that the `generate_expected_next_events()` test helper does
    // indeed output predictable results matching the order verified manually in
    // the above `test_next_events` (at least for the first 5 events).  Together
    // with the manual test above of next_events, this ensures the order is
    // correct for external call-sites.
    #[test]
    fn test_generate_expected_next_events() {
        let mut ng = make_test_generator();
        ng.wallclock_base_time = 1_000_000;

        let expected_events = generate_expected_next_events(1_000_000, 100);

        assert_eq!(
            (0..100)
                .map(|_| ng.next_event().unwrap())
                .collect::<Vec<Option<NextEvent>>>(),
            expected_events
        );
    }
}
