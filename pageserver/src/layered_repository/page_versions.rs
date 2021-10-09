use std::{collections::HashMap, ops::RangeBounds};

use zenith_utils::{accum::Accum, lsn::Lsn, vec_map::VecMap};

use super::storage_layer::PageVersion;

const EMPTY_SLICE: &[(Lsn, PageVersion)] = &[];

#[derive(Debug, Default)]
pub struct PageVersions(HashMap<u32, VecMap<Lsn, PageVersion>>);

impl PageVersions {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn append_or_update_last(
        &mut self,
        blknum: u32,
        lsn: Lsn,
        page_version: PageVersion,
    ) -> Option<PageVersion> {
        let map = self.0.entry(blknum).or_insert_with(VecMap::default);
        map.append_or_update_last(lsn, page_version).unwrap()
    }

    /// Get a range of [`PageVersions`] in a block
    pub fn get_block_lsn_range<R: RangeBounds<Lsn>>(
        &self,
        blknum: u32,
        range: R,
    ) -> &[(Lsn, PageVersion)] {
        self.0
            .get(&blknum)
            .map(|vec_map| vec_map.slice_range(range))
            .unwrap_or(EMPTY_SLICE)
    }

    /// Split the page version map into two.
    ///
    /// Left contains everything up to and not including [`cutoff_lsn`].
    /// Right contains [`cutoff_lsn`] and everything after.
    pub fn split_at(&self, cutoff_lsn: Lsn, after_oldest_lsn: &mut Accum<Lsn>) -> (Self, Self) {
        let mut before_blocks = HashMap::new();
        let mut after_blocks = HashMap::new();

        for (blknum, vec_map) in self.0.iter() {
            let (before_versions, after_versions) = vec_map.split_at(&cutoff_lsn);

            if !before_versions.is_empty() {
                let old = before_blocks.insert(*blknum, before_versions);
                assert!(old.is_none());
            }

            if !after_versions.is_empty() {
                let (first_lsn, _first_pv) = &after_versions.as_slice()[0];
                after_oldest_lsn.accum(std::cmp::min, *first_lsn);

                let old = after_blocks.insert(*blknum, after_versions);
                assert!(old.is_none());
            }
        }

        (Self(before_blocks), Self(after_blocks))
    }

    /// Iterate through block-history pairs in block order.
    pub fn ordered_block_iter(&self) -> OrderedBlockIter<'_> {
        let mut ordered_blocks: Vec<u32> = self.0.keys().cloned().collect();
        ordered_blocks.sort_unstable();

        OrderedBlockIter {
            page_versions: self,
            ordered_blocks,
            cur_block_idx: 0,
        }
    }
}

pub struct OrderedBlockIter<'a> {
    page_versions: &'a PageVersions,

    ordered_blocks: Vec<u32>,
    cur_block_idx: usize,
}

impl<'a> Iterator for OrderedBlockIter<'a> {
    type Item = (u32, &'a VecMap<Lsn, PageVersion>);

    fn next(&mut self) -> Option<Self::Item> {
        let blknum: u32 = *self.ordered_blocks.get(self.cur_block_idx)?;
        self.cur_block_idx += 1;
        Some((blknum, self.page_versions.0.get(&blknum).unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_PAGE_VERSION: PageVersion = PageVersion {
        page_image: None,
        record: None,
    };

    #[test]
    fn test_ordered_iter() {
        let mut page_versions = PageVersions::default();
        const BLOCKS: u32 = 1000;
        const LSNS: u64 = 50;

        for blknum in 0..BLOCKS {
            for lsn in 0..LSNS {
                let old = page_versions.append_or_update_last(blknum, Lsn(lsn), EMPTY_PAGE_VERSION);
                assert!(old.is_none());
            }
        }

        let mut iter = page_versions.ordered_block_iter();
        for blknum in 0..BLOCKS {
            let (actual_blknum, vec_map) = iter.next().unwrap();
            let slice = vec_map.as_slice();
            assert_eq!(actual_blknum, blknum);
            assert_eq!(slice.len(), LSNS as usize);
            for lsn in 0..LSNS {
                assert_eq!(Lsn(lsn), slice[lsn as usize].0);
            }
        }
        assert!(iter.next().is_none());
        assert!(iter.next().is_none()); // should be robust against excessive next() calls
    }
}