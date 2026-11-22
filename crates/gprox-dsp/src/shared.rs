//! Lock-free single-writer parameter cell for the real-time thread.
//!
//! A sequence lock over `N` 32-bit words, built only from atomics (no
//! `unsafe`, no allocation after construction). The control side owns the
//! one [`ParamWriter`]; the processor owns a [`ParamReader`] and polls it at
//! the start of each block. A read never blocks or spins: if the writer is in
//! the middle of an update the reader reports "nothing new" and picks the
//! value up on the next block.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};

#[derive(Debug)]
struct Cell<const N: usize> {
    /// Even = stable, odd = write in progress. Incremented twice per write.
    seq: AtomicUsize,
    words: [AtomicU32; N],
}

/// The writing end. Not `Clone`: there is exactly one writer per cell.
#[derive(Debug)]
pub struct ParamWriter<const N: usize> {
    cell: Arc<Cell<N>>,
}

/// The reading end, used on the real-time thread.
#[derive(Debug)]
pub struct ParamReader<const N: usize> {
    cell: Arc<Cell<N>>,
    seen: usize,
}

/// Create a connected writer/reader pair. The reader reports the initial
/// words as new on its first poll.
pub fn param_cell<const N: usize>(initial: [u32; N]) -> (ParamWriter<N>, ParamReader<N>) {
    let cell = Arc::new(Cell {
        seq: AtomicUsize::new(2),
        words: initial.map(AtomicU32::new),
    });
    (
        ParamWriter { cell: cell.clone() },
        ParamReader { cell, seen: 0 },
    )
}

impl<const N: usize> ParamWriter<N> {
    /// Publish a new set of words. Wait-free.
    pub fn write(&mut self, words: &[u32; N]) {
        let c = &*self.cell;
        let s = c.seq.load(Ordering::Relaxed);
        c.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (dst, &w) in c.words.iter().zip(words) {
            dst.store(w, Ordering::Relaxed);
        }
        c.seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

impl<const N: usize> ParamReader<N> {
    /// Return the words if they changed since the last successful poll and
    /// a consistent snapshot could be taken. Wait-free; RT safe.
    pub fn poll(&mut self) -> Option<[u32; N]> {
        let c = &*self.cell;
        let s1 = c.seq.load(Ordering::Acquire);
        if s1 == self.seen || s1 & 1 == 1 {
            return None;
        }
        let mut out = [0u32; N];
        for (o, w) in out.iter_mut().zip(c.words.iter()) {
            *o = w.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        let s2 = c.seq.load(Ordering::Relaxed);
        if s1 != s2 {
            return None;
        }
        self.seen = s1;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_then_updates_then_quiet() {
        let (mut w, mut r) = param_cell([1, 2]);
        assert_eq!(r.poll(), Some([1, 2]));
        assert_eq!(r.poll(), None);
        w.write(&[3, 4]);
        w.write(&[5, 6]);
        assert_eq!(r.poll(), Some([5, 6]));
        assert_eq!(r.poll(), None);
    }

    #[test]
    fn concurrent_reads_are_never_torn() {
        let (mut w, mut r) = param_cell([0u32; 8]);
        let t = std::thread::spawn(move || {
            for i in 1..=200_000u32 {
                w.write(&[i; 8]);
            }
        });
        let mut last = 0;
        while !t.is_finished() {
            if let Some(v) = r.poll() {
                assert!(v.iter().all(|&x| x == v[0]), "torn read {v:?}");
                assert!(v[0] >= last);
                last = v[0];
            }
        }
        t.join().unwrap();
        // Drain whatever was written last.
        if let Some(v) = r.poll() {
            last = v[0];
        }
        assert_eq!(last, 200_000);
    }
}
