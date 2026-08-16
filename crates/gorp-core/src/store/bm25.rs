//! The flat, mmap-able BM25 layout written into an index directory.
//!
//! Provenance showed postcard-deserializing the old `bm25.bin` cost ~840 ms per
//! warm query on the kernel corpus — 21% of a hybrid query. This layout is
//! binary-searched in place, so a query touches only its own terms' postings
//! and pays nothing for the rest of the corpus.

use crate::rank::bm25::{Bm25Index, Postings, top_k};

const FLAT_MAGIC: &[u8; 8] = b"SGBM25F2";

/// Serialize an in-memory index into the flat mmap-able layout.
pub fn to_flat_bytes(idx: &Bm25Index) -> Vec<u8> {
    let mut terms: Vec<(&str, u32)> = idx.term_table().collect();
    terms.sort_unstable_by_key(|&(s, _)| s);
    let n_terms = terms.len();

    let mut term_offs: Vec<u32> = Vec::with_capacity(n_terms + 1);
    let mut term_bytes: Vec<u8> = Vec::new();
    let mut post_offs: Vec<u64> = Vec::with_capacity(n_terms + 1);
    let mut postings: Vec<u8> = Vec::new();
    for &(s, id) in &terms {
        term_offs.push(term_bytes.len() as u32);
        term_bytes.extend_from_slice(s.as_bytes());
        post_offs.push(postings.len() as u64);
        for &(chunk, tf) in idx.postings_of(id) {
            postings.extend_from_slice(&chunk.to_le_bytes());
            postings.extend_from_slice(&tf.to_le_bytes());
        }
    }
    term_offs.push(term_bytes.len() as u32);
    post_offs.push(postings.len() as u64);

    let align8 = |v: &mut Vec<u8>| v.resize(v.len().next_multiple_of(8), 0);
    let mut out = Vec::new();
    out.extend_from_slice(FLAT_MAGIC);
    out.extend_from_slice(&(idx.doc_lens().len() as u32).to_le_bytes());
    out.extend_from_slice(&(n_terms as u32).to_le_bytes());
    out.extend_from_slice(&idx.total_len().to_le_bytes());
    // reserve section-offset slots
    let off_slots = out.len();
    out.extend_from_slice(&[0u8; 40]);

    let mut offsets = [0u64; 5];
    let sections: [&[u8]; 5] = [
        bytemuck_cast_u32(&term_offs),
        &term_bytes,
        bytemuck_cast_u64(&post_offs),
        &postings,
        bytemuck_cast_u32(idx.doc_lens()),
    ];
    // Two-pass not needed: write sequentially, recording aligned starts.
    let mut buf = out;
    for (i, s) in sections.iter().enumerate() {
        align8(&mut buf);
        offsets[i] = buf.len() as u64;
        buf.extend_from_slice(s);
    }
    for (i, off) in offsets.iter().enumerate() {
        buf[off_slots + i * 8..off_slots + (i + 1) * 8].copy_from_slice(&off.to_le_bytes());
    }
    buf
}

fn bytemuck_cast_u32(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}
fn bytemuck_cast_u64(v: &[u64]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
}

/// Reader over the flat layout. Holds the mmap; all access is in place.
pub struct FlatBm25 {
    map: memmap2::Mmap,
    n_docs: u32,
    n_terms: u32,
    total_len: u64,
    off: [u64; 5],
}

/// 8 magic + 4 n_docs + 4 n_terms + 8 total_len + 5×8 section offsets.
///
/// Exactly the length the old `map.len() >= 64` check accepted, which is what
/// made the check useless: a file truncated to precisely its own header passed
/// validation and then handed out five section offsets pointing far past the
/// end of the file.
const HEADER_BYTES: usize = 64;

impl FlatBm25 {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let map = unsafe { memmap2::Mmap::map(&file)? };
        anyhow::ensure!(
            map.len() >= HEADER_BYTES && &map[..8] == FLAT_MAGIC,
            "bad bm25.flat header"
        );
        let u32_at = |o: usize| u32::from_le_bytes(map[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(map[o..o + 8].try_into().unwrap());
        let n_docs = u32_at(8);
        let n_terms = u32_at(12);
        let total_len = u64_at(16);
        let mut off = [0u64; 5];
        for (i, o) in off.iter_mut().enumerate() {
            *o = u64_at(24 + i * 8);
        }
        let this = Self { map, n_docs, n_terms, total_len, off };
        this.validate()?;
        Ok(this)
    }

    /// Check that every section named by the header actually lies inside the
    /// file, before anything indexes into one.
    ///
    /// This has to return `Err`, not panic, and the distinction is the whole
    /// point. `search::search` treats *any* failure to read a cache entry as a
    /// miss: it deletes the entry and answers from the streaming path. A panic
    /// is not a failure it can catch, so a corrupt `bm25.flat` used to abort the
    /// process, leave the entry in place, and abort the next invocation too —
    /// a permanently unusable cache that only `gorp cache --clear` could
    /// recover. Observed on three separate corruptions, three runs each, all
    /// exit 101 (SIMULATION.md §1.1).
    ///
    /// Deliberately O(1): the three fixed-size tables are bounds-checked here,
    /// and the two variable-length sections are checked against the last entry
    /// of their own table. Validating that every offset in those tables is
    /// monotonic would mean reading them end to end, which would page in tens of
    /// megabytes on a kernel-scale index and undo the reason this layout exists.
    /// The accessors stay bounds-safe instead, so a table that is in-bounds but
    /// internally inconsistent yields empty results rather than a crash.
    fn validate(&self) -> anyhow::Result<()> {
        let len = self.map.len() as u64;
        let n_terms = self.n_terms as u64;
        let n_docs = self.n_docs as u64;

        // term_offs and post_offs are (n_terms + 1) entries; doc_lens is n_docs.
        for (i, size, what) in [
            (0usize, (n_terms + 1) * 4, "term offset table"),
            (2, (n_terms + 1) * 8, "posting offset table"),
            (4, n_docs * 4, "document length table"),
        ] {
            let end = self.off[i].checked_add(size).ok_or_else(|| {
                anyhow::anyhow!("bm25.flat {what} offset overflows; index is corrupt")
            })?;
            anyhow::ensure!(
                end <= len,
                "bm25.flat {what} ends at {end} but the file is {len} bytes; \
                 index is corrupt — re-run `gorp index`"
            );
        }

        // The two byte blobs are as long as the last entry of their table says,
        // which is only safe to read now that the tables are known to fit.
        let term_bytes = self.u32_le(self.off[0] as usize + n_terms as usize * 4) as u64;
        let post_bytes = {
            let o = self.off[2] as usize + n_terms as usize * 8;
            u64::from_le_bytes(self.map[o..o + 8].try_into().unwrap())
        };
        for (i, size, what) in [
            (1usize, term_bytes, "term text"),
            (3, post_bytes, "postings"),
        ] {
            let end = self.off[i].checked_add(size).ok_or_else(|| {
                anyhow::anyhow!("bm25.flat {what} offset overflows; index is corrupt")
            })?;
            anyhow::ensure!(
                end <= len,
                "bm25.flat {what} section ends at {end} but the file is {len} bytes; \
                 index is corrupt — re-run `gorp index`"
            );
        }
        anyhow::ensure!(
            post_bytes % 6 == 0,
            "bm25.flat postings section is {post_bytes} bytes, not a multiple of \
             the 6-byte record; index is corrupt — re-run `gorp index`"
        );
        Ok(())
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs as usize
    }

    #[inline]
    fn u32_le(&self, byte_off: usize) -> u32 {
        self.map
            .get(byte_off..byte_off + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .unwrap_or(0)
    }

    /// A byte range of the map, or empty if it does not lie inside it.
    ///
    /// `validate` bounds the *sections*; this bounds an individual entry, whose
    /// start and end come out of a table that a corrupt file can make
    /// non-monotonic or out of range. Returning empty rather than panicking
    /// keeps the "a bad cache entry is a miss" contract intact for the cases
    /// an O(1) header check cannot reach.
    #[inline]
    fn slice(&self, start: usize, end: usize) -> &[u8] {
        if end < start {
            return &[];
        }
        self.map.get(start..end).unwrap_or(&[])
    }

    #[inline]
    fn term_at(&self, i: usize) -> &[u8] {
        let base = self.off[0] as usize;
        let lo = self.u32_le(base + i * 4) as usize;
        let hi = self.u32_le(base + (i + 1) * 4) as usize;
        let tb = self.off[1] as usize;
        self.slice(tb.saturating_add(lo), tb.saturating_add(hi))
    }

    fn find_term(&self, term: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.n_terms as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.term_at(mid).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// (chunk_id, tf) postings for the i-th sorted term.
    fn postings_at(&self, i: usize) -> impl Iterator<Item = (u32, u16)> + '_ {
        let base = self.off[2] as usize;
        let u64_at = |o: usize| {
            self.map
                .get(o..o + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0)
        };
        let lo = u64_at(base + i * 8) as usize;
        let hi = u64_at(base + (i + 1) * 8) as usize;
        let pb = self.off[3] as usize;
        self.slice(pb.saturating_add(lo), pb.saturating_add(hi)).chunks_exact(6).map(|rec| {
            (
                u32::from_le_bytes(rec[..4].try_into().unwrap()),
                u16::from_le_bytes(rec[4..6].try_into().unwrap()),
            )
        })
    }

    /// Length of one document. `chunk_id` comes out of a postings record, so a
    /// corrupt file can make it arbitrary; `u32_le` returns 0 for anything
    /// outside the map, which scores that chunk as empty rather than crashing.
    #[inline]
    fn doc_len_at(&self, chunk_id: u32) -> u32 {
        self.u32_le(self.off[4] as usize + chunk_id as usize * 4)
    }

    /// Top-k chunks by BM25 — the same scorer [`Bm25Index::query`] uses.
    pub fn query(&self, query: &str, k: usize) -> Vec<(u32, f32)> {
        top_k(self, query, k)
    }
}

impl Postings for FlatBm25 {
    fn n_docs(&self) -> usize {
        self.n_docs as usize
    }

    fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Term ids here are positions in the sorted term table, so they order the
    /// accumulation by term string — the same relation the in-memory store now
    /// has, which is why the two agree bit for bit.
    fn term_id(&self, token: &str) -> Option<u32> {
        self.find_term(token).map(|i| i as u32)
    }

    fn postings(&self, term: u32) -> impl Iterator<Item = (u32, u16)> + '_ {
        self.postings_at(term as usize)
    }

    fn doc_len(&self, chunk_id: u32) -> u32 {
        self.doc_len_at(chunk_id)
    }
}

#[cfg(test)]
mod tests;
