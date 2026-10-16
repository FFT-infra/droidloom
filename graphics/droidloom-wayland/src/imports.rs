//! Bounded compositor imports. Only legacy registration pauses request polling.
use droidloom_surface_bridge::layers::MAX_IMPORTS;

pub struct Pending<P> {
    pub buffer: u64,
    pub attempt: Option<u64>,
    pub params: P,
}

pub struct Imports<P> {
    pending: Vec<Pending<P>>,
}

impl<P> Default for Imports<P> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}

impl<P: PartialEq> Imports<P> {
    pub fn full(&self) -> bool {
        self.pending.len() >= MAX_IMPORTS
    }

    pub fn blocked(&self) -> bool {
        self.pending.iter().any(|p| p.attempt.is_none())
    }

    pub fn contains(&self, buffer: u64) -> bool {
        self.pending.iter().any(|p| p.buffer == buffer)
    }

    pub fn insert(&mut self, pending: Pending<P>) -> bool {
        if self.full() || self.blocked() || self.contains(pending.buffer) {
            return false;
        }
        self.pending.push(pending);
        true
    }

    pub fn finish(&mut self, params: &P) -> Option<Pending<P>> {
        let index = self.pending.iter().position(|p| &p.params == params)?;
        Some(self.pending.swap_remove(index))
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Pending<P>> + '_ {
        self.pending.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_imports_complete_out_of_order_with_their_original_identities() {
        let mut imports = Imports::default();
        for id in 1..=MAX_IMPORTS as u64 {
            assert!(imports.insert(Pending {
                buffer: id,
                attempt: Some(100 + id),
                params: id
            }));
        }
        assert!(imports.full());
        assert!(!imports.blocked());
        assert!(!imports.insert(Pending {
            buffer: 99,
            attempt: Some(199),
            params: 99
        }));
        let complete = imports.finish(&7).unwrap();
        assert_eq!((complete.buffer, complete.attempt), (7, Some(107)));
        assert!(imports.finish(&7).is_none());
        assert!(!imports.contains(7));
        assert!(!imports.full());
        assert!(imports.insert(Pending {
            buffer: 99,
            attempt: Some(199),
            params: 99
        }));
        assert!(!imports.insert(Pending {
            buffer: 99,
            attempt: Some(299),
            params: 100
        }));
        assert_eq!(imports.finish(&1).unwrap().attempt, Some(101));
        assert_eq!(imports.finish(&99).unwrap().attempt, Some(199));
    }

    #[test]
    fn legacy_import_resumes_requests_only_after_its_own_callback() {
        let mut imports = Imports::default();
        assert!(imports.insert(Pending {
            buffer: 1,
            attempt: Some(1),
            params: 10
        }));
        assert!(imports.insert(Pending {
            buffer: 2,
            attempt: None,
            params: 20
        }));
        assert!(imports.blocked());
        assert!(!imports.insert(Pending {
            buffer: 3,
            attempt: Some(3),
            params: 30
        }));
        imports.finish(&10).unwrap();
        assert!(imports.blocked());
        assert!(imports.finish(&20).unwrap().attempt.is_none());
        assert!(!imports.blocked());
        assert!(imports.insert(Pending {
            buffer: 3,
            attempt: Some(3),
            params: 30
        }));
    }

    #[test]
    fn retiring_a_stream_drops_every_pending_import_resource() {
        use std::{cell::Cell, rc::Rc};
        #[derive(PartialEq)]
        struct Resource(Rc<Cell<usize>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0));
        let mut imports = Imports::default();
        for id in 1..=MAX_IMPORTS as u64 {
            assert!(imports.insert(Pending {
                buffer: id,
                attempt: Some(id),
                params: Resource(dropped.clone())
            }));
        }
        for pending in imports.drain() {
            drop(pending.params);
        }
        assert_eq!(dropped.get(), MAX_IMPORTS);
        assert!(!imports.full() && !imports.blocked() && !imports.contains(1));
    }
}
