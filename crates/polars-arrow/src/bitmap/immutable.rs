use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use either::Either;
use polars_error::{polars_bail, PolarsResult};

use super::utils::{count_zeros, fmt, get_bit, get_bit_unchecked, BitChunk, BitChunks, BitmapIter};
use super::{chunk_iter_to_vec, IntoIter, MutableBitmap};
use crate::bitmap::iterator::{
    FastU32BitmapIter, FastU56BitmapIter, FastU64BitmapIter, TrueIdxIter,
};
use crate::buffer::Bytes;
use crate::trusted_len::TrustedLen;

const UNKNOWN_BIT_COUNT: u64 = u64::MAX;

/// An immutable container semantically equivalent to `Arc<Vec<bool>>` but represented as `Arc<Vec<u8>>` where
/// each boolean is represented as a single bit.
///
/// # Examples
/// ```
/// use polars_arrow::bitmap::{Bitmap, MutableBitmap};
///
/// let bitmap = Bitmap::from([true, false, true]);
/// assert_eq!(bitmap.iter().collect::<Vec<_>>(), vec![true, false, true]);
///
/// // creation directly from bytes
/// let bitmap = Bitmap::try_new(vec![0b00001101], 5).unwrap();
/// // note: the first bit is the left-most of the first byte
/// assert_eq!(bitmap.iter().collect::<Vec<_>>(), vec![true, false, true, true, false]);
/// // we can also get the slice:
/// assert_eq!(bitmap.as_slice(), ([0b00001101u8].as_ref(), 0, 5));
/// // debug helps :)
/// assert_eq!(format!("{:?}", bitmap), "[0b___01101]".to_string());
///
/// // it supports copy-on-write semantics (to a `MutableBitmap`)
/// let bitmap: MutableBitmap = bitmap.into_mut().right().unwrap();
/// assert_eq!(bitmap, MutableBitmap::from([true, false, true, true, false]));
///
/// // slicing is 'O(1)' (data is shared)
/// let bitmap = Bitmap::try_new(vec![0b00001101], 5).unwrap();
/// let mut sliced = bitmap.clone();
/// sliced.slice(1, 4);
/// assert_eq!(sliced.as_slice(), ([0b00001101u8].as_ref(), 1, 4)); // 1 here is the offset:
/// assert_eq!(format!("{:?}", sliced), "[0b___0110_]".to_string());
/// // when sliced (or cloned), it is no longer possible to `into_mut`.
/// let same: Bitmap = sliced.into_mut().left().unwrap();
/// ```
pub struct Bitmap {
    bytes: Arc<Bytes<u8>>,
    // Both offset and length are measured in bits. They are used to bound the
    // bitmap to a region of Bytes.
    offset: usize,
    length: usize,

    // A bit field that contains our cache for the number of unset bits.
    // If it is u64::MAX, we have no known value at all.
    // Other bit patterns where the top bit is set is reserved for future use.
    // If the top bit is not set we have an exact count.
    unset_bit_count_cache: AtomicU64,
}

impl Clone for Bitmap {
    fn clone(&self) -> Self {
        Self {
            bytes: Arc::clone(&self.bytes),
            offset: self.offset,
            length: self.length,
            unset_bit_count_cache: AtomicU64::new(
                self.unset_bit_count_cache.load(Ordering::Relaxed),
            ),
        }
    }
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (bytes, offset, len) = self.as_slice();
        fmt(bytes, offset, len, f)
    }
}

impl Default for Bitmap {
    fn default() -> Self {
        MutableBitmap::new().into()
    }
}

pub(super) fn check(bytes: &[u8], offset: usize, length: usize) -> PolarsResult<()> {
    if offset + length > bytes.len().saturating_mul(8) {
        polars_bail!(InvalidOperation:
            "The offset + length of the bitmap ({}) must be `<=` to the number of bytes times 8 ({})",
            offset + length,
            bytes.len().saturating_mul(8)
        );
    }
    Ok(())
}

impl Bitmap {
    /// Initializes an empty [`Bitmap`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Initializes a new [`Bitmap`] from vector of bytes and a length.
    /// # Errors
    /// This function errors iff `length > bytes.len() * 8`
    #[inline]
    pub fn try_new(bytes: Vec<u8>, length: usize) -> PolarsResult<Self> {
        check(&bytes, 0, length)?;
        Ok(Self {
            length,
            offset: 0,
            bytes: Arc::new(bytes.into()),
            unset_bit_count_cache: AtomicU64::new(UNKNOWN_BIT_COUNT),
        })
    }

    /// Returns the length of the [`Bitmap`].
    #[inline]
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns whether [`Bitmap`] is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a new iterator of `bool` over this bitmap
    pub fn iter(&self) -> BitmapIter {
        BitmapIter::new(&self.bytes, self.offset, self.length)
    }

    /// Returns an iterator over bits in bit chunks [`BitChunk`].
    ///
    /// This iterator is useful to operate over multiple bits via e.g. bitwise.
    pub fn chunks<T: BitChunk>(&self) -> BitChunks<T> {
        BitChunks::new(&self.bytes, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 32 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u32(&self) -> FastU32BitmapIter<'_> {
        FastU32BitmapIter::new(&self.bytes, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 56 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u56(&self) -> FastU56BitmapIter<'_> {
        FastU56BitmapIter::new(&self.bytes, self.offset, self.length)
    }

    /// Returns a fast iterator that gives 64 bits at a time.
    /// Has a remainder that must be handled separately.
    pub fn fast_iter_u64(&self) -> FastU64BitmapIter<'_> {
        FastU64BitmapIter::new(&self.bytes, self.offset, self.length)
    }

    /// Returns an iterator that only iterates over the set bits.
    pub fn true_idx_iter(&self) -> TrueIdxIter<'_> {
        TrueIdxIter::new(self.len(), Some(self))
    }

    /// Returns the byte slice of this [`Bitmap`].
    ///
    /// The returned tuple contains:
    /// * `.1`: The byte slice, truncated to the start of the first bit. So the start of the slice
    ///       is within the first 8 bits.
    /// * `.2`: The start offset in bits on a range `0 <= offsets < 8`.
    /// * `.3`: The length in number of bits.
    #[inline]
    pub fn as_slice(&self) -> (&[u8], usize, usize) {
        let start = self.offset / 8;
        let len = (self.offset % 8 + self.length).saturating_add(7) / 8;
        (
            &self.bytes[start..start + len],
            self.offset % 8,
            self.length,
        )
    }

    /// Returns the number of set bits on this [`Bitmap`].
    ///
    /// See `unset_bits` for details.
    #[inline]
    pub fn set_bits(&self) -> usize {
        self.length - self.unset_bits()
    }

    /// Returns the number of set bits on this [`Bitmap`] if it is known.
    ///
    /// See `lazy_unset_bits` for details.
    #[inline]
    pub fn lazy_set_bits(&self) -> Option<usize> {
        Some(self.length - self.lazy_unset_bits()?)
    }

    /// Returns the number of unset bits on this [`Bitmap`].
    ///
    /// Guaranteed to be `<= self.len()`.
    ///
    /// # Implementation
    ///
    /// This function counts the number of unset bits if it is not already
    /// computed. Repeated calls use the cached bitcount.
    pub fn unset_bits(&self) -> usize {
        let cache = self.unset_bit_count_cache.load(Ordering::Relaxed);
        if cache >> 63 != 0 {
            let zeros = count_zeros(&self.bytes, self.offset, self.length);
            self.unset_bit_count_cache
                .store(zeros as u64, Ordering::Relaxed);
            zeros
        } else {
            cache as usize
        }
    }

    /// Returns the number of unset bits on this [`Bitmap`] if it is known.
    ///
    /// Guaranteed to be `<= self.len()`.
    pub fn lazy_unset_bits(&self) -> Option<usize> {
        let cache = self.unset_bit_count_cache.load(Ordering::Relaxed);
        if cache >> 63 != 0 {
            None
        } else {
            Some(cache as usize)
        }
    }

    /// Updates the count of the number of set bits on this [`Bitmap`].
    ///
    /// # Safety
    ///
    /// The number of set bits must be correct.
    pub unsafe fn update_bit_count(&mut self, bits_set: usize) {
        assert!(bits_set <= self.length);
        let zeros = self.length - bits_set;
        self.unset_bit_count_cache
            .store(zeros as u64, Ordering::Relaxed);
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    /// # Panic
    /// Panics iff `offset + length > self.length`, i.e. if the offset and `length`
    /// exceeds the allocated capacity of `self`.
    #[inline]
    pub fn slice(&mut self, offset: usize, length: usize) {
        assert!(offset + length <= self.length);
        unsafe { self.slice_unchecked(offset, length) }
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    ///
    /// # Safety
    /// The caller must ensure that `self.offset + offset + length <= self.len()`
    #[inline]
    pub unsafe fn slice_unchecked(&mut self, offset: usize, length: usize) {
        // Fast path: no-op slice.
        if offset == 0 && length == self.length {
            return;
        }

        // Fast path: we have no nulls or are full-null.
        let unset_bit_count_cache = self.unset_bit_count_cache.get_mut();
        if *unset_bit_count_cache == 0 || *unset_bit_count_cache == self.length as u64 {
            let new_count = if *unset_bit_count_cache > 0 {
                length as u64
            } else {
                0
            };
            *unset_bit_count_cache = new_count;
            self.offset += offset;
            self.length = length;
            return;
        }

        if *unset_bit_count_cache >> 63 == 0 {
            // If we keep all but a small portion of the array it is worth
            // doing an eager re-count since we can reuse the old count via the
            // inclusion-exclusion principle.
            let small_portion = (self.length / 5).max(32);
            if length + small_portion >= self.length {
                // Subtract the null count of the chunks we slice off.
                let slice_end = self.offset + offset + length;
                let head_count = count_zeros(&self.bytes, self.offset, offset);
                let tail_count = count_zeros(&self.bytes, slice_end, self.length - length - offset);
                let new_count = *unset_bit_count_cache - head_count as u64 - tail_count as u64;
                *unset_bit_count_cache = new_count;
            } else {
                *unset_bit_count_cache = UNKNOWN_BIT_COUNT;
            }
        }

        self.offset += offset;
        self.length = length;
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    /// # Panic
    /// Panics iff `offset + length > self.length`, i.e. if the offset and `length`
    /// exceeds the allocated capacity of `self`.
    #[inline]
    #[must_use]
    pub fn sliced(self, offset: usize, length: usize) -> Self {
        assert!(offset + length <= self.length);
        unsafe { self.sliced_unchecked(offset, length) }
    }

    /// Slices `self`, offsetting by `offset` and truncating up to `length` bits.
    ///
    /// # Safety
    /// The caller must ensure that `self.offset + offset + length <= self.len()`
    #[inline]
    #[must_use]
    pub unsafe fn sliced_unchecked(mut self, offset: usize, length: usize) -> Self {
        self.slice_unchecked(offset, length);
        self
    }

    /// Returns whether the bit at position `i` is set.
    /// # Panics
    /// Panics iff `i >= self.len()`.
    #[inline]
    pub fn get_bit(&self, i: usize) -> bool {
        get_bit(&self.bytes, self.offset + i)
    }

    /// Unsafely returns whether the bit at position `i` is set.
    ///
    /// # Safety
    /// Unsound iff `i >= self.len()`.
    #[inline]
    pub unsafe fn get_bit_unchecked(&self, i: usize) -> bool {
        get_bit_unchecked(&self.bytes, self.offset + i)
    }

    /// Returns a pointer to the start of this [`Bitmap`] (ignores `offsets`)
    /// This pointer is allocated iff `self.len() > 0`.
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.bytes.deref().as_ptr()
    }

    /// Returns a pointer to the start of this [`Bitmap`] (ignores `offsets`)
    /// This pointer is allocated iff `self.len() > 0`.
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Converts this [`Bitmap`] to [`MutableBitmap`], returning itself if the conversion
    /// is not possible
    ///
    /// This operation returns a [`MutableBitmap`] iff:
    /// * this [`Bitmap`] is not an offsetted slice of another [`Bitmap`]
    /// * this [`Bitmap`] has not been cloned (i.e. [`Arc`]`::get_mut` yields [`Some`])
    /// * this [`Bitmap`] was not imported from the c data interface (FFI)
    pub fn into_mut(mut self) -> Either<Self, MutableBitmap> {
        match (
            self.offset,
            Arc::get_mut(&mut self.bytes).and_then(|b| b.get_vec()),
        ) {
            (0, Some(v)) => {
                let data = std::mem::take(v);
                Either::Right(MutableBitmap::from_vec(data, self.length))
            },
            _ => Either::Left(self),
        }
    }

    /// Converts this [`Bitmap`] into a [`MutableBitmap`], cloning its internal
    /// buffer if required (clone-on-write).
    pub fn make_mut(self) -> MutableBitmap {
        match self.into_mut() {
            Either::Left(data) => {
                if data.offset > 0 {
                    // re-align the bits (remove the offset)
                    let chunks = data.chunks::<u64>();
                    let remainder = chunks.remainder();
                    let vec = chunk_iter_to_vec(chunks.chain(std::iter::once(remainder)));
                    MutableBitmap::from_vec(vec, data.length)
                } else {
                    MutableBitmap::from_vec(data.bytes.as_ref().to_vec(), data.length)
                }
            },
            Either::Right(data) => data,
        }
    }

    /// Initializes an new [`Bitmap`] filled with unset values.
    #[inline]
    pub fn new_zeroed(length: usize) -> Self {
        Self::new_with_value(false, length)
    }

    /// Initializes an new [`Bitmap`] filled with the given value.
    #[inline]
    pub fn new_with_value(value: bool, length: usize) -> Self {
        // Don't use `MutableBitmap::from_len_zeroed().into()`, it triggers a bitcount.
        let bytes = if value {
            vec![u8::MAX; length.saturating_add(7) / 8]
        } else {
            vec![0; length.saturating_add(7) / 8]
        };
        let unset_bits = if value { 0 } else { length };
        unsafe { Bitmap::from_inner_unchecked(Arc::new(bytes.into()), 0, length, Some(unset_bits)) }
    }

    /// Counts the nulls (unset bits) starting from `offset` bits and for `length` bits.
    #[inline]
    pub fn null_count_range(&self, offset: usize, length: usize) -> usize {
        count_zeros(&self.bytes, self.offset + offset, length)
    }

    /// Creates a new [`Bitmap`] from a slice and length.
    /// # Panic
    /// Panics iff `length <= bytes.len() * 8`
    #[inline]
    pub fn from_u8_slice<T: AsRef<[u8]>>(slice: T, length: usize) -> Self {
        Bitmap::try_new(slice.as_ref().to_vec(), length).unwrap()
    }

    /// Alias for `Bitmap::try_new().unwrap()`
    /// This function is `O(1)`
    /// # Panic
    /// This function panics iff `length > bytes.len() * 8`
    #[inline]
    pub fn from_u8_vec(vec: Vec<u8>, length: usize) -> Self {
        Bitmap::try_new(vec, length).unwrap()
    }

    /// Returns whether the bit at position `i` is set.
    #[inline]
    pub fn get(&self, i: usize) -> Option<bool> {
        if i < self.len() {
            Some(unsafe { self.get_bit_unchecked(i) })
        } else {
            None
        }
    }

    /// Creates a `[Bitmap]` from its internal representation.
    /// This is the inverted from `[Bitmap::into_inner]`
    ///
    /// # Safety
    /// Callers must ensure all invariants of this struct are upheld.
    pub unsafe fn from_inner_unchecked(
        bytes: Arc<Bytes<u8>>,
        offset: usize,
        length: usize,
        unset_bits: Option<usize>,
    ) -> Self {
        debug_assert!(check(&bytes[..], offset, length).is_ok());

        let unset_bit_count_cache = if let Some(n) = unset_bits {
            AtomicU64::new(n as u64)
        } else {
            AtomicU64::new(UNKNOWN_BIT_COUNT)
        };
        Self {
            bytes,
            offset,
            length,
            unset_bit_count_cache,
        }
    }
}

impl<P: AsRef<[bool]>> From<P> for Bitmap {
    fn from(slice: P) -> Self {
        Self::from_trusted_len_iter(slice.as_ref().iter().copied())
    }
}

impl FromIterator<bool> for Bitmap {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = bool>,
    {
        MutableBitmap::from_iter(iter).into()
    }
}

impl Bitmap {
    /// Creates a new [`Bitmap`] from an iterator of booleans.
    ///
    /// # Safety
    /// The iterator must report an accurate length.
    #[inline]
    pub unsafe fn from_trusted_len_iter_unchecked<I: Iterator<Item = bool>>(iterator: I) -> Self {
        MutableBitmap::from_trusted_len_iter_unchecked(iterator).into()
    }

    /// Creates a new [`Bitmap`] from an iterator of booleans.
    #[inline]
    pub fn from_trusted_len_iter<I: TrustedLen<Item = bool>>(iterator: I) -> Self {
        MutableBitmap::from_trusted_len_iter(iterator).into()
    }

    /// Creates a new [`Bitmap`] from a fallible iterator of booleans.
    #[inline]
    pub fn try_from_trusted_len_iter<E, I: TrustedLen<Item = std::result::Result<bool, E>>>(
        iterator: I,
    ) -> std::result::Result<Self, E> {
        Ok(MutableBitmap::try_from_trusted_len_iter(iterator)?.into())
    }

    /// Creates a new [`Bitmap`] from a fallible iterator of booleans.
    ///
    /// # Safety
    /// The iterator must report an accurate length.
    #[inline]
    pub unsafe fn try_from_trusted_len_iter_unchecked<
        E,
        I: Iterator<Item = std::result::Result<bool, E>>,
    >(
        iterator: I,
    ) -> std::result::Result<Self, E> {
        Ok(MutableBitmap::try_from_trusted_len_iter_unchecked(iterator)?.into())
    }

    /// Create a new [`Bitmap`] from an arrow [`NullBuffer`]
    ///
    /// [`NullBuffer`]: arrow_buffer::buffer::NullBuffer
    #[cfg(feature = "arrow_rs")]
    pub fn from_null_buffer(value: arrow_buffer::buffer::NullBuffer) -> Self {
        let offset = value.offset();
        let length = value.len();
        let unset_bits = value.null_count();
        Self {
            offset,
            length,
            unset_bit_count_cache: AtomicU64::new(unset_bits as u64),
            bytes: Arc::new(crate::buffer::to_bytes(value.buffer().clone())),
        }
    }
}

impl<'a> IntoIterator for &'a Bitmap {
    type Item = bool;
    type IntoIter = BitmapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        BitmapIter::<'a>::new(&self.bytes, self.offset, self.length)
    }
}

impl IntoIterator for Bitmap {
    type Item = bool;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}

#[cfg(feature = "arrow_rs")]
impl From<Bitmap> for arrow_buffer::buffer::NullBuffer {
    fn from(value: Bitmap) -> Self {
        let null_count = value.unset_bits();
        let buffer = crate::buffer::to_buffer(value.bytes);
        let buffer = arrow_buffer::buffer::BooleanBuffer::new(buffer, value.offset, value.length);
        // SAFETY: null count is accurate
        unsafe { arrow_buffer::buffer::NullBuffer::new_unchecked(buffer, null_count) }
    }
}
