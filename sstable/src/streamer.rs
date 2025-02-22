use std::io;
use std::ops::Bound;

use tantivy_fst::automaton::AlwaysMatch;
use tantivy_fst::Automaton;

use crate::dictionary::Dictionary;
use crate::{SSTable, TermOrdinal};

/// `StreamerBuilder` is a helper object used to define
/// a range of terms that should be streamed.
pub struct StreamerBuilder<'a, TSSTable, A = AlwaysMatch>
where
    A: Automaton,
    A::State: Clone,
    TSSTable: SSTable,
{
    term_dict: &'a Dictionary<TSSTable>,
    automaton: A,
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
}

fn bound_as_byte_slice(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound.as_ref() {
        Bound::Included(key) => Bound::Included(key.as_slice()),
        Bound::Excluded(key) => Bound::Excluded(key.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

impl<'a, TSSTable, A> StreamerBuilder<'a, TSSTable, A>
where
    A: Automaton,
    A::State: Clone,
    TSSTable: SSTable,
{
    pub(crate) fn new(term_dict: &'a Dictionary<TSSTable>, automaton: A) -> Self {
        StreamerBuilder {
            term_dict,
            automaton,
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
        }
    }

    /// Limit the range to terms greater or equal to the bound
    pub fn ge<T: AsRef<[u8]>>(mut self, bound: T) -> Self {
        self.lower = Bound::Included(bound.as_ref().to_owned());
        self
    }

    /// Limit the range to terms strictly greater than the bound
    pub fn gt<T: AsRef<[u8]>>(mut self, bound: T) -> Self {
        self.lower = Bound::Excluded(bound.as_ref().to_owned());
        self
    }

    /// Limit the range to terms lesser or equal to the bound
    pub fn le<T: AsRef<[u8]>>(mut self, bound: T) -> Self {
        self.upper = Bound::Included(bound.as_ref().to_owned());
        self
    }

    /// Limit the range to terms lesser or equal to the bound
    pub fn lt<T: AsRef<[u8]>>(mut self, bound: T) -> Self {
        self.upper = Bound::Excluded(bound.as_ref().to_owned());
        self
    }

    /// Creates the stream corresponding to the range
    /// of terms defined using the `StreamerBuilder`.
    pub fn into_stream(self) -> io::Result<Streamer<'a, TSSTable, A>> {
        // TODO Optimize by skipping to the right first block.
        let start_state = self.automaton.start();
        let key_range = (
            bound_as_byte_slice(&self.lower),
            bound_as_byte_slice(&self.upper),
        );
        let delta_reader = self
            .term_dict
            .sstable_delta_reader_for_key_range(key_range)?;
        Ok(Streamer {
            automaton: self.automaton,
            states: vec![start_state],
            delta_reader,
            key: Vec::new(),
            term_ord: None,
            lower_bound: self.lower,
            upper_bound: self.upper,
        })
    }
}

/// `Streamer` acts as a cursor over a range of terms of a segment.
/// Terms are guaranteed to be sorted.
pub struct Streamer<'a, TSSTable, A = AlwaysMatch>
where
    A: Automaton,
    A::State: Clone,
    TSSTable: SSTable,
{
    automaton: A,
    states: Vec<A::State>,
    delta_reader: crate::DeltaReader<'a, TSSTable::ValueReader>,
    key: Vec<u8>,
    term_ord: Option<TermOrdinal>,
    lower_bound: Bound<Vec<u8>>,
    upper_bound: Bound<Vec<u8>>,
}

impl<'a, TSSTable, A> Streamer<'a, TSSTable, A>
where
    A: Automaton,
    A::State: Clone,
    TSSTable: SSTable,
{
    /// Advance position the stream on the next item.
    /// Before the first call to `.advance()`, the stream
    /// is an uninitialized state.
    pub fn advance(&mut self) -> bool {
        while self.delta_reader.advance().unwrap() {
            self.term_ord = Some(
                self.term_ord
                    .map(|term_ord| term_ord + 1u64)
                    .unwrap_or(0u64),
            );
            let common_prefix_len = self.delta_reader.common_prefix_len();
            self.states.truncate(common_prefix_len + 1);
            self.key.truncate(common_prefix_len);
            let mut state: A::State = self.states.last().unwrap().clone();
            for &b in self.delta_reader.suffix() {
                state = self.automaton.accept(&state, b);
                self.states.push(state.clone());
            }
            self.key.extend_from_slice(self.delta_reader.suffix());
            let match_lower_bound = match &self.lower_bound {
                Bound::Unbounded => true,
                Bound::Included(lower_bound_key) => lower_bound_key[..] <= self.key[..],
                Bound::Excluded(lower_bound_key) => lower_bound_key[..] < self.key[..],
            };
            if !match_lower_bound {
                continue;
            }
            // We match the lower key once. All subsequent keys will pass that bar.
            self.lower_bound = Bound::Unbounded;
            let match_upper_bound = match &self.upper_bound {
                Bound::Unbounded => true,
                Bound::Included(upper_bound_key) => upper_bound_key[..] >= self.key[..],
                Bound::Excluded(upper_bound_key) => upper_bound_key[..] > self.key[..],
            };
            if !match_upper_bound {
                return false;
            }
            if self.automaton.is_match(&state) {
                return true;
            }
        }
        false
    }

    /// Returns the `TermOrdinal` of the given term.
    ///
    /// May panic if the called as `.advance()` as never
    /// been called before.
    pub fn term_ord(&self) -> TermOrdinal {
        self.term_ord.unwrap_or(0u64)
    }

    /// Accesses the current key.
    ///
    /// `.key()` should return the key that was returned
    /// by the `.next()` method.
    ///
    /// If the end of the stream as been reached, and `.next()`
    /// has been called and returned `None`, `.key()` remains
    /// the value of the last key encountered.
    ///
    /// Before any call to `.next()`, `.key()` returns an empty array.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Accesses the current value.
    ///
    /// Calling `.value()` after the end of the stream will return the
    /// last `.value()` encountered.
    ///
    /// # Panics
    ///
    /// Calling `.value()` before the first call to `.advance()` returns
    /// `V::default()`.
    pub fn value(&self) -> &TSSTable::Value {
        self.delta_reader.value()
    }

    /// Return the next `(key, value)` pair.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<(&[u8], &TSSTable::Value)> {
        if self.advance() {
            Some((self.key(), self.value()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use common::OwnedBytes;

    use crate::{Dictionary, MonotonicU64SSTable};

    fn create_test_dictionary() -> io::Result<Dictionary<MonotonicU64SSTable>> {
        let mut dict_builder = Dictionary::<MonotonicU64SSTable>::builder(Vec::new())?;
        dict_builder.insert(b"abaisance", &0)?;
        dict_builder.insert(b"abalation", &1)?;
        dict_builder.insert(b"abalienate", &2)?;
        dict_builder.insert(b"abandon", &3)?;
        let buffer = dict_builder.finish()?;
        let owned_bytes = OwnedBytes::new(buffer);
        Dictionary::from_bytes(owned_bytes)
    }

    #[test]
    fn test_sstable_stream() -> io::Result<()> {
        let dict = create_test_dictionary()?;
        let mut streamer = dict.stream()?;
        assert!(streamer.advance());
        assert_eq!(streamer.key(), b"abaisance");
        assert_eq!(streamer.value(), &0);
        assert!(streamer.advance());
        assert_eq!(streamer.key(), b"abalation");
        assert_eq!(streamer.value(), &1);
        assert!(streamer.advance());
        assert_eq!(streamer.key(), b"abalienate");
        assert_eq!(streamer.value(), &2);
        assert!(streamer.advance());
        assert_eq!(streamer.key(), b"abandon");
        assert_eq!(streamer.value(), &3);
        assert!(!streamer.advance());
        Ok(())
    }

    #[test]
    fn test_sstable_search() -> io::Result<()> {
        let term_dict = create_test_dictionary()?;
        let ptn = tantivy_fst::Regex::new("ab.*t.*").unwrap();
        let mut term_streamer = term_dict.search(ptn).into_stream()?;
        assert!(term_streamer.advance());
        assert_eq!(term_streamer.key(), b"abalation");
        assert_eq!(term_streamer.value(), &1u64);
        assert!(term_streamer.advance());
        assert_eq!(term_streamer.key(), b"abalienate");
        assert_eq!(term_streamer.value(), &2u64);
        assert!(!term_streamer.advance());
        Ok(())
    }
}
