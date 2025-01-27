// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! DKG (Distributed Key Generation) Pallet for generating threshold BLS keys without
//! a trusted creator. This uses a variant of the classical Pedersen DKG protocol in which
//! the blockchain is used as a realiable broadcast channel. This way one does not need to
//! assume synchronicity of the underlying network, only that the committee members have
//! read access to the blockchain and are able to send transactions, with not too high
//! of a delay. The pallet as well as the protocol are explained in more detail in the
//! accompanying README.md, see also any description of the Pedersen DKG protocol (for
//! instance the original paper https://link.springer.com/chapter/10.1007%2F3-540-46416-6_47).
//! To configure the pallet one must provide in the config 1) the list of authorities running
//! the protocol, 2) the threshold, 3) the block number till which the protocol should terminate.

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{debug, decl_module, decl_storage, decl_event, traits::Get, Parameter};
use frame_system::{
	ensure_signed,
	offchain::{AppCrypto, CreateSignedTransaction, SendSignedTransaction, Signer},
};
use sp_runtime::{
	offchain::storage::StorageValueRef,
	traits::{IdentifyAccount, Member},
	RuntimeAppPublic,
};
use sp_std::{convert::TryInto, vec::Vec};

use codec::Encode;

use sp_dkg::{
	AuthIndex, Commitment, EncryptedShare, EncryptionKey, EncryptionPublicKey, RawSecret, Scalar,
	VerifyKey,
};

mod benchmarking;
mod tests;

pub mod crypto {
	use codec::{Decode, Encode};
	use sp_runtime::{MultiSignature, MultiSigner};

	#[cfg(feature = "std")]
	use serde::{Deserialize, Serialize};
	#[cfg_attr(feature = "std", derive(Deserialize, Serialize))]
	#[derive(Debug, Default, PartialEq, Eq, Clone, PartialOrd, Ord, Decode, Encode)]
	pub struct DKGId(sp_dkg::crypto::Public);
	impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for DKGId {
		type RuntimeAppPublic = sp_dkg::crypto::Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}

	impl From<sp_dkg::crypto::Public> for DKGId {
		fn from(pk: sp_dkg::crypto::Public) -> Self {
			DKGId(pk)
		}
	}

	impl From<MultiSigner> for DKGId {
		fn from(pk: MultiSigner) -> Self {
			match pk {
				MultiSigner::Sr25519(key) => DKGId(key.into()),
				_ => DKGId(Default::default()),
			}
		}
	}

	impl Into<MultiSigner> for DKGId {
		fn into(self) -> MultiSigner {
			MultiSigner::Sr25519(self.0.into())
		}
	}

	impl AsRef<[u8]> for DKGId {
		fn as_ref(&self) -> &[u8] {
			AsRef::<[u8]>::as_ref(&self.0)
		}
	}

	impl sp_runtime::RuntimeAppPublic for DKGId {
		const ID: sp_runtime::KeyTypeId = sp_runtime::KeyTypeId(*b"dkg!");
		const CRYPTO_ID: sp_runtime::CryptoTypeId = sp_dkg::crypto::CRYPTO_ID;
		type Signature = sp_dkg::crypto::Signature;

		fn all() -> sp_std::vec::Vec<Self> {
			sp_dkg::crypto::Public::all()
				.into_iter()
				.map(|p| p.into())
				.collect()
		}

		fn generate_pair(seed: Option<sp_std::vec::Vec<u8>>) -> Self {
			DKGId(sp_dkg::crypto::Public::generate_pair(seed))
		}

		fn sign<M: AsRef<[u8]>>(&self, msg: &M) -> Option<Self::Signature> {
			self.0.sign(msg)
		}

		fn verify<M: AsRef<[u8]>>(&self, msg: &M, signature: &Self::Signature) -> bool {
			self.0.verify(msg, signature)
		}

		fn to_raw_vec(&self) -> sp_std::vec::Vec<u8> {
			self.0.to_raw_vec()
		}
	}
}

pub trait Trait: CreateSignedTransaction<Call<Self>> {
	type Event: From<Event> + Into<<Self as frame_system::Trait>::Event>;
	/// The identifier type for an offchain worker.
	type AuthorityId: Member
		+ Parameter
		+ RuntimeAppPublic
		+ AppCrypto<Self::Public, Self::Signature>
		+ Default
		+ Ord
		+ From<Self::Public>
		+ Into<Self::Public>;

	/// The overarching dispatch call type.
	type Call: From<Call<Self>>;
	type DKGReady: Get<Self::BlockNumber>;
}

decl_storage! {
	trait Store for Module<T: Trait> as DKGWorker {

		/// The current authorities
		pub Authorities: map hasher(twox_64_concat) AuthIndex => T::AuthorityId;


		/// The threshold of BLS scheme
		pub Threshold: u64;
		pub NMembers: u64;

		// round 0 data

		EncryptionPKs: map hasher(twox_64_concat) AuthIndex => EncryptionPublicKey;


		// round 1 data

		// the value under key i is the CommitedPoly of ith node submitted in a tx in round 1
		CommittedPolynomials: map hasher(twox_64_concat) AuthIndex => Vec<Commitment>;
		// the value under key (i,j) is the share ith node dealt for jth node in round 1
		EncryptedShares: map hasher(twox_64_concat) (AuthIndex, AuthIndex) => EncryptedShare;


		// round 2 data

		// map of n bools: ith is true <=> both the below conditions are satisfied:
		// 1) ith node succesfully participated in round 0 and round 1
		// 2) there was no succesful dispute that proves cheating of ith node in round 2
		IsCorrectDealer: map hasher(twox_64_concat) AuthIndex => bool = false;


		// round 3 data

		pub MasterVerificationKey: VerifyKey;
		VerificationKeys: Vec<VerifyKey>;
	}
	add_extra_genesis {
		config(authorities): Vec<T::AuthorityId>;
		config(threshold): u64;
		build(|config| {
			Module::<T>::init_store(&config.authorities);
			Module::<T>::set_threshold(config.threshold);
		})
	}
}

decl_event!(
	pub enum Event {
		/// DKG started with a given number of nodes and given threshold.
		StartDKG(u64, u64),
		/// A Round terminated succesfully for given number of nodes.
		EndRound(u64, u64),
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {

		fn deposit_event() = default;

		#[weight = 1_000_000]
		pub fn post_encryption_key(origin, ix: AuthIndex, pk: EncryptionPublicKey) {
			let now = <frame_system::Module<T>>::block_number();
			if !(now <= Self::round_end(0)) {
				return Ok(());
			}

			if !Self::check_authority(ix, ensure_signed(origin)?) {
				return Ok(())
			}

			EncryptionPKs::insert(ix, pk);
		}

		#[weight = ((shares.len() + comm_poly.len()) as u64 + 1)*(1_000_000)]
		pub fn post_secret_shares(origin, ix: AuthIndex, shares: Vec<Option<EncryptedShare>>, comm_poly: Vec<Commitment>, hash_round0: T::Hash) {
			let now = <frame_system::Module<T>>::block_number();
			if !(now > Self::round_end(0) && now <= Self::round_end(1)) {
				debug::info!("Wrong block number for post_secret_shares: {:?}.", now);
				return Ok(());
			}
			if !Self::check_authority(ix, ensure_signed(origin)?) {
				return Ok(())
			}

			if (shares.len() != Self::n_members() as usize) || (comm_poly.len() as u64 != Self::threshold()) {
				debug::info!("Wrong shares len {:} or comm_poly len {:?} for post_secret_shares.",
					shares.len(),
					comm_poly.len());
				return Ok(());
			}

			if EncryptionPKs::contains_key(ix) {
				let round0_number: T::BlockNumber = Self::round_end(0);
				let correct_hash_round0 = <frame_system::Module<T>>::block_hash(round0_number);
				if hash_round0 == correct_hash_round0 {
					for (share_ix, share) in shares.iter().enumerate() {
						if share.is_some() {
							EncryptedShares::insert((ix, share_ix as AuthIndex), share.unwrap());
						}
					}
					CommittedPolynomials::insert(ix, comm_poly);
					IsCorrectDealer::insert(ix, true);
					debug::info!("Successfully executed post_secret_shares for id {:?} in block {:?}.", ix, now);
				} else {
					debug::info!("Wrong hash_round0 value for post_secret_shares.");
				}
			} else {
				debug::info!("A dealer {:?} who has not posted EncryptionPK tried to submit secret shares.", ix);
			}
		}

		#[weight = (disputes.len() as u64 + 1)*(4_000_000)]
		pub fn post_disputes(origin, ix: AuthIndex, disputes: Vec<(AuthIndex, EncryptionKey)>, hash_round1: T::Hash) {
			let now = <frame_system::Module<T>>::block_number();
			if !(now > Self::round_end(1) && now <= Self::round_end(2)) {
				debug::info!("Wrong block number for post_disputes: {:?}.", now);
				return Ok(());
			}

			if !Self::check_authority(ix, ensure_signed(origin)?) {
				return Ok(())
			}
			let round1_number: T::BlockNumber = Self::round_end(1);
			let correct_hash_round1 = <frame_system::Module<T>>::block_hash(round1_number);
			if hash_round1 == correct_hash_round1 {
				debug::info!("Considering {:?} disputes for {:?} in block {:?}", disputes.len(), ix, now);

				disputes.into_iter().for_each(|(creator, ek)| {
					if IsCorrectDealer::get(creator) == false {
						// No need to consider this dispute, the creator is already marked as incorrect.
						return
					}
					if !Self::check_encryption_key(&ek, creator as usize, ix as usize) {
						// there are 3 possible situations when this if fires, in all cases we can ignore this dispute
						// if ek is wrong then this dispute is unfounded
						// if creator's encryption key is None then he is marked as incorrect anyway
						// if ix's encryption key is None then this dispute is unfounded
						return
					}
					// The below line is fine since encrypted_shares_lists is initialized as a square
					// array with all Nones.

					if !EncryptedShares::contains_key((creator, ix)) {
						IsCorrectDealer::insert(creator, false);
						return
					}
					let encrypted_share = &EncryptedShares::get((creator, ix)).clone();

					let share = ek.decrypt(&encrypted_share);
					if share.is_none() || !Self::verify_share(&share.unwrap(), creator as usize, ix) {
						IsCorrectDealer::insert(creator, false);
					}
				});
			} else {
				debug::info!("Wrong hash_round0 value for post_disputes.");
			}
		}

		fn on_finalize(bn: T::BlockNumber) {
			for round_num in 0..4 {
				if bn == Self::round_end(round_num) {
					let count = match round_num {
						0 => Self::count_encryption_keys_received(),
						_ => Self::count_successful_nodes(),
					};
					Self::deposit_event(Event::EndRound(round_num as u64, count));
				}
			}

			if bn != Self::round_end(2) {
				return
			}

			let n_members = Self::n_members();

			let qualified = Self::is_correct_dealer();
			let mut secret_commitments = Vec::new();
			for i in 0..n_members {
				if qualified[i] && CommittedPolynomials::contains_key(i as AuthIndex) {
					secret_commitments.push(CommittedPolynomials::get(i as AuthIndex)[0].clone());
				}
			}

			let mvk = Commitment::derive_key(secret_commitments);
			MasterVerificationKey::put(mvk);


			let mut vks = Vec::new();
			for ix in 0..n_members {
				let x = &Scalar::from((ix + 1) as u64);
				let part_keys = (0..n_members)
					.filter(|creator| {
						qualified[*creator] && CommittedPolynomials::contains_key(*creator as AuthIndex)
					})
					.map(|creator| Commitment::poly_eval(&CommittedPolynomials::get(creator as AuthIndex), x))
					.collect();
				vks.push(Commitment::derive_key(part_keys))
			}
			VerificationKeys::put(vks);
		}

		fn offchain_worker(block_number: T::BlockNumber) {
			debug::info!("Offchain worker call at block {:?}.", block_number);
			// At the end of Round 2, the public Keybox is ready
			// Round 3 is only for the offchain worker to put the secret key in its storage.
			if block_number < Self::round_end(0)  {
					Self::handle_round0();
			} else if block_number < Self::round_end(1) {
					Self::handle_round1();
			} else if block_number < Self::round_end(2) {
					Self::handle_round2();
			} else if block_number < Self::round_end(3) {
					Self::handle_round3();
			}
		}
	}
}

impl<T: Trait> Module<T> {
	fn init_store(authorities: &[T::AuthorityId]) {
		if !authorities.is_empty() {
			assert!(!NMembers::exists(), "Authorities are already initialized!");
			let n_authorities = authorities.len() as u64;
			NMembers::put(n_authorities);

			let mut authorities = authorities.to_vec();
			authorities.sort();

			authorities
				.into_iter()
				.enumerate()
				.for_each(|(ix, auth)| Authorities::<T>::insert(ix as AuthIndex, auth));
		}
	}

	fn round_end(round_number: usize) -> T::BlockNumber {
		let mut rounds_left = T::DKGReady::get();
		let mut r: usize = 3;
		while round_number < r {
			rounds_left -= rounds_left / ((r + 1) as u32).into();
			r -= 1;
		}
		return rounds_left;
	}

	fn count_successful_nodes() -> u64 {
		let n_members = Self::n_members();
		let mut count = 0;
		for ix in 0..n_members {
			if IsCorrectDealer::contains_key(ix as u64) {
				if IsCorrectDealer::get(ix as u64) == true {
					count += 1;
				}
			}
		}
		count
	}

	fn count_encryption_keys_received() -> u64 {
		let n_members = Self::n_members();
		let mut count = 0;
		for ix in 0..n_members {
			if EncryptionPKs::contains_key(ix as u64) {
				count += 1;
			}
		}
		count
	}

	fn set_threshold(threshold: u64) {
		assert!(
			0 < threshold && threshold <= NMembers::get(),
			"Wrong threshold or n_members"
		);

		assert!(!Threshold::exists(), "Threshold is already initialized!");
		Threshold::set(threshold);
		Self::deposit_event(Event::StartDKG(NMembers::get(), threshold));
	}

	fn check_authority(ix: AuthIndex, who: T::AccountId) -> bool {
		if !Authorities::<T>::contains_key(ix) {
			return false;
		}

		let auth: T::AuthorityId = Authorities::<T>::get(ix);
		return Into::<T::Public>::into(auth).into_account() == who;
	}

	fn build_storage_key(prefix: &[u8], round_number: usize) -> Vec<u8> {
		let mut full_key = Vec::from("dkw::");
		full_key.append(Vec::from(prefix).as_mut());
		if round_number >= 1 {
			let block_number = Self::round_end(round_number - 1);
			let mut hashb = <frame_system::Module<T>>::block_hash(block_number).encode();
			full_key.append(&mut hashb);
		}
		full_key
	}

	// generate encryption pair and send public key on chain
	fn handle_round0() {
		const ALREADY_SET: () = ();

		let (ix, auth) = match Self::local_authority_key() {
			Some(ia) => ia,
			None => return,
		};

		let st_key = Self::build_storage_key(b"enc_key", 0);
		let val = StorageValueRef::persistent(&st_key);
		let res = val.mutate(|last_set: Option<Option<RawSecret>>| match last_set {
			Some(Some(_)) => Err(ALREADY_SET),
			_ => Ok(gen_raw_scalar()),
		});

		if let Ok(Ok(raw_scalar)) = res {
			let signer =
				Signer::<T, T::AuthorityId>::all_accounts().with_filter([auth.into()].to_vec());
			if !signer.can_sign() {
				debug::info!("DKG ERROR NO KEYS FOR SIGNER!!!");
				return;
			}
			let enc_pk = EncryptionPublicKey::from_raw_scalar(raw_scalar);
			let tx_res =
				signer.send_signed_transaction(|_| Call::post_encryption_key(ix, enc_pk.clone()));

			for (_, res) in &tx_res {
				if let Err(e) = res {
					debug::error!("DKG Failed to submit tx with encryption key: {:?}", e)
				}
			}
		}
	}

	// generate secret polynomial, encrypt it, and send it with commitments to the chain
	fn handle_round1() {
		const ALREADY_SET: () = ();

		let (ix, auth) = match Self::local_authority_key() {
			Some(ia) => ia,
			None => return,
		};

		// 0. generate secrets
		let n_members = Self::n_members();
		let threshold = Threshold::get();
		let st_key = Self::build_storage_key(b"secret_poly", 1);
		let val = StorageValueRef::persistent(&st_key);
		let res = val.mutate(|last_set: Option<Option<Vec<RawSecret>>>| match last_set {
			Some(Some(_)) => Err(ALREADY_SET),
			_ => Ok(gen_poly_coeffs(threshold - 1)),
		});

		if res.is_err() {
			return;
		}
		let res = res.unwrap();
		if res.is_err() {
			return;
		}
		let res = res.unwrap();
		let poly = &res.into_iter().map(|raw| Scalar::from_raw(raw)).collect();

		// 1. generate encryption keys
		let encryption_keys = Self::encryption_keys();

		// 2. generate secret shares
		let mut enc_shares = sp_std::vec![None; n_members];

		for ix in 0..n_members {
			if let Some(ref enc_key) = encryption_keys[ix] {
				let x = &Scalar::from((ix + 1) as u64);
				let share = poly_eval(poly, x);
				enc_shares[ix] = Some(enc_key.encrypt(&share));
			}
		}

		// 3. generate commitments
		let mut comms = Vec::new();
		for i in 0..threshold {
			comms.push(Commitment::new(poly[i as usize]));
		}

		// 4. send encrypted secret shares
		let round0_number: T::BlockNumber = Self::round_end(0);
		let hash_round0 = <frame_system::Module<T>>::block_hash(round0_number);
		let signer =
			Signer::<T, T::AuthorityId>::all_accounts().with_filter([auth.into()].to_vec());
		if !signer.can_sign() {
			debug::info!("DKG ERROR NO KEYS FOR SIGNER!!!");
			return;
		}
		let tx_res = signer.send_signed_transaction(|_| {
			Call::post_secret_shares(ix, enc_shares.clone(), comms.clone(), hash_round0)
		});

		for (_, res) in &tx_res {
			if let Err(e) = res {
				debug::error!("DKG Failed to submit tx with secret shares: {:?}", e)
			}
		}
	}

	// decrypt secret shares and send disputes to the chain
	fn handle_round2() {
		const ALREADY_SET: () = ();

		let (my_ix, auth) = match Self::local_authority_key() {
			Some((ix, auth)) => (ix, auth),
			None => return,
		};
		let st_key = Self::build_storage_key(b"secret_shares", 2);
		let val = StorageValueRef::persistent(&st_key);
		let res = val.mutate(
			|last_set: Option<Option<Vec<Option<[u8; 32]>>>>| match last_set {
				Some(Some(_)) => Err(ALREADY_SET),
				_ => Ok(Vec::new()),
			},
		);

		if res.is_err() || res.unwrap().is_err() {
			return;
		}

		let n_members = Self::n_members();

		// 0. generate encryption keys
		let encryption_keys = Self::encryption_keys();

		// 1. decrypt shares, check commitments
		let mut shares = sp_std::vec![None; n_members];
		let mut disputes = Vec::new();

		for creator in 0..n_members {
			let ek = &encryption_keys[creator];
			if ek.is_none() {
				// either the creator or us did not provide an encryption key
				// in the former case the creator is marked as incorrect dealer already
				// in the latter -- there is nothing to dispute
				continue;
			}
			if !EncryptedShares::contains_key((creator as AuthIndex, my_ix)) {
				disputes.push((creator as AuthIndex, ek.clone().unwrap()));
				continue;
			}
			let encrypted_share = &EncryptedShares::get((creator as AuthIndex, my_ix)).clone();
			let share = ek.as_ref().unwrap().decrypt(&encrypted_share);
			if share.is_none() || !Self::verify_share(&share.unwrap(), creator, my_ix) {
				disputes.push((creator as AuthIndex, ek.clone().unwrap()));
			} else {
				shares[creator] = Some(share.unwrap().to_bytes());
			}
		}

		// 2. save shares
		let res: Result<_, ()> = val.mutate(|_| Ok(shares));

		if res.is_err() || res.unwrap().is_err() {
			debug::info!("DKG handle_round2 error in setting shares");
			return;
		}

		// 3. send disputes
		let round1_number: T::BlockNumber = Self::round_end(1);
		let hash_round1 = <frame_system::Module<T>>::block_hash(round1_number);

		let signer =
			Signer::<T, T::AuthorityId>::all_accounts().with_filter([auth.into()].to_vec());
		if !signer.can_sign() {
			debug::info!("DKG ERROR NO KEYS FOR SIGNER {:?}!!!", my_ix);
			return;
		}

		let tx_res = signer
			.send_signed_transaction(|_| Call::post_disputes(my_ix, disputes.clone(), hash_round1));

		for (_, res) in &tx_res {
			if let Err(e) = res {
				debug::error!("DKG Failed to submit transaction with disputes: {:?}", e)
			}
		}
	}

	// derive local key pair
	fn handle_round3() {
		const ALREADY_SET: () = ();

		let qualified = Self::is_correct_dealer();

		let st_key_secret_key = Self::build_storage_key(b"threshold_secret_key", 3);
		let val = StorageValueRef::persistent(&st_key_secret_key);
		let res = val.mutate(|last_set: Option<Option<[u8; 32]>>| match last_set {
			Some(Some(_)) => Err(ALREADY_SET),
			_ => Ok([0; 32]),
		});

		if res.is_err() || res.unwrap().is_err() {
			return;
		}

		let st_key_secret_shares = Self::build_storage_key(b"secret_shares", 2);
		let secret = StorageValueRef::persistent(&st_key_secret_shares)
			.get::<Vec<Option<[u8; 32]>>>()
			.unwrap()
			.unwrap()
			.iter()
			.enumerate()
			.filter_map(|(ix, &share)| match (qualified[ix], share) {
				(false, _) | (true, None) => None,
				(true, Some(share)) => Some(Scalar::from_bytes(&share).unwrap()),
			})
			.fold(Scalar::zero(), |a, b| a + b);

		let res: Result<_, ()> = val.mutate(|_| Ok(secret.to_bytes()));

		if res.is_err() || res.unwrap().is_err() {
			debug::info!("DKG handle_round3 error in setting secret threshold key");
		}
	}

	fn local_authority_key() -> Option<(AuthIndex, T::AuthorityId)> {
		let st_key = Self::build_storage_key(b"local_key_info", 0);
		let maybe_key_info = StorageValueRef::persistent(&st_key).get();
		match maybe_key_info {
			Some(Some(key_info)) => return key_info,
			_ => {
				let local_keys = T::AuthorityId::all();
				let key_info = Authorities::<T>::iter().find_map(move |(index, authority)| {
					local_keys
						.clone()
						.into_iter()
						.position(|local_key| authority == local_key)
						.map(|location| (index as AuthIndex, local_keys[location].clone()))
				});
				StorageValueRef::persistent(&st_key).set(&key_info);
				return key_info;
			}
		}
	}

	fn get_encryption_key(creator: usize) -> Option<EncryptionKey> {
		let st_key = Self::build_storage_key(b"enc_key", 0);
		let raw_secret = StorageValueRef::persistent(&st_key).get().unwrap().unwrap();
		let secret = Scalar::from_raw(raw_secret);
		if EncryptionPKs::contains_key(creator as AuthIndex) {
			return Some(EncryptionPKs::get(creator as AuthIndex).to_encryption_key(secret));
		} else {
			return None;
		}
	}

	fn encryption_keys() -> Vec<Option<EncryptionKey>> {
		let mut keys = Vec::new();
		let n = Self::n_members();
		for i in 0..n {
			keys.push(Self::get_encryption_key(i));
		}
		keys
	}
	fn is_correct_dealer() -> Vec<bool> {
		let mut is_correct = Vec::new();
		let n = Self::n_members();
		for i in 0..n {
			is_correct.push(IsCorrectDealer::get(i as AuthIndex));
		}
		is_correct
	}

	fn verify_share(share: &Scalar, creator: usize, issuer: AuthIndex) -> bool {
		if !CommittedPolynomials::contains_key(creator as AuthIndex) {
			return false;
		}
		Commitment::poly_eval(
			&CommittedPolynomials::get(creator as AuthIndex),
			&Scalar::from(issuer + 1),
		)
		.verify_share(&share)
	}

	fn check_encryption_key(encryption_key: &EncryptionKey, creator: usize, issuer: usize) -> bool {
		if !EncryptionPKs::contains_key(creator as AuthIndex)
			|| !EncryptionPKs::contains_key(issuer as AuthIndex)
		{
			return false;
		}
		let epk1 = &EncryptionPKs::get(creator as AuthIndex);
		let epk2 = &EncryptionPKs::get(issuer as AuthIndex);
		encryption_key.is_correct(epk1, epk2)
	}

	pub fn master_verification_key() -> Option<VerifyKey> {
		match MasterVerificationKey::exists() {
			true => Some(MasterVerificationKey::get()),
			false => None,
		}
	}

	pub fn master_key_ready() -> T::BlockNumber {
		// this is the round number when the outside world expects master key to be ready
		T::DKGReady::get()
	}

	pub fn public_keybox_parts() -> Option<(Option<AuthIndex>, Vec<VerifyKey>, VerifyKey, u64)> {
		// we cannot call local_authority_key() here to fetch ix above, because local_authority_key()
		// uses the offchain_worker storage, which cannot be used outside of an offchain worker context
		let local_keys = T::AuthorityId::all();
		let ix = Authorities::<T>::iter().find_map(move |(index, authority)| {
			local_keys
				.clone()
				.into_iter()
				.position(|local_key| authority == local_key)
				.map(|_| index as AuthIndex)
		});

		let verification_keys = match Self::verification_keys() {
			Some(keys) => keys,
			None => return None,
		};

		let master_key = match Self::master_verification_key() {
			Some(key) => key,
			None => return None,
		};

		let threshold = Self::threshold();

		Some((ix, verification_keys, master_key, threshold))
	}

	pub fn storage_key_sk() -> Option<Vec<u8>> {
		let now = <frame_system::Module<T>>::block_number();
		let deadline_round_2 = Self::round_end(2);
		if now <= deadline_round_2 {
			return None;
		}
		Some(Self::build_storage_key(b"threshold_secret_key", 3))
	}

	pub fn verification_keys() -> Option<Vec<VerifyKey>> {
		if !VerificationKeys::exists() {
			return None;
		}

		Some(VerificationKeys::get())
	}

	pub fn threshold() -> u64 {
		Threshold::get()
	}

	pub fn n_members() -> usize {
		NMembers::get() as usize
	}
}

impl<T: Trait> sp_runtime::BoundToRuntimeAppPublic for Module<T> {
	type Public = T::AuthorityId;
}

fn u8_array_to_raw_scalar(bytes: [u8; 32]) -> RawSecret {
	let mut out = [0u64; 4];
	for i in 0..4 {
		out[i] = u64::from_le_bytes(
			bytes[8 * i..8 * (i + 1)]
				.try_into()
				.expect("slice with incorrect length"),
		);
	}
	out
}

fn gen_raw_scalar() -> RawSecret {
	u8_array_to_raw_scalar(sp_io::offchain::random_seed())
}

fn gen_poly_coeffs(deg: u64) -> Vec<RawSecret> {
	let mut coeffs = Vec::new();
	for _ in 0..deg + 1 {
		coeffs.push(gen_raw_scalar());
	}

	coeffs
}

fn poly_eval(coeffs: &Vec<Scalar>, x: &Scalar) -> Scalar {
	let mut eval = Scalar::zero();
	for coeff in coeffs.iter().rev() {
		eval *= x;
		eval += coeff;
	}

	eval
}
