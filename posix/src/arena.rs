// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Anonymous memory arenas (root `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §5, W9).
//!
//! A job may create only a few MemoryObjects (four, at most 16 MiB in all)
//! and its root region has few mapping slots, so an `mmap` cannot be one
//! kernel mapping. The layer maps a few large read-write arenas instead and
//! hands out page runs from them, first fit. `munmap` returns pages to their
//! arena, which keeps them mapped and marks them dirty; a run handed out
//! again is zeroed first if any of its pages is dirty, so anonymous memory
//! always starts zeroed. Arenas are never unmapped.

/// An arena's usual size: 4 MiB.
pub const ARENA_PAGES: u64 = 1024;
/// The largest arena, and so the largest single allocation: 16 MiB, a job's
/// whole page budget.
pub const MAX_ARENA_PAGES: u64 = 4096;
/// A job's MemoryObject budget.
pub const MAX_ARENAS: usize = 4;
pub const PAGE_SIZE: u64 = 4096;
const WORDS: usize = (MAX_ARENA_PAGES / 64) as usize;

pub struct Arena {
    base: u64,
    pages: u64,
    used: [u64; WORDS],
    dirty: [u64; WORDS],
}

fn test(bits: &[u64; WORDS], page: u64) -> bool {
    bits[(page / 64) as usize] & (1 << (page % 64)) != 0
}

fn set(bits: &mut [u64; WORDS], page: u64, value: bool) {
    let word = &mut bits[(page / 64) as usize];
    if value {
        *word |= 1 << (page % 64);
    } else {
        *word &= !(1 << (page % 64));
    }
}

impl Arena {
    /// A fresh, zeroed arena of `pages` pages mapped at `base`.
    pub const fn new(base: u64, pages: u64) -> Self {
        assert!(pages <= MAX_ARENA_PAGES);
        Self {
            base,
            pages,
            used: [0; WORDS],
            dirty: [0; WORDS],
        }
    }

    pub const fn base(&self) -> u64 {
        self.base
    }

    pub const fn end(&self) -> u64 {
        self.base + self.pages * PAGE_SIZE
    }

    /// Takes the first run of `count` free pages: its address, and whether
    /// any of it was used before and so must be zeroed.
    pub fn allocate(&mut self, count: u64) -> Option<(u64, bool)> {
        if count == 0 || count > self.pages {
            return None;
        }
        let mut start = 0;
        while start + count <= self.pages {
            match (start..start + count).find(|&page| test(&self.used, page)) {
                Some(taken) => start = taken + 1,
                None => {
                    let mut dirty = false;
                    for page in start..start + count {
                        set(&mut self.used, page, true);
                        dirty |= test(&self.dirty, page);
                        set(&mut self.dirty, page, false);
                    }
                    return Some((self.base + start * PAGE_SIZE, dirty));
                }
            }
        }
        None
    }

    /// Returns `[address, address + length)`, page-aligned and inside this
    /// arena, to the free pages. Free pages in the range stay free.
    pub fn release(&mut self, address: u64, length: u64) {
        let first = (address - self.base) / PAGE_SIZE;
        for page in first..first + length / PAGE_SIZE {
            if test(&self.used, page) {
                set(&mut self.used, page, false);
                set(&mut self.dirty, page, true);
            }
        }
    }

    pub fn pages_in_use(&self) -> u64 {
        self.used
            .iter()
            .map(|word| u64::from(word.count_ones()))
            .sum()
    }
}

/// Where a range lies relative to the arenas.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    /// Wholly inside arena `index`.
    Inside(usize),
    /// Outside every arena.
    Outside,
    /// Partly inside one: no operation may split an arena.
    Straddling,
}

pub struct Arenas {
    arenas: [Option<Arena>; MAX_ARENAS],
}

impl Default for Arenas {
    fn default() -> Self {
        Self::new()
    }
}

impl Arenas {
    pub const fn new() -> Self {
        Self {
            arenas: [const { None }; MAX_ARENAS],
        }
    }

    /// A run of `count` pages from the existing arenas.
    pub fn allocate(&mut self, count: u64) -> Option<(u64, bool)> {
        self.arenas
            .iter_mut()
            .flatten()
            .find_map(|arena| arena.allocate(count))
    }

    pub fn is_full(&self) -> bool {
        self.arenas.iter().all(Option::is_some)
    }

    /// Adds a newly mapped arena; `false` if every slot is taken.
    pub fn add(&mut self, arena: Arena) -> bool {
        match self.arenas.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(arena);
                true
            }
            None => false,
        }
    }

    pub fn place(&self, address: u64, length: u64) -> Place {
        let end = address.saturating_add(length);
        for (index, arena) in self.arenas.iter().enumerate() {
            let Some(arena) = arena else { continue };
            if address >= arena.base() && end <= arena.end() {
                return Place::Inside(index);
            }
            if address < arena.end() && arena.base() < end {
                return Place::Straddling;
            }
        }
        Place::Outside
    }

    pub fn release(&mut self, index: usize, address: u64, length: u64) {
        if let Some(arena) = &mut self.arenas[index] {
            arena.release(address, length);
        }
    }

    pub fn arena(&self, index: usize) -> Option<&Arena> {
        self.arenas.get(index)?.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x4000_0000;

    #[test]
    fn runs_are_first_fit_and_fresh_pages_need_no_zeroing() {
        let mut arena = Arena::new(BASE, 8);
        assert_eq!(arena.allocate(3), Some((BASE, false)));
        assert_eq!(arena.allocate(2), Some((BASE + 3 * PAGE_SIZE, false)));
        assert_eq!(arena.allocate(4), None);
        assert_eq!(arena.allocate(3), Some((BASE + 5 * PAGE_SIZE, false)));
        assert_eq!(arena.allocate(1), None);
        assert_eq!(arena.pages_in_use(), 8);
    }

    #[test]
    fn released_pages_are_reused_and_zeroed_first() {
        let mut arena = Arena::new(BASE, 8);
        let (first, _) = arena.allocate(4).unwrap();
        arena.allocate(4).unwrap();
        arena.release(first + PAGE_SIZE, 2 * PAGE_SIZE);
        assert_eq!(arena.pages_in_use(), 6);
        // The hole is reused, and it was used before: zero it.
        assert_eq!(arena.allocate(2), Some((first + PAGE_SIZE, true)));
        // Releasing twice, or releasing free pages, changes nothing.
        arena.release(first, PAGE_SIZE);
        arena.release(first, PAGE_SIZE);
        assert_eq!(arena.pages_in_use(), 7);
        assert_eq!(arena.allocate(1), Some((first, true)));
        assert_eq!(arena.allocate(0), None);
    }

    #[test]
    fn ranges_are_inside_outside_or_straddling() {
        let mut arenas = Arenas::new();
        assert!(arenas.add(Arena::new(BASE, 4)));
        assert_eq!(arenas.place(BASE + PAGE_SIZE, PAGE_SIZE), Place::Inside(0));
        assert_eq!(arenas.place(BASE, 4 * PAGE_SIZE), Place::Inside(0));
        assert_eq!(
            arenas.place(BASE + 3 * PAGE_SIZE, 2 * PAGE_SIZE),
            Place::Straddling
        );
        assert_eq!(
            arenas.place(BASE - PAGE_SIZE, 2 * PAGE_SIZE),
            Place::Straddling
        );
        assert_eq!(
            arenas.place(BASE + 4 * PAGE_SIZE, PAGE_SIZE),
            Place::Outside
        );
        for index in 1..MAX_ARENAS {
            assert!(arenas.add(Arena::new(BASE * (index as u64 + 1), 4)));
        }
        assert!(arenas.is_full());
        assert!(!arenas.add(Arena::new(BASE * 9, 4)));
    }

    #[test]
    fn an_arena_can_be_the_whole_budget() {
        let mut arena = Arena::new(BASE, MAX_ARENA_PAGES);
        assert_eq!(arena.allocate(MAX_ARENA_PAGES), Some((BASE, false)));
        assert_eq!(arena.allocate(1), None);
    }
}
