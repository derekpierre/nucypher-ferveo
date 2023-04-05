use std::collections::BTreeMap;

use anyhow::{anyhow, Context};
use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup, Group};
use ark_poly::EvaluationDomain;
use ferveo_common::{is_power_of_2, ExternalValidator};
use measure_time::print_time;
use rand::RngCore;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    aggregate, make_validators, AggregatedPvss, DkgState, Error, Params,
    PubliclyVerifiableParams, PubliclyVerifiableSS, Pvss, Result,
};

/// The DKG context that holds all of the local state for participating in the DKG
// TODO: Consider removing Clone to avoid accidentally NOT-mutating state.
//  Currently, we're assuming that the DKG is only mutated by the owner of the instance.
//  Consider removing Clone after finalizing ferveo::api
#[derive(Clone, Debug)]
pub struct PubliclyVerifiableDkg<E: Pairing> {
    pub params: Params,
    pub pvss_params: PubliclyVerifiableParams<E>,
    // TODO: What is session_keypair?
    pub session_keypair: ferveo_common::Keypair<E>,
    pub validators: Vec<ferveo_common::Validator<E>>,
    pub vss: BTreeMap<u32, PubliclyVerifiableSS<E>>,
    pub domain: ark_poly::Radix2EvaluationDomain<E::ScalarField>,
    pub state: DkgState<E>,
    pub me: usize,
}

impl<E: Pairing> PubliclyVerifiableDkg<E> {
    /// Create a new DKG context to participate in the DKG
    /// Every identity in the DKG is linked to an ed25519 public key;
    /// `validators`: List of validators
    /// `params` contains the parameters of the DKG such as number of shares
    /// `me` the validator creating this instance
    /// `session_keypair` the keypair for `me`
    pub fn new(
        validators: &[ExternalValidator<E>],
        params: Params,
        me: &ExternalValidator<E>,
        session_keypair: ferveo_common::Keypair<E>,
    ) -> Result<Self> {
        // Make sure that the number of shares is a power of 2 for the FFT to work (Radix-2 FFT domain is being used)
        if !is_power_of_2(params.shares_num) {
            return Err(Error::Other(anyhow!(
                "number of shares must be a power of 2"
            )));
        }

        let domain = ark_poly::Radix2EvaluationDomain::<E::ScalarField>::new(
            params.shares_num as usize,
        )
        .expect("unable to construct domain");

        // keep track of the owner of this instance in the validator set
        let me = validators.iter().position(|probe| me == probe).context(
            "could not find this validator in the provided validator set",
        )?;

        let validators = make_validators(validators);

        Ok(Self {
            session_keypair,
            params,
            pvss_params: PubliclyVerifiableParams::<E> {
                g: E::G1::generator(),
                h: E::G2::generator(),
            },
            vss: BTreeMap::new(),
            domain,
            state: DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            },
            me,
            validators,
        })
    }

    /// Create a new PVSS instance within this DKG session, contributing to the final key
    /// `rng` is a cryptographic random number generator
    /// Returns a PVSS dealing message to post on-chain
    pub fn share<R: RngCore>(&mut self, rng: &mut R) -> Result<Message<E>> {
        print_time!("PVSS Sharing");
        let vss = self.create_share(rng)?;
        match self.state {
            DkgState::Sharing { .. } | DkgState::Dealt => {
                Ok(Message::Deal(vss))
            }
            _ => Err(Error::Other(anyhow!(
                "DKG is not in a valid state to deal PVSS shares"
            ))),
        }
    }

    pub fn create_share<R: RngCore>(
        &self,
        rng: &mut R,
    ) -> Result<PubliclyVerifiableSS<E>> {
        use ark_std::UniformRand;
        Pvss::<E>::new(&E::ScalarField::rand(rng), self, rng)
    }

    /// Aggregate all received PVSS messages into a single message, prepared to post on-chain
    pub fn aggregate(&self) -> Result<Message<E>> {
        match self.state {
            DkgState::Dealt => {
                let final_key = self.final_key();
                Ok(Message::Aggregate(Aggregation {
                    vss: aggregate(self),
                    final_key,
                }))
            }
            _ => Err(Error::Other(anyhow!(
                "Not enough PVSS transcripts received to aggregate"
            ))),
        }
    }

    /// Returns the public key generated by the DKG
    pub fn final_key(&self) -> E::G1Affine {
        self.vss
            .values()
            .map(|vss| vss.coeffs[0].into_group())
            .sum::<E::G1>()
            .into_affine()
    }

    /// Verify a DKG related message in a block proposal
    /// `sender` is the validator of the sender of the message
    /// `payload` is the content of the message
    pub fn verify_message(
        &self,
        sender: &ExternalValidator<E>,
        payload: &Message<E>,
    ) -> Result<()> {
        match payload {
            Message::Deal(pvss) if matches!(self.state, DkgState::Sharing { .. } | DkgState::Dealt) => {
                // TODO: If this is two slow, we can convert self.validators to
                // an address keyed hashmap after partitioning the shares shares
                // in the [`new`] method
                let sender = self
                    .validators
                    .iter()
                    .position(|probe| sender == &probe.validator)
                    .context("dkg received unknown dealer")?;
                if self.vss.contains_key(&(sender as u32)) {
                    Err(Error::Other(anyhow!("Repeat dealer {}", sender)))
                } else if !pvss.verify_optimistic() {
                    Err(Error::Other(anyhow!("Invalid PVSS transcript")))
                } else {
                    Ok(())
                }
            }
            Message::Aggregate(Aggregation { vss, final_key }) if matches!(self.state, DkgState::Dealt) => {
                let minimum_shares = self.params.shares_num - self.params.security_threshold;
                let verified_shares = vss.verify_aggregation(self)?;
                // we reject aggregations that fail to meet the security threshold
                if verified_shares < minimum_shares {
                    Err(Error::Other(anyhow!(
                        "Aggregation failed because the verified shares was insufficient"
                    )))
                } else if &self.final_key() == final_key {
                    Ok(())
                } else {
                    Err(Error::Other(anyhow!(
                        "The final key was not correctly derived from the aggregated transcripts"
                    )))
                }
            }
            _ => Err(Error::Other(anyhow!(
                "DKG state machine is not in correct state to verify this message"
            ))),
        }
    }

    /// After consensus has agreed to include a verified
    /// message on the blockchain, we apply the chains
    /// to the state machine
    pub fn apply_message(
        &mut self,
        sender: ExternalValidator<E>,
        payload: Message<E>,
    ) -> Result<()> {
        match payload {
            Message::Deal(pvss) if matches!(self.state, DkgState::Sharing { .. } | DkgState::Dealt) => {
                // Add the ephemeral public key and pvss transcript
                let sender = self
                    .validators
                    .iter()
                    .position(|probe| sender.address == probe.validator.address)
                    .context("dkg received unknown dealer")?;
                self.vss.insert(sender as u32, pvss);

                // we keep track of the amount of shares seen until the security
                // threshold is met. Then we may change the state of the DKG
                if let DkgState::Sharing {
                    ref mut accumulated_shares,
                    ..
                } = &mut self.state
                {
                    *accumulated_shares += 1;
                    if *accumulated_shares >= self.params.security_threshold {
                        self.state = DkgState::Dealt;
                    }
                }
                Ok(())
            }
            Message::Aggregate(_) if matches!(self.state, DkgState::Dealt) => {
                // change state and cache the final key
                self.state = DkgState::Success {
                    final_key: self.final_key(),
                };
                Ok(())
            }
            _ => Err(Error::Other(anyhow!(
                "DKG state machine is not in correct state to apply this message"
            ))),
        }
    }

    pub fn deal(
        &mut self,
        sender: ExternalValidator<E>,
        pvss: Pvss<E>,
    ) -> Result<()> {
        // Add the ephemeral public key and pvss transcript
        let sender = self
            .validators
            .iter()
            .position(|probe| sender.address == probe.validator.address)
            .context("dkg received unknown dealer")?;
        self.vss.insert(sender as u32, pvss);
        Ok(())
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(bound(
    serialize = "AggregatedPvss<E>: Serialize",
    deserialize = "AggregatedPvss<E>: DeserializeOwned"
))]
pub struct Aggregation<E: Pairing> {
    vss: AggregatedPvss<E>,
    #[serde_as(as = "ferveo_common::serialization::SerdeAs")]
    final_key: E::G1Affine,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(bound(
    serialize = "AggregatedPvss<E>: Serialize, Pvss<E>: Serialize",
    deserialize = "AggregatedPvss<E>: DeserializeOwned, Pvss<E>: DeserializeOwned"
))]
pub enum Message<E: Pairing> {
    Deal(Pvss<E>),
    Aggregate(Aggregation<E>),
}

/// Factory functions for testing
#[cfg(test)]
pub(crate) mod test_common {
    pub use ark_bls12_381::Bls12_381 as EllipticCurve;
    pub use ark_ff::UniformRand;

    pub use super::*;

    pub type G1 = <EllipticCurve as Pairing>::G1Affine;

    pub fn gen_n_keypairs(
        n: u32,
    ) -> Vec<ferveo_common::Keypair<EllipticCurve>> {
        let rng = &mut ark_std::test_rng();
        (0..n)
            .map(|_| ferveo_common::Keypair::<EllipticCurve>::new(rng))
            .collect()
    }

    /// Generate a set of keypairs for each validator
    pub fn gen_keypairs() -> Vec<ferveo_common::Keypair<EllipticCurve>> {
        gen_n_keypairs(4)
    }

    pub fn gen_n_validators(
        keypairs: &[ferveo_common::Keypair<EllipticCurve>],
        n: u32,
    ) -> Vec<ExternalValidator<EllipticCurve>> {
        (0..n)
            .map(|i| ExternalValidator {
                address: format!("validator_{}", i),
                public_key: keypairs[i as usize].public(),
            })
            .collect()
    }

    /// Generate a few validators
    pub fn gen_validators(
        keypairs: &[ferveo_common::Keypair<EllipticCurve>],
    ) -> Vec<ExternalValidator<EllipticCurve>> {
        gen_n_validators(keypairs, 4)
    }

    pub fn setup_dkg_for_n_validators(
        security_threshold: u32,
        shares_num: u32,
        my_index: usize,
    ) -> PubliclyVerifiableDkg<EllipticCurve> {
        let keypairs = gen_n_keypairs(shares_num);
        let validators = gen_n_validators(&keypairs, shares_num);
        let me = validators[my_index].clone();
        PubliclyVerifiableDkg::new(
            &validators,
            Params {
                tau: 0,
                security_threshold,
                shares_num,
            },
            &me,
            keypairs[my_index],
        )
        .expect("Setup failed")
    }

    /// Create a test dkg
    ///
    /// The [`test_dkg_init`] module checks correctness of this setup
    pub fn setup_dkg(validator: usize) -> PubliclyVerifiableDkg<EllipticCurve> {
        setup_dkg_for_n_validators(2, 4, validator)
    }

    /// Set up a dkg with enough pvss transcripts to meet the threshold
    ///
    /// The correctness of this function is tested in the module [`test_dealing`]
    pub fn setup_dealt_dkg() -> PubliclyVerifiableDkg<EllipticCurve> {
        setup_dealt_dkg_with_n_validators(2, 4)
    }

    pub fn setup_dealt_dkg_with_n_validators(
        security_threshold: u32,
        shares_num: u32,
    ) -> PubliclyVerifiableDkg<EllipticCurve> {
        let rng = &mut ark_std::test_rng();

        // Gather everyone's transcripts
        let transcripts = (0..shares_num).map(|i| {
            let mut dkg = setup_dkg_for_n_validators(
                security_threshold,
                shares_num,
                i as usize,
            );
            dkg.share(rng).expect("Test failed")
        });

        // Our test dkg
        let mut dkg =
            setup_dkg_for_n_validators(security_threshold, shares_num, 0);
        transcripts.enumerate().for_each(|(sender, pvss)| {
            dkg.apply_message(dkg.validators[sender].validator.clone(), pvss)
                .expect("Setup failed");
        });
        dkg
    }
}

/// Test initializing DKG
#[cfg(test)]
mod test_dkg_init {
    use super::test_common::*;

    /// Test that dkg fails to start if the `me` input
    /// is not in the validator set
    #[test]
    fn test_dkg_fail_unknown_validator() {
        let rng = &mut ark_std::test_rng();
        let keypairs = gen_keypairs();
        let keypair = ferveo_common::Keypair::<EllipticCurve>::new(rng);
        let err = PubliclyVerifiableDkg::<EllipticCurve>::new(
            &gen_validators(&keypairs),
            Params {
                tau: 0,
                security_threshold: 4,
                shares_num: 8,
            },
            &ExternalValidator::<EllipticCurve> {
                address: "non-existant-validator".into(),
                public_key: keypair.public(),
            },
            keypair,
        )
        .expect_err("Test failed");
        assert_eq!(err.to_string(), "Something went wrong")
    }
}

/// Test the dealing phase of the DKG
#[cfg(test)]
mod test_dealing {
    use ark_ec::AffineRepr;

    use super::test_common::*;
    use crate::DkgState::Dealt;

    /// Test that dealing correct PVSS transcripts
    /// pass verification an application and that
    /// state is updated correctly
    #[test]
    fn test_pvss_dealing() {
        let rng = &mut ark_std::test_rng();
        // gather everyone's transcripts
        let mut transcripts = vec![];
        for i in 0..4 {
            let mut dkg = setup_dkg(i);
            transcripts.push(dkg.share(rng).expect("Test failed"));
        }
        // our test dkg
        let mut dkg = setup_dkg(0);

        let mut expected = 0u32;
        for (sender, pvss) in transcripts.iter().enumerate() {
            // check the verification passes
            assert!(dkg
                .verify_message(&dkg.validators[sender].validator, pvss)
                .is_ok());
            // check that application passes
            assert!(dkg
                .apply_message(
                    dkg.validators[sender].validator.clone(),
                    pvss.clone(),
                )
                .is_ok());

            expected += 1;
            if sender < (dkg.params.security_threshold - 1) as usize {
                // check that shares accumulates correctly
                match dkg.state {
                    DkgState::Sharing {
                        accumulated_shares, ..
                    } => {
                        assert_eq!(accumulated_shares, expected)
                    }
                    _ => panic!("Test failed"),
                }
            } else {
                // check that when enough shares is accumulated, we transition state
                assert!(matches!(dkg.state, DkgState::Dealt));
            }
        }
    }

    /// Test the verification and application of
    /// pvss transcripts from unknown validators
    /// are rejected
    #[test]
    fn test_pvss_from_unknown_dealer_rejected() {
        let rng = &mut ark_std::test_rng();
        let mut dkg = setup_dkg(0);
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0
            }
        ));
        let pvss = dkg.share(rng).expect("Test failed");
        let sender = ExternalValidator::<EllipticCurve> {
            address: "fake-address".into(),
            public_key: ferveo_common::Keypair::<EllipticCurve>::new(rng)
                .public(),
        };
        // check that verification fails
        assert!(dkg.verify_message(&sender, &pvss).is_err());
        // check that application fails
        assert!(dkg.apply_message(sender, pvss).is_err());
        // check that state has not changed
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));
    }

    /// Test that if a validator sends two pvss transcripts,
    /// the second fails to verify
    #[test]
    fn test_pvss_sent_twice_rejected() {
        let rng = &mut ark_std::test_rng();
        let mut dkg = setup_dkg(0);
        // We start with an empty state
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));

        let pvss = dkg.share(rng).expect("Test failed");
        let sender = dkg.validators[3].validator.clone();

        // First PVSS is accepted
        assert!(dkg.verify_message(&sender, &pvss).is_ok());
        assert!(dkg.apply_message(sender.clone(), pvss.clone()).is_ok());
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 1,
                block: 0,
            }
        ));

        // Second PVSS is rejected
        assert!(dkg.verify_message(&sender, &pvss).is_err());
    }

    /// Test that if a validators tries to verify it's own
    /// share message, it passes
    #[test]
    fn test_own_pvss() {
        let rng = &mut ark_std::test_rng();
        let mut dkg = setup_dkg(0);
        // We start with an empty state
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));

        // Sender creates a PVSS transcript
        let pvss = dkg.share(rng).expect("Test failed");
        // Note that state of DKG has not changed
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));

        let sender = dkg.validators[0].validator.clone();

        // Sender verifies it's own PVSS transcript
        assert!(dkg.verify_message(&sender, &pvss).is_ok());
        assert!(dkg.apply_message(sender, pvss).is_ok());
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 1,
                block: 0,
            }
        ));
    }

    /// Test that the [`PubliclyVerifiableDkg<E>::share`] method
    /// errors if its state is not [`DkgState::Shared{..} | Dkg::Dealt`]
    #[test]
    fn test_pvss_cannot_share_from_wrong_state() {
        let rng = &mut ark_std::test_rng();
        let mut dkg = setup_dkg(0);
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));

        dkg.state = DkgState::Success {
            final_key: G1::zero(),
        };
        assert!(dkg.share(rng).is_err());

        // check that even if security threshold is met, we can still share
        dkg.state = Dealt;
        assert!(dkg.share(rng).is_ok());
    }

    /// Check that share messages can only be
    /// verified or applied if the dkg is in
    /// state [`DkgState::Share{..} | DkgState::Dealt`]
    #[test]
    fn test_share_message_state_guards() {
        let rng = &mut ark_std::test_rng();
        let mut dkg = setup_dkg(0);
        let pvss = dkg.share(rng).expect("Test failed");
        assert!(matches!(
            dkg.state,
            DkgState::Sharing {
                accumulated_shares: 0,
                block: 0,
            }
        ));
        let sender = dkg.validators[3].validator.clone();
        dkg.state = DkgState::Success {
            final_key: G1::zero(),
        };
        assert!(dkg.verify_message(&sender, &pvss).is_err());
        assert!(dkg.apply_message(sender.clone(), pvss.clone()).is_err());

        // check that we can still accept pvss transcripts after meeting threshold
        dkg.state = Dealt;
        assert!(dkg.verify_message(&sender, &pvss).is_ok());
        assert!(dkg.apply_message(sender, pvss).is_ok());
        assert!(matches!(dkg.state, DkgState::Dealt))
    }
}

/// Test aggregating transcripts into final key
#[cfg(test)]
mod test_aggregation {
    use ark_ec::AffineRepr;

    use super::test_common::*;

    /// Test that if the security threshold is
    /// met, we can create a final key
    #[test]
    fn test_aggregate() {
        let mut dkg = setup_dealt_dkg();
        let aggregate = dkg.aggregate().expect("Test failed");
        let sender = dkg.validators[dkg.me].validator.clone();
        assert!(dkg.verify_message(&sender, &aggregate).is_ok());
        assert!(dkg.apply_message(sender, aggregate).is_ok());
        assert!(matches!(dkg.state, DkgState::Success { .. }));
    }

    /// Test that aggregate only succeeds if we are in
    /// the state [`DkgState::Dealt]
    #[test]
    fn test_aggregate_state_guards() {
        let mut dkg = setup_dealt_dkg();
        dkg.state = DkgState::Sharing {
            accumulated_shares: 0,
            block: 0,
        };
        assert!(dkg.aggregate().is_err());
        dkg.state = DkgState::Success {
            final_key: G1::zero(),
        };
        assert!(dkg.aggregate().is_err());
    }

    /// Test that aggregate message fail to be verified
    /// or applied unless dkg.state is
    /// [`DkgState::Dealt`]
    #[test]
    fn test_aggregate_message_state_guards() {
        let mut dkg = setup_dealt_dkg();
        let aggregate = dkg.aggregate().expect("Test failed");
        let sender = dkg.validators[dkg.me].validator.clone();
        dkg.state = DkgState::Sharing {
            accumulated_shares: 0,
            block: 0,
        };
        assert!(dkg.verify_message(&sender, &aggregate).is_err());
        assert!(dkg
            .apply_message(sender.clone(), aggregate.clone())
            .is_err());
        dkg.state = DkgState::Success {
            final_key: G1::zero(),
        };
        assert!(dkg.verify_message(&sender, &aggregate).is_err());
        assert!(dkg.apply_message(sender, aggregate).is_err())
    }

    /// Test that an aggregate message will fail to verify if the
    /// security threshold is not met
    #[test]
    fn test_aggregate_wont_verify_if_under_threshold() {
        let mut dkg = setup_dealt_dkg();
        dkg.params.shares_num = 10;
        let aggregate = dkg.aggregate().expect("Test failed");
        let sender = dkg.validators[dkg.me].validator.clone();
        assert!(dkg.verify_message(&sender, &aggregate).is_err());
    }

    /// If the aggregated pvss passes, check that the announced
    /// key is correct. Verification should fail if it is not
    #[test]
    fn test_aggregate_wont_verify_if_wrong_key() {
        let mut dkg = setup_dealt_dkg();
        let mut aggregate = dkg.aggregate().expect("Test failed");
        while dkg.final_key() == G1::zero() {
            dkg = setup_dealt_dkg();
        }
        if let Message::Aggregate(Aggregation { final_key, .. }) =
            &mut aggregate
        {
            *final_key = G1::zero();
        }
        let sender = dkg.validators[dkg.me].validator.clone();
        assert!(dkg.verify_message(&sender, &aggregate).is_err());
    }
}
