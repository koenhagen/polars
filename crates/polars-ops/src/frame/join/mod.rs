mod args;
#[cfg(feature = "asof_join")]
mod asof;
#[cfg(feature = "dtype-categorical")]
mod checks;
mod cross_join;
mod general;
mod hash_join;
#[cfg(feature = "merge_sorted")]
mod merge_sorted;

use std::borrow::Cow;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;

use ahash::RandomState;
pub use args::*;
use arrow::trusted_len::TrustedLen;
#[cfg(feature = "asof_join")]
pub use asof::{AsOfOptions, AsofJoin, AsofJoinBy, AsofStrategy};
#[cfg(feature = "dtype-categorical")]
pub(crate) use checks::*;
pub use cross_join::CrossJoin;
#[cfg(feature = "chunked_ids")]
use either::Either;
#[cfg(feature = "chunked_ids")]
use general::create_chunked_index_mapping;
pub use general::{_finish_join, _join_suffix_name};
pub use hash_join::*;
use hashbrown::hash_map::{Entry, RawEntryMut};
#[cfg(feature = "merge_sorted")]
pub use merge_sorted::_merge_sorted_dfs;
use polars_core::hashing::{_df_rows_to_hashes_threaded_vertical, _HASHMAP_INIT_SIZE};
use polars_core::prelude::*;
pub(super) use polars_core::series::IsSorted;
#[allow(unused_imports)]
use polars_core::utils::slice_slice;
use polars_core::utils::{_to_physical_and_bit_repr, slice_offsets};
use polars_core::POOL;
use polars_utils::hashing::BytesHash;
use rayon::prelude::*;

use super::IntoDf;
use crate::frame::join::general::coalesce_outer_join;

pub trait DataFrameJoinOps: IntoDf {
    /// Generic join method. Can be used to join on multiple columns.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polars_core::prelude::*;
    /// # use polars_ops::prelude::*;
    /// let df1: DataFrame = df!("Fruit" => &["Apple", "Banana", "Pear"],
    ///                          "Phosphorus (mg/100g)" => &[11, 22, 12])?;
    /// let df2: DataFrame = df!("Name" => &["Apple", "Banana", "Pear"],
    ///                          "Potassium (mg/100g)" => &[107, 358, 115])?;
    ///
    /// let df3: DataFrame = df1.join(&df2, ["Fruit"], ["Name"], JoinArgs::new(JoinType::Inner))?;
    /// assert_eq!(df3.shape(), (3, 3));
    /// println!("{}", df3);
    /// # Ok::<(), PolarsError>(())
    /// ```
    ///
    /// Output:
    ///
    /// ```text
    /// shape: (3, 3)
    /// +--------+----------------------+---------------------+
    /// | Fruit  | Phosphorus (mg/100g) | Potassium (mg/100g) |
    /// | ---    | ---                  | ---                 |
    /// | str    | i32                  | i32                 |
    /// +========+======================+=====================+
    /// | Apple  | 11                   | 107                 |
    /// +--------+----------------------+---------------------+
    /// | Banana | 22                   | 358                 |
    /// +--------+----------------------+---------------------+
    /// | Pear   | 12                   | 115                 |
    /// +--------+----------------------+---------------------+
    /// ```
    fn join<I, S>(
        &self,
        other: &DataFrame,
        left_on: I,
        right_on: I,
        args: JoinArgs,
    ) -> PolarsResult<DataFrame>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let df_left = self.to_df();
        #[cfg(feature = "cross_join")]
        if let JoinType::Cross = args.how {
            return df_left.cross_join(other, args.suffix.as_deref(), None);
        }
        let selected_left = df_left.select_series(left_on)?;
        let selected_right = other.select_series(right_on)?;
        self._join_impl(other, selected_left, selected_right, args, true, false)
    }

    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    #[allow(unused_mut)]
    fn _join_impl(
        &self,
        other: &DataFrame,
        mut selected_left: Vec<Series>,
        mut selected_right: Vec<Series>,
        args: JoinArgs,
        _check_rechunk: bool,
        _verbose: bool,
    ) -> PolarsResult<DataFrame> {
        let left_df = self.to_df();
        args.validation
            .is_valid_join(&args.how, selected_left.len())?;

        #[cfg(feature = "cross_join")]
        if let JoinType::Cross = args.how {
            return left_df.cross_join(other, args.suffix.as_deref(), args.slice);
        }

        #[cfg(feature = "chunked_ids")]
        {
            // a left join create chunked-ids
            // the others not yet.
            // TODO! change this to other join types once they support chunked-id joins
            if _check_rechunk
                && !(matches!(args.how, JoinType::Left)
                    || std::env::var("POLARS_NO_CHUNKED_JOIN").is_ok())
            {
                let mut left = Cow::Borrowed(left_df);
                let mut right = Cow::Borrowed(other);
                if left_df.should_rechunk() {
                    if _verbose {
                        eprintln!("{:?} join triggered a rechunk of the left DataFrame: {} columns are affected", args.how, left_df.width());
                    }

                    let mut tmp_left = left_df.clone();
                    tmp_left.as_single_chunk_par();
                    left = Cow::Owned(tmp_left);
                }
                if other.should_rechunk() {
                    if _verbose {
                        eprintln!("{:?} join triggered a rechunk of the right DataFrame: {} columns are affected", args.how, other.width());
                    }
                    let mut tmp_right = other.clone();
                    tmp_right.as_single_chunk_par();
                    right = Cow::Owned(tmp_right);
                }
                return left._join_impl(
                    &right,
                    selected_left,
                    selected_right,
                    args,
                    false,
                    _verbose,
                );
            }
        }

        polars_ensure!(
            selected_left.len() == selected_right.len(),
            ComputeError:
                format!(
                    "the number of columns given as join key (left: {}, right:{}) should be equal",
                    selected_left.len(),
                    selected_right.len()
                )
        );

        if let Some((l, r)) = selected_left
            .iter()
            .zip(&selected_right)
            .find(|(l, r)| l.dtype() != r.dtype())
        {
            polars_bail!(
                ComputeError:
                    format!(
                        "datatypes of join keys don't match - `{}`: {} on left does not match `{}`: {} on right",
                        l.name(), l.dtype(), r.name(), r.dtype()
                    )
            );
        };

        #[cfg(feature = "dtype-categorical")]
        for (l, r) in selected_left.iter_mut().zip(selected_right.iter_mut()) {
            match _check_categorical_src(l.dtype(), r.dtype()) {
                Ok(_) => {},
                Err(_) => {
                    let (ca_left, ca_right) =
                        make_categoricals_compatible(l.categorical()?, r.categorical()?)?;
                    *l = ca_left.into_series().with_name(l.name());
                    *r = ca_right.into_series().with_name(r.name());
                },
            }
        }

        // Single keys.
        if selected_left.len() == 1 {
            let s_left = &selected_left[0];
            let s_right = &selected_right[0];
            return match args.how {
                JoinType::Inner => {
                    left_df._inner_join_from_series(other, s_left, s_right, args, _verbose)
                },
                JoinType::Left => {
                    left_df._left_join_from_series(other, s_left, s_right, args, _verbose)
                },
                JoinType::Outer { .. } => {
                    left_df._outer_join_from_series(other, s_left, s_right, args)
                },
                #[cfg(feature = "semi_anti_join")]
                JoinType::Anti => {
                    left_df._semi_anti_join_from_series(s_left, s_right, args.slice, true)
                },
                #[cfg(feature = "semi_anti_join")]
                JoinType::Semi => {
                    left_df._semi_anti_join_from_series(s_left, s_right, args.slice, false)
                },
                #[cfg(feature = "asof_join")]
                JoinType::AsOf(options) => {
                    let left_on = selected_left[0].name();
                    let right_on = selected_right[0].name();

                    match (options.left_by, options.right_by) {
                        (Some(left_by), Some(right_by)) => left_df._join_asof_by(
                            other,
                            left_on,
                            right_on,
                            left_by,
                            right_by,
                            options.strategy,
                            options.tolerance,
                            args.suffix.as_deref(),
                            args.slice,
                        ),
                        (None, None) => left_df._join_asof(
                            other,
                            left_on,
                            right_on,
                            options.strategy,
                            options.tolerance,
                            args.suffix,
                            args.slice,
                        ),
                        _ => {
                            panic!("expected by arguments on both sides")
                        },
                    }
                },
                JoinType::Cross => {
                    unreachable!()
                },
            };
        }

        fn remove_selected(df: &DataFrame, selected: &[Series]) -> DataFrame {
            let mut new = None;
            for s in selected {
                new = match new {
                    None => Some(df.drop(s.name()).unwrap()),
                    Some(new) => Some(new.drop(s.name()).unwrap()),
                }
            }
            new.unwrap()
        }

        // Make sure that we don't have logical types.
        // We don't overwrite the original selected as that might be used to create a column in the new df.
        let selected_left_physical = _to_physical_and_bit_repr(&selected_left);
        let selected_right_physical = _to_physical_and_bit_repr(&selected_right);

        // Multiple keys.
        match args.how {
            JoinType::Inner => {
                let left = unsafe { DataFrame::new_no_checks(selected_left_physical) };
                let right = unsafe { DataFrame::new_no_checks(selected_right_physical) };
                let (mut left, mut right, swap) = det_hash_prone_order!(left, right);
                let (join_idx_left, join_idx_right) =
                    _inner_join_multiple_keys(&mut left, &mut right, swap, args.join_nulls);
                let mut join_idx_left = &*join_idx_left;
                let mut join_idx_right = &*join_idx_right;

                if let Some((offset, len)) = args.slice {
                    join_idx_left = slice_slice(join_idx_left, offset, len);
                    join_idx_right = slice_slice(join_idx_right, offset, len);
                }

                let (df_left, df_right) = POOL.join(
                    // SAFETY: join indices are known to be in bounds
                    || unsafe { left_df._create_left_df_from_slice(join_idx_left, false, !swap) },
                    || unsafe {
                        // remove join columns
                        remove_selected(other, &selected_right)
                            ._take_unchecked_slice(join_idx_right, true)
                    },
                );
                _finish_join(df_left, df_right, args.suffix.as_deref())
            },
            JoinType::Left => {
                let mut left = unsafe { DataFrame::new_no_checks(selected_left_physical) };
                let mut right = unsafe { DataFrame::new_no_checks(selected_right_physical) };

                if let Some((offset, len)) = args.slice {
                    left = left.slice(offset, len);
                }
                let ids =
                    _left_join_multiple_keys(&mut left, &mut right, None, None, args.join_nulls);
                left_df._finish_left_join(ids, &remove_selected(other, &selected_right), args)
            },
            JoinType::Outer { .. } => {
                let df_left = unsafe { DataFrame::new_no_checks(selected_left_physical) };
                let df_right = unsafe { DataFrame::new_no_checks(selected_right_physical) };

                let (mut left, mut right, swap) = det_hash_prone_order!(df_left, df_right);
                let (mut join_idx_l, mut join_idx_r) =
                    _outer_join_multiple_keys(&mut left, &mut right, swap, args.join_nulls);

                if let Some((offset, len)) = args.slice {
                    let (offset, len) = slice_offsets(offset, len, join_idx_l.len());
                    join_idx_l.slice(offset, len);
                    join_idx_r.slice(offset, len);
                }
                let idx_ca_l = IdxCa::with_chunk("", join_idx_l);
                let idx_ca_r = IdxCa::with_chunk("", join_idx_r);

                // Take the left and right dataframes by join tuples
                let (df_left, df_right) = POOL.join(
                    || unsafe { left_df.take_unchecked(&idx_ca_l) },
                    || unsafe { other.take_unchecked(&idx_ca_r) },
                );

                let JoinType::Outer { coalesce } = args.how else {
                    unreachable!()
                };
                let names_left = selected_left.iter().map(|s| s.name()).collect::<Vec<_>>();
                let names_right = selected_right.iter().map(|s| s.name()).collect::<Vec<_>>();
                let out = _finish_join(df_left, df_right, args.suffix.as_deref());
                if coalesce {
                    Ok(coalesce_outer_join(
                        out?,
                        &names_left,
                        &names_right,
                        args.suffix.as_deref(),
                        left_df,
                    ))
                } else {
                    out
                }
            },
            #[cfg(feature = "asof_join")]
            JoinType::AsOf(_) => polars_bail!(
                ComputeError: "asof join not supported for join on multiple keys"
            ),
            #[cfg(feature = "semi_anti_join")]
            JoinType::Anti | JoinType::Semi => {
                let mut left = unsafe { DataFrame::new_no_checks(selected_left_physical) };
                let mut right = unsafe { DataFrame::new_no_checks(selected_right_physical) };

                let idx = if matches!(args.how, JoinType::Anti) {
                    _left_anti_multiple_keys(&mut left, &mut right, args.join_nulls)
                } else {
                    _left_semi_multiple_keys(&mut left, &mut right, args.join_nulls)
                };
                // SAFETY:
                // indices are in bounds
                Ok(unsafe { left_df._finish_anti_semi_join(&idx, args.slice) })
            },
            JoinType::Cross => {
                unreachable!()
            },
        }
    }

    /// Perform an inner join on two DataFrames.
    ///
    /// # Example
    ///
    /// ```
    /// # use polars_core::prelude::*;
    /// # use polars_ops::prelude::*;
    /// fn join_dfs(left: &DataFrame, right: &DataFrame) -> PolarsResult<DataFrame> {
    ///     left.inner_join(right, ["join_column_left"], ["join_column_right"])
    /// }
    /// ```
    fn inner_join<I, S>(
        &self,
        other: &DataFrame,
        left_on: I,
        right_on: I,
    ) -> PolarsResult<DataFrame>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.join(other, left_on, right_on, JoinArgs::new(JoinType::Inner))
    }

    /// Perform a left join on two DataFrames
    /// # Example
    ///
    /// ```no_run
    /// # use polars_core::prelude::*;
    /// # use polars_ops::prelude::*;
    /// let df1: DataFrame = df!("Wavelength (nm)" => &[480.0, 650.0, 577.0, 1201.0, 100.0])?;
    /// let df2: DataFrame = df!("Color" => &["Blue", "Yellow", "Red"],
    ///                          "Wavelength nm" => &[480.0, 577.0, 650.0])?;
    ///
    /// let df3: DataFrame = df1.left_join(&df2, ["Wavelength (nm)"], ["Wavelength nm"])?;
    /// println!("{:?}", df3);
    /// # Ok::<(), PolarsError>(())
    /// ```
    ///
    /// Output:
    ///
    /// ```text
    /// shape: (5, 2)
    /// +-----------------+--------+
    /// | Wavelength (nm) | Color  |
    /// | ---             | ---    |
    /// | f64             | str    |
    /// +=================+========+
    /// | 480             | Blue   |
    /// +-----------------+--------+
    /// | 650             | Red    |
    /// +-----------------+--------+
    /// | 577             | Yellow |
    /// +-----------------+--------+
    /// | 1201            | null   |
    /// +-----------------+--------+
    /// | 100             | null   |
    /// +-----------------+--------+
    /// ```
    fn left_join<I, S>(&self, other: &DataFrame, left_on: I, right_on: I) -> PolarsResult<DataFrame>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.join(other, left_on, right_on, JoinArgs::new(JoinType::Left))
    }

    /// Perform an outer join on two DataFrames
    /// # Example
    ///
    /// ```
    /// # use polars_core::prelude::*;
    /// # use polars_ops::prelude::*;
    /// fn join_dfs(left: &DataFrame, right: &DataFrame) -> PolarsResult<DataFrame> {
    ///     left.outer_join(right, ["join_column_left"], ["join_column_right"])
    /// }
    /// ```
    fn outer_join<I, S>(
        &self,
        other: &DataFrame,
        left_on: I,
        right_on: I,
    ) -> PolarsResult<DataFrame>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.join(
            other,
            left_on,
            right_on,
            JoinArgs::new(JoinType::Outer { coalesce: false }),
        )
    }
}

trait DataFrameJoinOpsPrivate: IntoDf {
    // hack for a macro
    fn len(&self) -> usize {
        self.to_df().height()
    }

    fn _inner_join_from_series(
        &self,
        other: &DataFrame,
        s_left: &Series,
        s_right: &Series,
        args: JoinArgs,
        verbose: bool,
    ) -> PolarsResult<DataFrame> {
        let left_df = self.to_df();
        #[cfg(feature = "dtype-categorical")]
        _check_categorical_src(s_left.dtype(), s_right.dtype())?;
        let ((join_tuples_left, join_tuples_right), sorted) =
            _sort_or_hash_inner(s_left, s_right, verbose, args.validation, args.join_nulls)?;

        let mut join_tuples_left = &*join_tuples_left;
        let mut join_tuples_right = &*join_tuples_right;

        if let Some((offset, len)) = args.slice {
            join_tuples_left = slice_slice(join_tuples_left, offset, len);
            join_tuples_right = slice_slice(join_tuples_right, offset, len);
        }

        let (df_left, df_right) = POOL.join(
            // SAFETY: join indices are known to be in bounds
            || unsafe { left_df._create_left_df_from_slice(join_tuples_left, false, sorted) },
            || unsafe {
                other
                    .drop(s_right.name())
                    .unwrap()
                    ._take_unchecked_slice(join_tuples_right, true)
            },
        );
        _finish_join(df_left, df_right, args.suffix.as_deref())
    }
}

impl DataFrameJoinOps for DataFrame {}
impl DataFrameJoinOpsPrivate for DataFrame {}
