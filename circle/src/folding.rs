use core::marker::PhantomData;

use alloc::vec::Vec;
use itertools::Itertools;
use p3_field::{
    batch_multiplicative_inverse, extension::ComplexExtendable, AbstractField, ExtensionField,
};
use p3_fri::FriFolder;
use p3_matrix::{
    row_index_mapped::{RowIndexMap, RowIndexMappedView},
    Matrix,
};
use p3_util::{log2_strict_usize, reverse_bits_len};

use crate::domain::CircleDomain;

pub(crate) fn fold_bivariate<F: ComplexExtendable, EF: ExtensionField<F>>(
    evals: impl Matrix<EF>,
    beta: EF,
) -> Vec<EF> {
    assert_eq!(evals.width(), 2);
    let domain = CircleDomain::standard(log2_strict_usize(evals.height()) + 1);
    let mut twiddles = batch_multiplicative_inverse(
        &domain
            .points()
            .take(evals.height())
            .map(|p| p.imag())
            .collect_vec(),
    );
    twiddles = circle_bitrev_permute(&twiddles);
    fold(evals, beta, &twiddles)
}

pub(crate) struct CircleFriFolder<F, EF> {
    bivariate_beta: EF,
    _phantom: PhantomData<F>,
}

impl<F, EF> CircleFriFolder<F, EF> {
    pub(crate) fn new(bivariate_beta: EF) -> Self {
        Self {
            bivariate_beta,
            _phantom: PhantomData,
        }
    }
}

impl<F: ComplexExtendable, EF: ExtensionField<F>> FriFolder<EF> for CircleFriFolder<F, EF> {
    fn fold_row(index: usize, log_height: usize, beta: EF, evals: impl Iterator<Item = EF>) -> EF {
        let evals = evals.collect_vec();
        assert_eq!(evals.len(), 2);

        let shift = F::circle_two_adic_generator(log_height + 3);
        let g = F::circle_two_adic_generator(log_height + 2);
        let orig_idx = circle_bitrev_idx(index, log_height);
        let t = (shift * g.exp_u64(orig_idx as u64)).real().inverse();

        let sum = evals[0] + evals[1];
        let diff = (evals[0] - evals[1]) * t;
        (sum + beta * diff).halve()
    }
    fn fold_matrix<M: Matrix<EF>>(beta: EF, m: M) -> Vec<EF> {
        assert_eq!(m.width(), 2);
        let domain = CircleDomain::standard(log2_strict_usize(m.height()) + 2);
        let mut twiddles = batch_multiplicative_inverse(
            &domain
                .points()
                .take(m.height())
                .map(|p| p.real())
                .collect_vec(),
        );
        twiddles = circle_bitrev_permute(&twiddles);
        fold(m, beta, &twiddles)
    }
    // todo: remove from trait
    fn mix_in(&self, _index: usize, _log_height: usize, current: &mut EF, new: EF) {
        *current += new;
    }
    /*
    fn m(&self, current: &mut [EF], new: &[EF]) {
        let new = RowMajorMatrix::new(new.to_vec(), 2);
        let domain = CircleDomain::standard(log2_strict_usize(new.height()) + 1);
        let mut twiddles = batch_multiplicative_inverse(
            &domain
                .points()
                .take(new.height())
                .map(|p| p.imag())
                .collect_vec(),
        );
        twiddles = circle_bitrev_permute(&twiddles);
        let folded = fold(new, self.bivariate_beta, &twiddles);
    }
    */
}

fn fold<F: ComplexExtendable, EF: ExtensionField<F>>(
    evals: impl Matrix<EF>,
    beta: EF,
    twiddles: &[F],
) -> Vec<EF> {
    evals
        .rows()
        .zip(twiddles)
        .map(|(mut row, &t)| {
            let (lo, hi) = row.next_tuple().unwrap();
            let sum = lo + hi;
            let diff = (lo - hi) * t;
            (sum + beta * diff).halve()
        })
        .collect_vec()
}

// circlebitrev -> natural
// can make faster with:
// https://lemire.me/blog/2018/02/21/iterating-over-set-bits-quickly/
fn circle_bitrev_idx(mut idx: usize, bits: usize) -> usize {
    idx = reverse_bits_len(idx, bits);
    for i in 0..bits {
        if idx & (1 << i) != 0 {
            idx ^= (1 << i) - 1;
        }
    }
    idx
}

fn circle_bitrev_idx_inv(mut idx: usize, bits: usize) -> usize {
    for i in (0..bits).rev() {
        if idx & (1 << i) != 0 {
            idx ^= (1 << i) - 1;
        }
    }
    reverse_bits_len(idx, bits)
}

// can do in place if use cycles? bitrev makes it harder
pub(crate) fn circle_bitrev_permute<T: Clone>(xs: &[T]) -> Vec<T> {
    let bits = log2_strict_usize(xs.len());
    (0..xs.len())
        .map(|i| xs[circle_bitrev_idx(i, bits)].clone())
        .collect()
}

pub struct CircleBitrevPerm {
    log_height: usize,
}

pub type CircleBitrevView<M> = RowIndexMappedView<CircleBitrevPerm, M>;

impl CircleBitrevPerm {
    pub fn new<T: Send + Sync, M: Matrix<T>>(inner: M) -> RowIndexMappedView<CircleBitrevPerm, M> {
        RowIndexMappedView {
            index_map: CircleBitrevPerm {
                log_height: log2_strict_usize(inner.height()),
            },
            inner,
        }
    }
}

impl RowIndexMap for CircleBitrevPerm {
    fn height(&self) -> usize {
        1 << self.log_height
    }
    fn map_row_index(&self, r: usize) -> usize {
        circle_bitrev_idx(r, self.log_height)
    }
}

/*
pub(crate) struct CircleBitrevInvPermutation;
impl RowPermutation for CircleBitrevInvPermutation {
    fn permute_index(r: usize, height: usize) -> usize {
        let bits = log2_strict_usize(height);
        circle_bitrev_idx_inv(r, bits)
    }
}
*/

#[cfg(test)]
mod tests {
    use p3_field::{
        extension::BinomialExtensionField, AbstractExtensionField, AbstractField, Field,
    };
    use p3_matrix::{dense::RowMajorMatrix, Matrix};
    use p3_mersenne_31::Mersenne31;
    use rand::{thread_rng, Rng};

    use crate::Cfft;

    use super::*;

    #[test]
    fn test_circle_bitrev() {
        assert_eq!(circle_bitrev_permute(&[0]), &[0]);
        assert_eq!(circle_bitrev_permute(&[0, 1]), &[0, 1]);
        assert_eq!(circle_bitrev_permute(&[0, 1, 2, 3]), &[0, 3, 1, 2]);
        assert_eq!(
            circle_bitrev_permute(&[0, 1, 2, 3, 4, 5, 6, 7]),
            &[0, 7, 3, 4, 1, 6, 2, 5]
        );
    }

    fn do_test_folding(log_n: usize, log_blowup: usize) {
        // dbg!(log_n, log_blowup);

        let mut rng = thread_rng();

        type F = Mersenne31;
        type EF = BinomialExtensionField<F, 3>;

        let mut evals: Vec<EF> = {
            let evals = RowMajorMatrix::<F>::rand(
                &mut rng,
                1 << log_n,
                <EF as AbstractExtensionField<F>>::D,
            );
            let lde = Cfft::default().lde(
                evals,
                CircleDomain::standard(log_n),
                CircleDomain::standard(log_n + log_blowup),
            );
            lde.rows()
                .map(|r| EF::from_base_slice(&r.collect_vec()))
                .collect()
        };

        evals = circle_bitrev_permute(&evals);

        evals = fold_bivariate::<F, _>(RowMajorMatrix::new(evals, 2), rng.gen());
        for _ in log_blowup..(log_n + log_blowup - 1) {
            evals = CircleFriFolder::<F, EF>::fold_matrix(rng.gen(), RowMajorMatrix::new(evals, 2));
        }
        assert_eq!(evals.len(), 1 << log_blowup);
        assert_eq!(
            evals,
            core::iter::repeat(evals[0]).take(evals.len()).collect_vec()
        );
    }

    #[test]
    fn test_folding() {
        do_test_folding(4, 1);
        do_test_folding(5, 2);
    }
}
