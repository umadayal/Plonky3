use alloc::collections::BTreeMap;
use alloc::slice;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::{transmute, MaybeUninit};

use itertools::{izip, Itertools};
use p3_field::{Field, Powers, TwoAdicField};
use p3_matrix::bitrev::{BitReversableMatrix, BitReversalPerm, BitReversedMatrixView};
use p3_matrix::dense::{RowMajorMatrix, RowMajorMatrixViewMut};
use p3_matrix::util::reverse_matrix_index_bits;
use p3_matrix::Matrix;
use p3_maybe_rayon::prelude::*;
use p3_util::{log2_strict_usize, reverse_bits_len, reverse_slice_index_bits};
use tracing::{debug_span, info_span, instrument};

use crate::butterflies::{Butterfly, DitButterfly};
use crate::{divide_by_height, TwoAdicSubgroupDft};

/// A parallel FFT algorithm which divides a butterfly network's layers into two halves.
///
/// For the first half, we apply a butterfly network with smaller blocks in earlier layers,
/// i.e. either DIT or Bowers G. Then we bit-reverse, and for the second half, we continue executing
/// the same network but in bit-reversed order. This way we're always working with small blocks,
/// so within each half, we can have a certain amount of parallelism with no cross-thread
/// communication.
#[derive(Default, Clone, Debug)]
pub struct Radix2DitParallel<F> {
    /// Twiddles based on roots of unity, used in the forward DFT.
    twiddles: RefCell<BTreeMap<usize, VectorPair<F>>>,

    /// A map from `(log_h, shift)` to forward DFT twiddles with that coset shift baked in.
    #[allow(clippy::type_complexity)]
    coset_twiddles: RefCell<BTreeMap<(usize, F), Vec<Vec<F>>>>,

    /// Twiddles based on inverse roots of unity, used in the inverse DFT.
    inverse_twiddles: RefCell<BTreeMap<usize, VectorPair<F>>>,
}

/// A pair of vectors, one with twiddle factors in their natural order, the other bit-reversed.
#[derive(Default, Clone, Debug)]
struct VectorPair<F> {
    twiddles: Vec<F>,
    bit_reversed_twiddles: Vec<F>,
}

#[instrument(level = "debug", skip_all)]
fn compute_twiddles<F: TwoAdicField + Ord>(log_h: usize) -> VectorPair<F> {
    let half_h = (1 << log_h) >> 1;
    let root = F::two_adic_generator(log_h);
    let twiddles: Vec<F> = root.powers().take(half_h).collect();
    let mut bit_reversed_twiddles = twiddles.clone();
    reverse_slice_index_bits(&mut bit_reversed_twiddles);
    VectorPair {
        twiddles,
        bit_reversed_twiddles,
    }
}

#[instrument(level = "debug", skip_all)]
fn compute_coset_twiddles<F: TwoAdicField + Ord>(log_h: usize, shift: F) -> Vec<Vec<F>> {
    let mid = log_h / 2;
    let h = 1 << log_h;
    let root = F::two_adic_generator(log_h);

    (0..log_h)
        .map(|layer| {
            let shift_power = shift.exp_power_of_2(layer);
            let powers = Powers {
                base: root.exp_power_of_2(layer),
                current: shift_power,
            };
            let mut twiddles: Vec<_> = powers.take(h >> (layer + 1)).collect();
            let layer_rev = log_h - 1 - layer;
            if layer_rev >= mid {
                reverse_slice_index_bits(&mut twiddles);
            }
            twiddles
        })
        .collect()
}

#[instrument(level = "debug", skip_all)]
fn compute_inverse_twiddles<F: TwoAdicField + Ord>(log_h: usize) -> VectorPair<F> {
    let half_h = (1 << log_h) >> 1;
    let root_inv = F::two_adic_generator(log_h).inverse();
    let twiddles: Vec<F> = root_inv.powers().take(half_h).collect();
    let mut bit_reversed_twiddles = twiddles.clone();

    // In the middle of the coset LDE, we're in bit-reversed order.
    reverse_slice_index_bits(&mut bit_reversed_twiddles);

    VectorPair {
        twiddles,
        bit_reversed_twiddles,
    }
}

impl<F: TwoAdicField + Ord> TwoAdicSubgroupDft<F> for Radix2DitParallel<F> {
    type Evaluations = BitReversedMatrixView<RowMajorMatrix<F>>;

    fn dft_batch(&self, mut mat: RowMajorMatrix<F>) -> Self::Evaluations {
        let h = mat.height();
        let log_h = log2_strict_usize(h);

        // Compute twiddle factors, or take memoized ones if already available.
        let mut twiddles_ref_mut = self.twiddles.borrow_mut();
        let twiddles = twiddles_ref_mut
            .entry(log_h)
            .or_insert_with(|| compute_twiddles(log_h));

        let mid = log_h / 2;

        // The first half looks like a normal DIT.
        reverse_matrix_index_bits(&mut mat);
        par_dit_layer(&mut mat, mid, &twiddles.twiddles);

        // For the second half, we flip the DIT, working in bit-reversed order.
        reverse_matrix_index_bits(&mut mat);
        par_dit_layer_rev(&mut mat, mid, &twiddles.bit_reversed_twiddles);

        mat.bit_reverse_rows()
    }

    #[instrument(skip_all, fields(dims = %mat.dimensions(), added_bits = added_bits))]
    fn coset_lde_batch(
        &self,
        mut mat: RowMajorMatrix<F>,
        added_bits: usize,
        shift: F,
    ) -> Self::Evaluations {
        let w = mat.width;
        let h = mat.height();
        let log_h = log2_strict_usize(h);
        let mid = log_h / 2;

        let mut twiddles_ref_mut = self.inverse_twiddles.borrow_mut();
        let twiddles = twiddles_ref_mut
            .entry(log_h)
            .or_insert_with(|| compute_inverse_twiddles(log_h));

        // The first half looks like a normal DIT.
        reverse_matrix_index_bits(&mut mat);
        par_dit_layer(&mut mat, mid, &twiddles.twiddles);

        // For the second half, we flip the DIT, working in bit-reversed order.
        reverse_matrix_index_bits(&mut mat);
        par_dit_layer_rev(&mut mat, mid, &twiddles.bit_reversed_twiddles);
        // We skip the final bit-reversal, since the next FFT expects bit-reversed input.

        divide_by_height(&mut mat);

        let lde_elems = w * (h << added_bits);
        let elems_to_add = lde_elems - w * h;
        debug_span!("reserve_exact").in_scope(|| mat.values.reserve_exact(elems_to_add));

        let g_big = F::two_adic_generator(log_h + added_bits);

        let mat_ptr = mat.values.as_mut_ptr();
        let rest_ptr = unsafe { (mat_ptr as *mut MaybeUninit<F>).add(w * h) };
        let first_slice: &mut [F] = unsafe { slice::from_raw_parts_mut(mat_ptr, w * h) };
        let rest_slice: &mut [MaybeUninit<F>] =
            unsafe { slice::from_raw_parts_mut(rest_ptr, lde_elems - w * h) };
        assert!(mat.values.spare_capacity_mut().len() >= rest_slice.len());
        let mut first_coset_mat = RowMajorMatrixViewMut::new(first_slice, w);
        let mut rest_cosets_mat = rest_slice
            .chunks_exact_mut(w * h)
            .map(|slice| RowMajorMatrixViewMut::new(slice, w))
            .collect_vec();

        for coset_idx in 1..(1 << added_bits) {
            let total_shift = g_big.exp_u64(coset_idx as u64) * shift;
            let coset_idx = reverse_bits_len(coset_idx, added_bits);
            let dest = &mut rest_cosets_mat[coset_idx - 1]; // - 1 because we removed the first matrix.

            // // TODO: naive
            // for r in 0..h {
            //     for c in 0..w {
            //         dest.values[r * w + c].write(first_coset_mat.values[r * w + c]);
            //     }
            // }
            let first_coset_mat_maybe = unsafe {
                transmute::<&RowMajorMatrixViewMut<F>, &RowMajorMatrixViewMut<MaybeUninit<F>>>(
                    &first_coset_mat,
                )
            };
            // TODO: Could skip it and have coset_dft read initial input from a separate buffer.
            dest.copy_from(first_coset_mat_maybe);
            let dest = unsafe {
                transmute::<&mut RowMajorMatrixViewMut<MaybeUninit<F>>, &mut RowMajorMatrixViewMut<F>>(
                    dest,
                )
            };

            coset_dft(self, dest, total_shift);
        }

        // Now run a forward DFT on the very first coset, this time in-place.
        coset_dft(self, &mut first_coset_mat.as_view_mut(), shift);

        unsafe {
            assert!(mat.values.capacity() >= lde_elems);
            mat.values.set_len(lde_elems);
        }
        BitReversalPerm::new_view(mat)
    }
}

fn coset_dft<F: TwoAdicField + Ord>(
    dft: &Radix2DitParallel<F>,
    mat: &mut RowMajorMatrixViewMut<F>,
    shift: F,
) {
    let log_h = log2_strict_usize(mat.height());
    let mid = log_h / 2;

    let mut twiddles_ref_mut = dft.coset_twiddles.borrow_mut();
    let twiddles = twiddles_ref_mut
        .entry((log_h, shift))
        .or_insert_with(|| compute_coset_twiddles(log_h, shift));

    // The first half looks like a normal DIT.
    // par_dit_layer(&mut mat, mid, &old_twiddles.twiddles);
    info_span!("modified par_dit_layer").in_scope(|| {
        mat.par_row_chunks_exact_mut(1 << mid)
            .for_each(|mut submat| {
                for layer in 0..mid {
                    let layer_rev = log_h - 1 - layer;
                    dit_layer(&mut submat, layer, twiddles[layer_rev].iter().copied());
                }
            });
    });

    // For the second half, we flip the DIT, working in bit-reversed order.
    reverse_matrix_index_bits(mat);

    // par_dit_layer_rev(&mut mat, mid, &old_twiddles.bit_reversed_twiddles);
    info_span!("modified par_dit_layer_rev").in_scope(|| {
        mat.par_row_chunks_exact_mut(1 << (log_h - mid))
            .enumerate()
            .for_each(|(thread, mut submat)| {
                for layer in mid..log_h {
                    let layer_rev = log_h - 1 - layer;
                    let first_block = thread << (layer - mid);
                    dit_layer_rev(
                        &mut submat,
                        log_h,
                        layer,
                        twiddles[layer_rev][first_block..].iter().copied(),
                    );
                }
            });
    });
}

/// This can be used as the first half of a parallelized butterfly network.
#[instrument(level = "debug", skip_all)]
fn par_dit_layer<F: Field>(mat: &mut RowMajorMatrix<F>, mid: usize, twiddles: &[F]) {
    let log_h = log2_strict_usize(mat.height());

    // max block size: 2^mid
    mat.par_row_chunks_exact_mut(1 << mid)
        .for_each(|mut submat| {
            for layer in 0..mid {
                let layer_rev = log_h - 1 - layer;
                let layer_pow = 1 << layer_rev;
                dit_layer(
                    &mut submat,
                    layer,
                    twiddles.iter().copied().step_by(layer_pow),
                );
            }
        });
}

/// This can be used as the second half of a parallelized butterfly network.
#[instrument(level = "debug", skip_all)]
fn par_dit_layer_rev<F: Field>(mat: &mut RowMajorMatrix<F>, mid: usize, twiddles_rev: &[F]) {
    let log_h = log2_strict_usize(mat.height());

    // max block size: 2^(log_h - mid)
    mat.par_row_chunks_exact_mut(1 << (log_h - mid))
        .enumerate()
        .for_each(|(thread, mut submat)| {
            for layer in mid..log_h {
                let first_block = thread << (layer - mid);
                dit_layer_rev(
                    &mut submat,
                    log_h,
                    layer,
                    twiddles_rev[first_block..].iter().copied(),
                );
            }
        });
}

/// One layer of a DIT butterfly network.
fn dit_layer<F: Field>(
    submat: &mut RowMajorMatrixViewMut<'_, F>,
    layer: usize,
    twiddles: impl Iterator<Item = F> + Clone,
) {
    let half_block_size = 1 << layer;
    let block_size = half_block_size * 2;
    let width = submat.width();
    debug_assert!(submat.height() >= block_size);

    for block in submat.values.chunks_mut(block_size * width) {
        let (lows, highs) = block.split_at_mut(half_block_size * width);

        for (lo, hi, twiddle) in izip!(
            lows.chunks_mut(width),
            highs.chunks_mut(width),
            twiddles.clone()
        ) {
            DitButterfly(twiddle).apply_to_rows(lo, hi);
        }
    }
}

/// Like `dit_layer`, except the matrix and twiddles are encoded in bit-reversed order.
/// This can also be viewed as a layer of the Bowers G^T network.
fn dit_layer_rev<F: Field>(
    submat: &mut RowMajorMatrixViewMut<'_, F>,
    log_h: usize,
    layer: usize,
    twiddles_rev: impl Iterator<Item = F>,
) {
    let layer_rev = log_h - 1 - layer;

    let half_block_size = 1 << layer_rev;
    let block_size = half_block_size * 2;
    let width = submat.width();
    debug_assert!(submat.height() >= block_size);

    for (block, twiddle) in submat
        .values
        .chunks_mut(block_size * width)
        .zip(twiddles_rev)
    {
        let (lo, hi) = block.split_at_mut(half_block_size * width);
        DitButterfly(twiddle).apply_to_rows(lo, hi)
    }
}
