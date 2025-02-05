use crate::hash_to_curve::htp_bls12381_g2;
use crate::SetupParams;

use ark_ec::{AffineCurve, PairingEngine};
use ark_ff::{Field, One, PrimeField, ToBytes, UniformRand, Zero};
use ark_poly::{
    univariate::DensePolynomial, EvaluationDomain, Polynomial, UVPolynomial,
};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use itertools::izip;

use subproductdomain::{fast_multiexp, SubproductDomain};

use rand_core::RngCore;
use std::usize;

use thiserror::Error;

mod ciphertext;
mod combine;
mod context;
mod decryption;
mod hash_to_curve;
mod key_share;
mod refresh;

pub use ciphertext::*;
pub use combine::*;
pub use context::*;
pub use decryption::*;
pub use key_share::*;
pub use refresh::*;

#[cfg(feature = "api")]
pub mod api;

#[cfg(feature = "serialization")]
pub mod serialization;

pub trait ThresholdEncryptionParameters {
    type E: PairingEngine;
}

#[derive(Debug, Error)]
pub enum ThresholdEncryptionError {
    /// Error
    /// Refers to the check 4.4.2 in the paper: https://eprint.iacr.org/2022/898.pdf
    #[error("ciphertext verification failed")]
    CiphertextVerificationFailed,

    /// Error
    /// Refers to the check 4.4.4 in the paper: https://eprint.iacr.org/2022/898.pdf
    #[error("Decryption share verification failed")]
    DecryptionShareVerificationFailed,

    /// Hashing to curve failed
    #[error("Could not hash to curve")]
    HashToCurveError,

    #[error("plaintext verification failed")]
    PlaintextVerificationFailed,
}

pub type Result<T> = std::result::Result<T, ThresholdEncryptionError>;

fn hash_to_g2<T: ark_serialize::CanonicalDeserialize>(message: &[u8]) -> T {
    let mut point_ser: Vec<u8> = Vec::new();
    let point = htp_bls12381_g2(message);
    point.serialize(&mut point_ser).unwrap();
    T::deserialize(&point_ser[..]).unwrap()
}

fn construct_tag_hash<E: PairingEngine>(
    u: E::G1Affine,
    stream_ciphertext: &[u8],
    aad: &[u8],
) -> E::G2Affine {
    let mut hash_input = Vec::<u8>::new();
    u.write(&mut hash_input).unwrap();
    hash_input.extend_from_slice(stream_ciphertext);
    hash_input.extend_from_slice(aad);

    hash_to_g2(&hash_input)
}

pub fn setup_fast<E: PairingEngine>(
    threshold: usize,
    shares_num: usize,
    rng: &mut impl RngCore,
) -> (
    E::G1Affine,
    E::G2Affine,
    Vec<PrivateDecryptionContextFast<E>>,
) {
    assert!(shares_num >= threshold);

    // Generators G∈G1, H∈G2
    let g = E::G1Affine::prime_subgroup_generator();
    let h = E::G2Affine::prime_subgroup_generator();

    // The dealer chooses a uniformly random polynomial f of degree t-1
    let threshold_poly = DensePolynomial::<E::Fr>::rand(threshold - 1, rng);
    // Domain, or omega Ω
    let fft_domain =
        ark_poly::Radix2EvaluationDomain::<E::Fr>::new(shares_num).unwrap();
    // `evals` are evaluations of the polynomial f over the domain, omega: f(ω_j) for ω_j in Ω
    let evals = threshold_poly.evaluate_over_domain_by_ref(fft_domain);

    // A - public key shares of participants
    let pubkey_shares = fast_multiexp(&evals.evals, g.into_projective());
    let pubkey_share = g.mul(evals.evals[0]);
    debug_assert!(pubkey_shares[0] == E::G1Affine::from(pubkey_share));

    // Y, but only when b = 1 - private key shares of participants
    let privkey_shares = fast_multiexp(&evals.evals, h.into_projective());

    // a_0
    let x = threshold_poly.coeffs[0];

    // F_0 - The commitment to the constant term, and is the public key output Y from PVDKG
    let pubkey = g.mul(x);
    let privkey = h.mul(x);

    let mut domain_points = Vec::with_capacity(shares_num);
    let mut point = E::Fr::one();
    let mut domain_points_inv = Vec::with_capacity(shares_num);
    let mut point_inv = E::Fr::one();

    for _ in 0..shares_num {
        domain_points.push(point); // 1, t, t^2, t^3, ...; where t is a scalar generator fft_domain.group_gen
        point *= fft_domain.group_gen;
        domain_points_inv.push(point_inv);
        point_inv *= fft_domain.group_gen_inv;
    }

    let mut private_contexts = vec![];
    let mut public_contexts = vec![];

    // (domain, domain_inv, A, Y)
    for (index, (domain, domain_inv, public, private)) in izip!(
        domain_points.iter(),
        domain_points_inv.iter(),
        pubkey_shares.iter(),
        privkey_shares.iter()
    )
    .enumerate()
    {
        let private_key_share = PrivateKeyShare::<E> {
            private_key_share: *private,
        };
        let b = E::Fr::rand(rng);
        let mut blinded_key_shares = private_key_share.blind(b);
        blinded_key_shares.multiply_by_omega_inv(domain_inv);
        private_contexts.push(PrivateDecryptionContextFast::<E> {
            index,
            setup_params: SetupParams {
                b,
                b_inv: b.inverse().unwrap(),
                g,
                h_inv: E::G2Prepared::from(-h),
                g_inv: E::G1Prepared::from(-g),
                h,
            },
            private_key_share,
            public_decryption_contexts: vec![],
        });
        public_contexts.push(PublicDecryptionContextFast::<E> {
            domain: *domain,
            public_key_share: PublicKeyShare::<E> {
                public_key_share: *public,
            },
            blinded_key_share: blinded_key_shares,
            lagrange_n_0: *domain,
            h_inv: E::G2Prepared::from(-h),
        });
    }
    for private in private_contexts.iter_mut() {
        private.public_decryption_contexts = public_contexts.clone();
    }

    (pubkey.into(), privkey.into(), private_contexts)
}

pub fn setup_simple<E: PairingEngine>(
    threshold: usize,
    shares_num: usize,
    rng: &mut impl RngCore,
) -> (
    E::G1Affine,
    E::G2Affine,
    Vec<PrivateDecryptionContextSimple<E>>,
) {
    assert!(shares_num >= threshold);

    let g = E::G1Affine::prime_subgroup_generator();
    let h = E::G2Affine::prime_subgroup_generator();

    // The dealer chooses a uniformly random polynomial f of degree t-1
    let threshold_poly = DensePolynomial::<E::Fr>::rand(threshold - 1, rng);
    // Domain, or omega Ω
    let fft_domain =
        ark_poly::Radix2EvaluationDomain::<E::Fr>::new(shares_num).unwrap();
    // `evals` are evaluations of the polynomial f over the domain, omega: f(ω_j) for ω_j in Ω
    let evals = threshold_poly.evaluate_over_domain_by_ref(fft_domain);

    let shares_x = fft_domain.elements().collect::<Vec<_>>();

    // A - public key shares of participants
    let pubkey_shares = fast_multiexp(&evals.evals, g.into_projective());
    let pubkey_share = g.mul(evals.evals[0]);
    debug_assert!(pubkey_shares[0] == E::G1Affine::from(pubkey_share));

    // Y, but only when b = 1 - private key shares of participants
    let privkey_shares = fast_multiexp(&evals.evals, h.into_projective());

    // a_0
    let x = threshold_poly.coeffs[0];
    // F_0
    let pubkey = g.mul(x);
    let privkey = h.mul(x);

    let secret = threshold_poly.evaluate(&E::Fr::zero());
    debug_assert!(secret == x);

    let mut private_contexts = vec![];
    let mut public_contexts = vec![];

    // (domain, A, Y)
    for (index, (domain, public, private)) in
        izip!(shares_x.iter(), pubkey_shares.iter(), privkey_shares.iter())
            .enumerate()
    {
        let private_key_share = PrivateKeyShare::<E> {
            private_key_share: *private,
        };
        let b = E::Fr::rand(rng);
        let blinded_key_share = private_key_share.blind(b);
        private_contexts.push(PrivateDecryptionContextSimple::<E> {
            index,
            setup_params: SetupParams {
                b,
                b_inv: b.inverse().unwrap(),
                g,
                h_inv: E::G2Prepared::from(-h),
                g_inv: E::G1Prepared::from(-g),
                h,
            },
            private_key_share,
            validator_private_key: b,
            public_decryption_contexts: vec![],
        });
        public_contexts.push(PublicDecryptionContextSimple::<E> {
            domain: *domain,
            public_key_share: PublicKeyShare::<E> {
                public_key_share: *public,
            },
            blinded_key_share,
            h,
            validator_public_key: h.mul(b),
        });
    }
    for private in private_contexts.iter_mut() {
        private.public_decryption_contexts = public_contexts.clone();
    }

    (pubkey.into(), privkey.into(), private_contexts)
}

#[cfg(test)]
mod tests {
    use crate::*;
    use ark_bls12_381::Fr;
    use ark_ec::ProjectiveCurve;
    use ark_ff::BigInteger256;
    use ark_std::test_rng;
    use itertools::Itertools;
    use rand::prelude::StdRng;
    use std::collections::HashMap;
    use std::ops::Mul;

    type E = ark_bls12_381::Bls12_381;
    type Fqk = <ark_bls12_381::Bls12_381 as PairingEngine>::Fqk;

    #[test]
    fn ciphertext_serialization() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, _) = setup_fast::<E>(threshold, shares_num, rng);

        let ciphertext = encrypt::<StdRng, E>(msg, aad, &pubkey, rng);

        let serialized = ciphertext.to_bytes();
        let deserialized: Ciphertext<E> = Ciphertext::from_bytes(&serialized);

        assert_eq!(serialized, deserialized.to_bytes())
    }

    #[test]
    fn symmetric_encryption() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, privkey, contexts) =
            setup_fast::<E>(threshold, shares_num, rng);
        let g_inv = &contexts[0].setup_params.g_inv;

        let ciphertext = encrypt::<StdRng, E>(msg, aad, &pubkey, rng);

        let plaintext =
            checked_decrypt(&ciphertext, aad, g_inv, &privkey).unwrap();

        assert_eq!(msg, plaintext)
    }

    fn test_ciphertext_validation_fails<E: PairingEngine>(
        msg: &[u8],
        aad: &[u8],
        ciphertext: &Ciphertext<E>,
        shared_secret: &E::Fqk,
        g_inv: &E::G1Prepared,
    ) {
        // So far, the ciphertext is valid
        let plaintext = checked_decrypt_with_shared_secret(
            ciphertext,
            aad,
            g_inv,
            shared_secret,
        )
        .unwrap();
        assert_eq!(plaintext, msg);

        // Malformed the ciphertext
        let mut ciphertext = ciphertext.clone();
        ciphertext.ciphertext[0] += 1;
        assert!(checked_decrypt_with_shared_secret(
            &ciphertext,
            aad,
            g_inv,
            shared_secret,
        )
        .is_err());

        // Malformed the AAD
        let aad = "bad aad".as_bytes();
        assert!(checked_decrypt_with_shared_secret(
            &ciphertext,
            aad,
            g_inv,
            shared_secret,
        )
        .is_err());
    }

    #[test]
    fn ciphertext_validity_check() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) = setup_fast::<E>(threshold, shares_num, rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let mut ciphertext = encrypt::<StdRng, E>(msg, aad, &pubkey, rng);

        // So far, the ciphertext is valid
        assert!(check_ciphertext_validity(&ciphertext, aad, g_inv).is_ok());

        // Malformed the ciphertext
        ciphertext.ciphertext[0] += 1;
        assert!(check_ciphertext_validity(&ciphertext, aad, g_inv).is_err());

        // Malformed the AAD
        let aad = "bad aad".as_bytes();
        assert!(check_ciphertext_validity(&ciphertext, aad, g_inv).is_err());
    }

    #[test]
    fn fast_decryption_share_validation() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) = setup_fast::<E>(threshold, shares_num, rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let ciphertext = encrypt::<StdRng, E>(msg, aad, &pubkey, rng);

        let bad_aad = "bad aad".as_bytes();
        assert!(contexts[0]
            .create_share(&ciphertext, bad_aad, g_inv)
            .is_err());
    }

    #[test]
    fn simple_decryption_share_validation() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, rng);
        let _g_inv = &contexts[0].setup_params.g_inv;
        let ciphertext = encrypt::<StdRng, E>(msg, aad, &pubkey, rng);

        let bad_aad = "bad aad".as_bytes();
        assert!(contexts[0].create_share(&ciphertext, bad_aad).is_err());
    }

    #[test]
    fn fast_threshold_encryption() {
        let mut rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_fast::<E>(threshold, shares_num, &mut rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        let mut decryption_shares: Vec<DecryptionShareFast<E>> = vec![];
        for context in contexts.iter() {
            decryption_shares
                .push(context.create_share(&ciphertext, aad, g_inv).unwrap());
        }

        // TODO: Verify and enable this check
        /*for pub_context in contexts[0].public_decryption_contexts.iter() {
            assert!(pub_context
                .blinded_key_shares
                .verify_blinding(&pub_context.public_key_shares, rng));
        }*/

        let prepared_blinded_key_shares = prepare_combine_fast(
            &contexts[0].public_decryption_contexts,
            &decryption_shares,
        );

        let shared_secret = checked_share_combine_fast(
            &contexts[0].public_decryption_contexts,
            &ciphertext,
            &decryption_shares,
            &prepared_blinded_key_shares,
        )
        .unwrap();

        test_ciphertext_validation_fails(
            msg,
            aad,
            &ciphertext,
            &shared_secret,
            g_inv,
        );
    }

    #[test]
    fn simple_threshold_decryption() {
        let mut rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, &mut rng);
        let g_inv = &contexts[0].setup_params.g_inv;

        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        let decryption_shares: Vec<_> = contexts
            .iter()
            .map(|c| c.create_share(&ciphertext, aad).unwrap())
            .collect();

        let shared_secret = make_shared_secret(
            &contexts[0].public_decryption_contexts,
            &decryption_shares,
        );

        test_ciphertext_validation_fails(
            msg,
            aad,
            &ciphertext,
            &shared_secret,
            g_inv,
        );
    }

    #[test]
    fn simple_threshold_decryption_precomputed() {
        let mut rng = &mut test_rng();
        let threshold = 16 * 2 / 3;
        let shares_num = 16;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, &mut rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        let domain = contexts[0]
            .public_decryption_contexts
            .iter()
            .map(|c| c.domain)
            .collect::<Vec<_>>();
        let lagrange_coeffs = prepare_combine_simple::<E>(&domain);

        let decryption_shares: Vec<_> = contexts
            .iter()
            .zip_eq(lagrange_coeffs.iter())
            .map(|(context, lagrange_coeff)| {
                context.create_share_precomputed(&ciphertext, lagrange_coeff)
            })
            .collect();

        let shared_secret =
            share_combine_simple_precomputed::<E>(&decryption_shares);

        test_ciphertext_validation_fails(
            msg,
            aad,
            &ciphertext,
            &shared_secret,
            g_inv,
        );
    }

    #[test]
    fn simple_threshold_decryption_share_verification() {
        let mut rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, &mut rng);

        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        let decryption_shares: Vec<_> = contexts
            .iter()
            .map(|c| c.create_share(&ciphertext, aad).unwrap())
            .collect();

        // In simple tDec variant, we verify decryption shares only after decryption fails.
        // We could do that before, but we prefer to optimize for the happy path.

        // Let's assume that combination failed here. We'll try to verify decryption shares
        // against validator checksums.

        // There is no share aggregation in current version of tpke (it's mocked).
        // ShareEncryptions are called BlindedKeyShares.

        let pub_contexts = &contexts[0].public_decryption_contexts;
        assert!(verify_decryption_shares_simple(
            pub_contexts,
            &ciphertext,
            &decryption_shares,
        ));

        // Now, let's test that verification fails if we one of the decryption shares is invalid.

        let mut has_bad_checksum = decryption_shares[0].clone();
        has_bad_checksum.validator_checksum = has_bad_checksum
            .validator_checksum
            .mul(BigInteger256::rand(rng))
            .into_affine();

        assert!(!has_bad_checksum.verify(
            &pub_contexts[0].blinded_key_share.blinded_key_share,
            &pub_contexts[0].validator_public_key.into_affine(),
            &pub_contexts[0].h.into_projective(),
            &ciphertext,
        ));

        let mut has_bad_share = decryption_shares[0].clone();
        has_bad_share.decryption_share =
            has_bad_share.decryption_share.mul(Fqk::rand(rng));

        assert!(!has_bad_share.verify(
            &pub_contexts[0].blinded_key_share.blinded_key_share,
            &pub_contexts[0].validator_public_key.into_affine(),
            &pub_contexts[0].h.into_projective(),
            &ciphertext,
        ));
    }

    /// Ñ parties (where t <= Ñ <= N) jointly execute a "share recovery" algorithm, and the output is 1 new share.
    /// The new share is intended to restore a previously existing share, e.g., due to loss or corruption.
    #[test]
    fn simple_threshold_decryption_with_share_recovery_at_selected_point() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;

        let (_, _, mut contexts) =
            setup_simple::<E>(threshold, shares_num, rng);

        // Prepare participants

        // First, save the soon-to-be-removed participant
        let selected_participant = contexts.pop().unwrap();
        let x_r = selected_participant
            .public_decryption_contexts
            .last()
            .unwrap()
            .domain;
        let original_private_key_share = selected_participant.private_key_share;

        // Remove one participant from the contexts and all nested structures
        let mut remaining_participants = contexts;
        for p in &mut remaining_participants {
            p.public_decryption_contexts.pop().unwrap();
        }

        // Each participant prepares an update for each other participant
        let domain_points = remaining_participants[0]
            .public_decryption_contexts
            .iter()
            .map(|c| c.domain)
            .collect::<Vec<_>>();
        let h = remaining_participants[0].public_decryption_contexts[0].h;
        let share_updates = remaining_participants
            .iter()
            .map(|p| {
                let deltas_i = prepare_share_updates_for_recovery::<E>(
                    &domain_points,
                    &h,
                    &x_r,
                    threshold,
                    rng,
                );
                (p.index, deltas_i)
            })
            .collect::<HashMap<_, _>>();

        // Participants share updates and update their shares
        let new_share_fragments: Vec<_> = remaining_participants
            .iter()
            .map(|p| {
                // Current participant receives updates from other participants
                let updates_for_participant: Vec<_> = share_updates
                    .values()
                    .map(|updates| *updates.get(p.index).unwrap())
                    .collect();

                // And updates their share
                update_share_for_recovery::<E>(
                    &p.private_key_share,
                    &updates_for_participant,
                )
            })
            .collect();

        // Now, we have to combine new share fragments into a new share
        let domain_points = &remaining_participants[0]
            .public_decryption_contexts
            .iter()
            .map(|ctxt| ctxt.domain)
            .collect::<Vec<_>>();
        let new_private_key_share = recover_share_from_updated_private_shares(
            &x_r,
            domain_points,
            &new_share_fragments,
        );

        assert_eq!(new_private_key_share, original_private_key_share);
    }

    fn make_shared_secret_from_contexts<E: PairingEngine>(
        contexts: &[PrivateDecryptionContextSimple<E>],
        ciphertext: &Ciphertext<E>,
        aad: &[u8],
        _g_inv: &E::G1Prepared,
    ) -> E::Fqk {
        let decryption_shares: Vec<_> = contexts
            .iter()
            .map(|c| c.create_share(ciphertext, aad).unwrap())
            .collect();
        make_shared_secret(
            &contexts[0].public_decryption_contexts,
            &decryption_shares,
        )
    }

    fn make_shared_secret<E: PairingEngine>(
        pub_contexts: &[PublicDecryptionContextSimple<E>],
        decryption_shares: &[DecryptionShareSimple<E>],
    ) -> E::Fqk {
        let domain = pub_contexts.iter().map(|c| c.domain).collect::<Vec<_>>();
        let lagrange = prepare_combine_simple::<E>(&domain);
        share_combine_simple::<E>(decryption_shares, &lagrange)
    }

    /// Ñ parties (where t <= Ñ <= N) jointly execute a "share recovery" algorithm, and the output is 1 new share.
    /// The new share is independent from the previously existing shares. We can use this to on-board a new participant into an existing cohort.
    #[test]
    fn simple_threshold_decryption_with_share_recovery_at_random_point() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        // Create an initial shared secret
        let old_shared_secret = make_shared_secret_from_contexts(
            &contexts,
            &ciphertext,
            aad,
            g_inv,
        );

        // Now, we're going to recover a new share at a random point and check that the shared secret is still the same

        // Our random point
        let x_r = Fr::rand(rng);

        // Remove one participant from the contexts and all nested structures
        let mut remaining_participants = contexts.clone();
        let removed_participant = remaining_participants.pop().unwrap();
        for p in &mut remaining_participants {
            p.public_decryption_contexts.pop().unwrap();
        }

        // Each participant prepares an update for each other participant
        let domain_points = remaining_participants[0]
            .public_decryption_contexts
            .iter()
            .map(|c| c.domain)
            .collect::<Vec<_>>();
        let h = remaining_participants[0].public_decryption_contexts[0].h;
        let share_updates = remaining_participants
            .iter()
            .map(|p| {
                let deltas_i = prepare_share_updates_for_recovery::<E>(
                    &domain_points,
                    &h,
                    &x_r,
                    threshold,
                    rng,
                );
                (p.index, deltas_i)
            })
            .collect::<HashMap<_, _>>();

        // Participants share updates and update their shares
        let new_share_fragments: Vec<_> = remaining_participants
            .iter()
            .map(|p| {
                // Current participant receives updates from other participants
                let updates_for_participant: Vec<_> = share_updates
                    .values()
                    .map(|updates| *updates.get(p.index).unwrap())
                    .collect();

                // And updates their share
                update_share_for_recovery::<E>(
                    &p.private_key_share,
                    &updates_for_participant,
                )
            })
            .collect();

        // Now, we have to combine new share fragments into a new share
        let domain_points = &remaining_participants[0]
            .public_decryption_contexts
            .iter()
            .map(|ctxt| ctxt.domain)
            .collect::<Vec<_>>();
        let new_private_key_share = recover_share_from_updated_private_shares(
            &x_r,
            domain_points,
            &new_share_fragments,
        );

        // Get decryption shares from remaining participants
        let mut decryption_shares: Vec<_> = remaining_participants
            .iter()
            .map(|c| c.create_share(&ciphertext, aad).unwrap())
            .collect();

        // Create a decryption share from a recovered private key share
        let new_validator_decryption_key = Fr::rand(rng);
        let validator_index = removed_participant.index;
        decryption_shares.push(
            DecryptionShareSimple::create(
                validator_index,
                &new_validator_decryption_key,
                &new_private_key_share,
                &ciphertext,
                aad,
                g_inv,
            )
            .unwrap(),
        );

        // Creating a shared secret from remaining shares and the recovered one
        let new_shared_secret = make_shared_secret(
            &remaining_participants[0].public_decryption_contexts,
            &decryption_shares,
        );

        assert_eq!(old_shared_secret, new_shared_secret);
    }

    /// Ñ parties (where t <= Ñ <= N) jointly execute a "share refresh" algorithm.
    /// The output is M new shares (with M <= Ñ), with each of the M new shares substituting the
    /// original share (i.e., the original share is deleted).
    #[test]
    fn simple_threshold_decryption_with_share_refreshing() {
        let rng = &mut test_rng();
        let shares_num = 16;
        let threshold = shares_num * 2 / 3;
        let msg: &[u8] = "abc".as_bytes();
        let aad: &[u8] = "my-aad".as_bytes();

        let (pubkey, _, contexts) =
            setup_simple::<E>(threshold, shares_num, rng);
        let g_inv = &contexts[0].setup_params.g_inv;
        let pub_contexts = contexts[0].public_decryption_contexts.clone();
        let ciphertext = encrypt::<_, E>(msg, aad, &pubkey, rng);

        // Create an initial shared secret
        let old_shared_secret = make_shared_secret_from_contexts(
            &contexts,
            &ciphertext,
            aad,
            g_inv,
        );

        // Now, we're going to refresh the shares and check that the shared secret is the same

        // Dealer computes a new random polynomial with constant term x_r
        let polynomial =
            make_random_polynomial_at::<E>(threshold, &Fr::zero(), rng);

        // Dealer shares the polynomial with participants

        // Participants computes new decryption shares
        let new_decryption_shares: Vec<_> = contexts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // Participant computes share updates and update their private key shares
                let private_key_share = refresh_private_key_share::<E>(
                    &p.setup_params.h.into_projective(),
                    &p.public_decryption_contexts[i].domain,
                    &polynomial,
                    &p.private_key_share,
                );
                DecryptionShareSimple::create(
                    p.index,
                    &p.validator_private_key,
                    &private_key_share,
                    &ciphertext,
                    aad,
                    g_inv,
                )
                .unwrap()
            })
            .collect();

        let new_shared_secret =
            make_shared_secret(&pub_contexts, &new_decryption_shares);

        assert_eq!(old_shared_secret, new_shared_secret);
    }
}
