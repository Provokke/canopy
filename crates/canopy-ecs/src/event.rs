//! Double-buffered events.
//!
//! Events written during frame N are readable during frames N and N+1; each
//! [`Events::update`] call discards the older buffer. Readers keep a cursor
//! ([`EventCursor`]) so each reader sees each event at most once.

/// Storage for events of type `E`, usually kept as a resource.
#[derive(Debug)]
pub struct Events<E> {
    older: Vec<(usize, E)>,
    newer: Vec<(usize, E)>,
    count: usize,
}

impl<E> Default for Events<E> {
    fn default() -> Self {
        Self {
            older: Vec::new(),
            newer: Vec::new(),
            count: 0,
        }
    }
}

/// A reader position into an [`Events`] stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventCursor(usize);

impl<E> Events<E> {
    /// Sends an event.
    pub fn send(&mut self, e: E) {
        self.newer.push((self.count, e));
        self.count += 1;
    }

    /// Swaps buffers, dropping events older than two updates.
    pub fn update(&mut self) {
        std::mem::swap(&mut self.older, &mut self.newer);
        self.newer.clear();
    }

    /// Reads all events not yet seen through `cursor` and advances it.
    pub fn read<'a>(&'a self, cursor: &mut EventCursor) -> impl Iterator<Item = &'a E> + 'a {
        let from = cursor.0;
        cursor.0 = self.count;
        self.older
            .iter()
            .chain(self.newer.iter())
            .filter(move |(id, _)| *id >= from)
            .map(|(_, e)| e)
    }

    /// Number of events currently buffered.
    pub fn len(&self) -> usize {
        self.older.len() + self.newer.len()
    }

    /// Whether no events are buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// System that advances the `Events<E>` buffers by one update. Add it once per frame
/// (e.g. to a `First` schedule) for every event type in use.
pub fn update_events<E: Send + Sync + 'static>(mut events: crate::system::ResMut<Events<E>>) {
    events.update();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_live_two_updates_and_are_read_once() {
        let mut ev = Events::default();
        let mut c = EventCursor::default();
        ev.send(1);
        ev.send(2);
        assert_eq!(ev.read(&mut c).copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(ev.read(&mut c).count(), 0);
        ev.update();
        ev.send(3);
        let mut late = EventCursor::default();
        assert_eq!(
            ev.read(&mut late).copied().collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(ev.read(&mut c).copied().collect::<Vec<_>>(), vec![3]);
        ev.update();
        ev.update();
        assert!(ev.is_empty());
    }
}
