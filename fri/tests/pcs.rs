use p3_baby_bear::{BabyBear, DiffusionMatrixBabybear};
use p3_challenger::{CanObserve, DuplexChallenger, FieldChallenger};
use p3_commit::{ExtensionMmcs, Pcs};
use p3_dft::Radix2DitParallel;
use p3_field::extension::BinomialExtensionField;
use p3_field::Field;
use p3_fri::{FriConfig, TwoAdicFriPcs};
use p3_matrix::dense::RowMajorMatrix;
use p3_merkle_tree::FieldMerkleTreeMmcs;
use p3_poseidon2::{Poseidon2, HLMDSMat4, Poseidon2ExternalMatrix};
use p3_symmetric::{PaddingFreeSponge, TruncatedPermutation};
use rand::thread_rng;

fn make_test_fri_pcs(log_degrees: &[usize]) {
    let mut rng = thread_rng();
    type Val = BabyBear;
    type Challenge = BinomialExtensionField<Val, 4>;

    let external_linear_layer: Poseidon2ExternalMatrix<_> = Poseidon2ExternalMatrix::new(HLMDSMat4);
    type Perm = Poseidon2<Val, Poseidon2ExternalMatrix<HLMDSMat4>, DiffusionMatrixBabybear, 16, 7>;
    let perm = Perm::new_from_rng(8, external_linear_layer, 22, DiffusionMatrixBabybear, &mut thread_rng());

    type MyHash = PaddingFreeSponge<Perm, 16, 8, 8>;
    let hash = MyHash::new(perm.clone());

    type MyCompress = TruncatedPermutation<Perm, 2, 8, 16>;
    let compress = MyCompress::new(perm.clone());

    type ValMmcs = FieldMerkleTreeMmcs<
        <Val as Field>::Packing,
        <Val as Field>::Packing,
        MyHash,
        MyCompress,
        8,
    >;
    let val_mmcs = ValMmcs::new(hash, compress);

    type ChallengeMmcs = ExtensionMmcs<Val, Challenge, ValMmcs>;
    let challenge_mmcs = ChallengeMmcs::new(val_mmcs.clone());

    type Dft = Radix2DitParallel;
    let dft = Dft {};

    type Challenger = DuplexChallenger<Val, Perm, 16>;

    let fri_config = FriConfig {
        log_blowup: 1,
        num_queries: 10,
        proof_of_work_bits: 8,
        mmcs: challenge_mmcs,
    };
    type MyPcs = TwoAdicFriPcs<Val, Dft, ValMmcs, ChallengeMmcs>;
    let max_log_n = log_degrees.iter().copied().max().unwrap();
    let pcs: MyPcs = MyPcs::new(max_log_n, dft, val_mmcs, fri_config);

    let mut challenger = Challenger::new(perm.clone());

    let domains_and_polys = log_degrees
        .iter()
        .map(|&d| {
            (
                <MyPcs as Pcs<Challenge, Challenger>>::natural_domain_for_degree(&pcs, 1 << d),
                RowMajorMatrix::<Val>::rand(&mut rng, 1 << d, 10),
            )
        })
        .collect::<Vec<_>>();

    let (commit, data) =
        <MyPcs as Pcs<Challenge, Challenger>>::commit(&pcs, domains_and_polys.clone());

    challenger.observe(commit);

    let zeta = challenger.sample_ext_element::<Challenge>();

    let points = domains_and_polys
        .iter()
        .map(|_| vec![zeta])
        .collect::<Vec<_>>();

    let (opening, proof) = pcs.open(vec![(&data, points)], &mut challenger);

    // verify the proof.
    let mut challenger = Challenger::new(perm);
    challenger.observe(commit);
    let _ = challenger.sample_ext_element::<Challenge>();

    let os = domains_and_polys
        .iter()
        .zip(&opening[0])
        .map(|((domain, _), mat_openings)| (*domain, vec![(zeta, mat_openings[0].clone())]))
        .collect();
    pcs.verify(vec![(commit, os)], &proof, &mut challenger)
        .unwrap()
}

#[test]
fn test_fri_pcs_single() {
    make_test_fri_pcs(&[3]);
}

#[test]
fn test_fri_pcs_many_equal() {
    for i in 1..4 {
        make_test_fri_pcs(&[i; 5]);
    }
}

#[test]
fn test_fri_pcs_many_different() {
    for i in 2..4 {
        let degrees = (3..3 + i).collect::<Vec<_>>();
        make_test_fri_pcs(&degrees);
    }
}

#[test]
fn test_fri_pcs_many_different_rev() {
    for i in 2..4 {
        let degrees = (3..3 + i).rev().collect::<Vec<_>>();
        make_test_fri_pcs(&degrees);
    }
}
