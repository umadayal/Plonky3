use core::marker::PhantomData;

use alloc::vec;
use alloc::vec::Vec;
use itertools::{izip, Itertools};
use p3_challenger::{CanSample, FieldChallenger};
use p3_commit::{DirectMmcs, OpenedValues, Pcs, UnivariatePcs, UnivariatePcsWithLde};
use p3_dft::TwoAdicSubgroupDft;
use p3_field::{
    batch_multiplicative_inverse, cyclic_subgroup_coset_known_order, AbstractExtensionField,
    AbstractField, ExtensionField, Field, TwoAdicField,
};
use p3_interpolation::interpolate_coset;
use p3_matrix::{
    bitrev::{BitReversableMatrix, BitReversedMatrixView},
    dense::{RowMajorMatrix, RowMajorMatrixView},
    Dimensions, Matrix, MatrixRows,
};
use p3_util::{log2_strict_usize, reverse_slice_index_bits, VecExt};
use serde::{Deserialize, Serialize};
use tracing::{info_span, instrument};

use crate::{prover, verifier::VerificationErrorForFriConfig, FriConfig, FriProof};

pub struct TwoAdicFriPcs<FC, Val, Dft, M> {
    fri: FC,
    dft: Dft,
    mmcs: M,
    _phantom: PhantomData<Val>,
}

impl<FC, Val, Dft, M> TwoAdicFriPcs<FC, Val, Dft, M> {
    pub fn new(fri: FC, dft: Dft, mmcs: M) -> Self {
        Self {
            fri,
            dft,
            mmcs,
            _phantom: PhantomData,
        }
    }
    fn coset_shift(&self) -> Val
    where
        Val: TwoAdicField,
    {
        Val::generator()
    }
}

#[derive(Serialize, Deserialize)]
pub struct TwoAdicFriPcsProof<FC: FriConfig, Val, InputMmcsProof> {
    #[serde(bound = "")]
    pub(crate) fri_proof: FriProof<FC>,
    /// For each query, for each committed batch, query openings for that batch
    pub(crate) input_openings: Vec<Vec<InputOpening<Val, InputMmcsProof>>>,
}

#[derive(Serialize, Deserialize)]
pub struct InputOpening<Val, InputMmcsProof> {
    pub(crate) opened_values: Vec<Vec<Val>>,
    pub(crate) opening_proof: InputMmcsProof,
}

impl<FC, Val, Dft, M, In> Pcs<Val, In> for TwoAdicFriPcs<FC, Val, Dft, M>
where
    Val: TwoAdicField,
    FC: FriConfig,
    FC::Challenge: ExtensionField<Val>,
    FC::Challenger: FieldChallenger<Val>,
    Dft: TwoAdicSubgroupDft<Val>,
    M: 'static + for<'a> DirectMmcs<Val, Mat<'a> = RowMajorMatrixView<'a, Val>>,
    In: MatrixRows<Val>,
{
    type Commitment = M::Commitment;
    type ProverData = M::ProverData;
    type Proof = TwoAdicFriPcsProof<FC, Val, M::Proof>;
    type Error = VerificationErrorForFriConfig<FC>;

    fn commit_batches(&self, polynomials: Vec<In>) -> (Self::Commitment, Self::ProverData) {
        self.commit_shifted_batches(polynomials, Val::one())
    }
}

impl<FC, Val, Dft, M, In> UnivariatePcsWithLde<Val, FC::Challenge, In, FC::Challenger>
    for TwoAdicFriPcs<FC, Val, Dft, M>
where
    Val: TwoAdicField,
    FC: FriConfig,
    FC::Challenge: ExtensionField<Val>,
    FC::Challenger: FieldChallenger<Val>,
    Dft: TwoAdicSubgroupDft<Val>,
    M: 'static + for<'a> DirectMmcs<Val, Mat<'a> = RowMajorMatrixView<'a, Val>>,
    In: MatrixRows<Val>,
{
    type Lde<'a> = BitReversedMatrixView<M::Mat<'a>> where Self: 'a;

    fn coset_shift(&self) -> Val {
        self.coset_shift()
    }

    fn log_blowup(&self) -> usize {
        self.fri.log_blowup()
    }

    fn get_ldes<'a, 'b>(&'a self, prover_data: &'b Self::ProverData) -> Vec<Self::Lde<'b>>
    where
        'a: 'b,
    {
        // We committed to the bit-reversed LDE, so now we wrap it to return in natural order.
        self.mmcs
            .get_matrices(prover_data)
            .into_iter()
            .map(|m| BitReversedMatrixView::new(m))
            .collect()
    }

    fn commit_shifted_batches(
        &self,
        polynomials: Vec<In>,
        coset_shift: Val,
    ) -> (Self::Commitment, Self::ProverData) {
        let shift = self.coset_shift() / coset_shift;
        let ldes = info_span!("compute all coset LDEs").in_scope(|| {
            polynomials
                .into_iter()
                .map(|poly| {
                    let input = poly.to_row_major_matrix();
                    // Commit to the bit-reversed LDE.
                    self.dft
                        .coset_lde_batch(input, self.fri.log_blowup(), shift)
                        .bit_reverse_rows()
                        .to_row_major_matrix()
                })
                .collect()
        });
        self.mmcs.commit(ldes)
    }
}

impl<FC, Val, Dft, M, In> UnivariatePcs<Val, FC::Challenge, In, FC::Challenger>
    for TwoAdicFriPcs<FC, Val, Dft, M>
where
    Val: TwoAdicField,
    FC: FriConfig,
    FC::Challenge: ExtensionField<Val>,
    FC::Challenger: FieldChallenger<Val>,
    Dft: TwoAdicSubgroupDft<Val>,
    M: 'static + for<'a> DirectMmcs<Val, Mat<'a> = RowMajorMatrixView<'a, Val>>,
    In: MatrixRows<Val>,
{
    #[instrument(name = "open_multi_batches", skip_all)]
    fn open_multi_batches(
        &self,
        prover_data_and_points: &[(&Self::ProverData, &[Vec<FC::Challenge>])],
        challenger: &mut FC::Challenger,
    ) -> (OpenedValues<FC::Challenge>, Self::Proof) {
        // Batch combination challenge
        let alpha = <FC::Challenger as CanSample<FC::Challenge>>::sample(challenger);
        let mut cached_alpha_pows = vec![FC::Challenge::one()];

        let coset_shift = self.coset_shift();

        let mut all_opened_values: OpenedValues<FC::Challenge> = vec![];
        let mut reduced_openings: [_; 32] = core::array::from_fn(|_| None);
        let mut num_reduced = [0; 32];

        for (data, points) in prover_data_and_points {
            let mats = self.mmcs.get_matrices(data);
            let opened_values_for_round = all_opened_values.pushed_mut(vec![]);
            for (mat, points_for_mat) in izip!(mats, *points) {
                let log_height = log2_strict_usize(mat.height());
                let reduced_opening_for_log_height = reduced_openings[log_height]
                    .get_or_insert_with(|| vec![FC::Challenge::zero(); mat.height()]);
                debug_assert_eq!(reduced_opening_for_log_height.len(), mat.height());

                let mut subgroup = cyclic_subgroup_coset_known_order(
                    Val::two_adic_generator(log_height),
                    coset_shift,
                    mat.height(),
                )
                .collect_vec();
                reverse_slice_index_bits(&mut subgroup);

                let inv_denoms = info_span!("batch invert denominators").in_scope(|| {
                    let denoms = points_for_mat
                        .iter()
                        .flat_map(|&z| {
                            subgroup
                                .iter()
                                .map(move |&x| FC::Challenge::from_base(x) - z)
                        })
                        .collect_vec();
                    RowMajorMatrix::new(batch_multiplicative_inverse(&denoms), mat.height())
                });

                let opened_values_for_mat = opened_values_for_round.pushed_mut(vec![]);
                for (&point, inv_denoms) in izip!(points_for_mat, inv_denoms.rows()) {
                    // Use Barycentric interpolation to evaluate the matrix at the given point.
                    let values = info_span!("compute opened values with Lagrange interpolation")
                        .in_scope(|| {
                            let (low_coset, _) =
                                mat.split_rows(mat.height() >> self.fri.log_blowup());
                            interpolate_coset(
                                &BitReversedMatrixView::new(low_coset),
                                coset_shift,
                                point,
                            )
                        });

                    let alpha_pows = get_cached_powers(
                        alpha,
                        &mut cached_alpha_pows,
                        num_reduced[log_height],
                        mat.width(),
                    );

                    info_span!("reduce openings").in_scope(|| {
                        // for each row
                        for (row, reduced_opening, &inv_denom) in izip!(
                            mat.rows(),
                            reduced_opening_for_log_height.iter_mut(),
                            inv_denoms,
                        ) {
                            // for each column
                            for (&p_at_x, &p_at_point, &alpha_pow) in
                                izip!(row, &values, alpha_pows)
                            {
                                *reduced_opening += alpha_pow
                                    * /* p(X) - p(z) */ (FC::Challenge::from_base(p_at_x) - p_at_point)
                                    * /* 1/(X - z)  */ inv_denom;
                            }
                        }
                    });

                    num_reduced[log_height] += mat.width();
                    opened_values_for_mat.push(values);
                }
            }
        }

        let (fri_proof, query_indices) = prover::prove(&self.fri, &reduced_openings, challenger);

        let input_openings = query_indices
            .into_iter()
            .map(|index| {
                prover_data_and_points
                    .iter()
                    .map(|(data, _)| {
                        let (opened_values, opening_proof) = self.mmcs.open_batch(index, data);
                        InputOpening {
                            opened_values,
                            opening_proof,
                        }
                    })
                    .collect()
            })
            .collect();

        (
            all_opened_values,
            TwoAdicFriPcsProof {
                fri_proof,
                input_openings,
            },
        )
    }

    fn verify_multi_batches(
        &self,
        commits_and_points: &[(Self::Commitment, &[Vec<FC::Challenge>])],
        dims: &[Vec<Dimensions>],
        values: OpenedValues<FC::Challenge>,
        proof: &Self::Proof,
        challenger: &mut FC::Challenger,
    ) -> Result<(), Self::Error> {
        // todo!()
        Ok(())
    }
}

fn get_cached_powers<'a, F: Field>(
    power: F,
    cache: &'a mut Vec<F>,
    start: usize,
    count: usize,
) -> &'a [F] {
    while cache.len() < start + count {
        cache.push(*cache.last().unwrap() * power);
    }
    &cache[start..start + count]
}
