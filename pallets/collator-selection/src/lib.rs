// Copyright 2020-2021 Manta Network.
// This file is part of Manta.
//
// Manta is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// Manta is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with Manta.  If not, see <http://www.gnu.org/licenses/>.
//
// The pallet-collator-selection pallet is forked from Parity's cumulus module:
// https://github.com/paritytech/cumulus/tree/master/pallets/collator-selection
// The original license is Apache-2.0.

//! Collator Selection pallet.
//!
//! A pallet to manage collators in a parachain.
//!
//! ## Overview
//!
//! The Collator Selection pallet manages the collators of a parachain. **Collation is _not_ a
//! secure activity** and this pallet does not implement any game-theoretic mechanisms to meet BFT
//! safety assumptions of the chosen set.
//!
//! ## Terminology
//!
//! - Collator: A parachain block producer.
//! - Bond: An amount of `Balance` _reserved_ for candidate registration.
//! - Invulnerable: An account guaranteed to be in the collator set.
//!
//! ## Implementation
//!
//! The final [`Collators`] are aggregated from two individual lists:
//!
//! 1. [`Invulnerables`]: a set of collators appointed by governance. These accounts will always be
//!    collators.
//! 2. [`Candidates`]: these are *candidates to the collation task* and may or may not be elected as
//!    a final collator.
//!
//! The current implementation resolves congestion of [`Candidates`] in a first-come-first-serve
//! manner.
//!
//! ### Rewards
//!
//! The Collator Selection pallet maintains an on-chain account (the "Pot"). In each block, the
//! collator who authored it receives:
//!
//! - Half the value of the Pot.
//! - Half the value of the transaction fees within the block. The other half of the transaction
//!   fees are deposited into the Pot.
//!
//! To initiate rewards an ED needs to be transferred to the pot address.
//!
//! Note: Eventually the Pot distribution may be modified as discussed in
//! [this issue](https://github.com/paritytech/statemint/issues/21#issuecomment-810481073).

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod weights;

#[frame_support::pallet]
pub mod pallet {
	pub use crate::weights::WeightInfo;
	use core::ops::Div;
	use frame_support::{
		dispatch::DispatchResultWithPostInfo,
		inherent::Vec,
		pallet_prelude::*,
		sp_runtime::{
			traits::{AccountIdConversion, CheckedSub, Zero},
			RuntimeDebug,
		},
		traits::{
			Currency, EnsureOrigin, ExistenceRequirement::KeepAlive, ReservableCurrency,
			ValidatorRegistration, ValidatorSet,
		},
		weights::DispatchClass,
		PalletId,
	};
	use frame_system::{pallet_prelude::*, Config as SystemConfig};
	use pallet_session::SessionManager;
	use sp_runtime::traits::Convert;
	use sp_staking::SessionIndex;

	type BalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as SystemConfig>::AccountId>>::Balance;

	/// A convertor from collators id. Since this pallet does not have stash/controller, this is
	/// just identity.
	pub struct IdentityCollator;
	impl<T> sp_runtime::traits::Convert<T, Option<T>> for IdentityCollator {
		fn convert(t: T) -> Option<T> {
			Some(t)
		}
	}

	/// Configure the pallet by specifying the parameters and types on which it depends.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The currency mechanism.
		type Currency: ReservableCurrency<Self::AccountId>;

		/// Origin that can dictate updating parameters of this pallet.
		type UpdateOrigin: EnsureOrigin<Self::Origin>;

		/// Account Identifier from which the internal Pot is generated.
		type PotId: Get<PalletId>;

		/// Maximum number of candidates that we should have. This is used for benchmarking and is not
		/// enforced.
		///
		/// This does not take into account the invulnerables.
		type MaxCandidates: Get<u32>;

		/// Maximum number of invulnerables.
		///
		/// Used only for benchmarking.
		type MaxInvulnerables: Get<u32>;

		// n-th Percentile of lowest-performing collators to be checked for kicking
		type PerformancePercentileToConsiderForKick: Get<u8>;

		// If a collator underperforms the percentile by more than this, it'll be kicked
		type UnderperformPercentileByPercentToKick: Get<u8>;

		/// A stable ID for a validator.
		type ValidatorId: Member + Parameter;

		/// A conversion from account ID to validator ID.
		///
		/// Its cost must be at most one storage read.
		type ValidatorIdOf: Convert<Self::AccountId, Option<Self::ValidatorId>>;

		/// Validate a user is registered
		type ValidatorRegistration: ValidatorRegistration<Self::ValidatorId>
			+ ValidatorSet<Self::ValidatorId>;

		/// The weight information of this pallet.
		type WeightInfo: WeightInfo;
	}

	/// Basic information about a collation candidate.
	#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug, scale_info::TypeInfo)]
	pub struct CandidateInfo<AccountId, Balance> {
		/// Account identifier.
		pub who: AccountId,
		/// Reserved deposit.
		pub deposit: Balance,
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	/// The invulnerable, fixed collators.
	#[pallet::storage]
	#[pallet::getter(fn invulnerables)]
	pub type Invulnerables<T: Config> = StorageValue<_, Vec<T::AccountId>, ValueQuery>;

	/// The (community, limited) collation candidates.
	#[pallet::storage]
	#[pallet::getter(fn candidates)]
	pub type Candidates<T: Config> =
		StorageValue<_, Vec<CandidateInfo<T::AccountId, BalanceOf<T>>>, ValueQuery>;

	// RAD Add collator performance map storage item, compare with Acala
	pub(super) type BlockCount = u32;
	#[pallet::type_value]
	pub(super) fn StartingBlockCount() -> BlockCount {
		0u32.into()
	}
	#[pallet::storage]
	pub(super) type BlocksPerCollatorThisSession<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, BlockCount, ValueQuery, StartingBlockCount>; // RAD: Note: AccountId is user-selectable

	/// Desired number of candidates.
	///
	/// This should ideally always be less than [`Config::MaxCandidates`] for weights to be correct.
	#[pallet::storage]
	#[pallet::getter(fn desired_candidates)]
	pub type DesiredCandidates<T> = StorageValue<_, u32, ValueQuery>;

	/// Fixed deposit bond for each candidate.
	#[pallet::storage]
	#[pallet::getter(fn candidacy_bond)]
	pub type CandidacyBond<T> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub invulnerables: Vec<T::AccountId>,
		pub candidacy_bond: BalanceOf<T>,
		pub desired_candidates: u32,
	}

	#[cfg(feature = "std")]
	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self {
				invulnerables: Default::default(),
				candidacy_bond: Default::default(),
				desired_candidates: Default::default(),
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
		fn build(&self) {
			let duplicate_invulnerables = self
				.invulnerables
				.iter()
				.collect::<std::collections::BTreeSet<_>>();
			assert!(
				duplicate_invulnerables.len() == self.invulnerables.len(),
				"duplicate invulnerables in genesis."
			);

			assert!(
				T::MaxInvulnerables::get() >= (self.invulnerables.len() as u32),
				"genesis invulnerables are more than T::MaxInvulnerables",
			);
			assert!(
				T::MaxCandidates::get() >= self.desired_candidates,
				"genesis desired_candidates are more than T::MaxCandidates",
			);
			// assert!(// RAD: Is there a way to make this check?
			// 	T::Period > BlockCount::MAX,
			// 	"there are more blocks per session than fit into BlockCount value type, increase size",
			// );
			assert!(
				T::PerformancePercentileToConsiderForKick::get() < 100,
				"Percentile must be given as number between 0 and 100",
			);
			assert!(
				T::UnderperformPercentileByPercentToKick::get() <= 100,
				"Kicking threshold must be given as number between 0 and 100",
			);
			<DesiredCandidates<T>>::put(&self.desired_candidates);
			<CandidacyBond<T>>::put(&self.candidacy_bond);
			<Invulnerables<T>>::put(&self.invulnerables);
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		NewInvulnerables(Vec<T::AccountId>),
		NewDesiredCandidates(u32),
		NewCandidacyBond(BalanceOf<T>),
		CandidateAdded(T::AccountId, BalanceOf<T>),
		CandidateRemoved(T::AccountId),
	}

	// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		/// Too many candidates
		TooManyCandidates,
		/// Unknown error
		Unknown,
		/// Permission issue
		Permission,
		/// User is already a candidate
		AlreadyCandidate,
		/// User is not a candidate
		NotCandidate,
		/// User is already an Invulnerable
		AlreadyInvulnerable,
		/// Account has no associated validator ID
		NoAssociatedValidatorId,
		/// Validator ID is not yet registered
		ValidatorNotRegistered,
		/// Removing invulnerable collators is not allowed
		NotAllowRemoveInvulnerable,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Set candidate collator as invulnerable.
		///
		/// `new`: candidate collator.
		#[pallet::weight(T::WeightInfo::set_invulnerables(new.len() as u32))]
		pub fn set_invulnerables(
			origin: OriginFor<T>,
			new: Vec<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			T::UpdateOrigin::ensure_origin(origin)?;
			// we trust origin calls, this is just a for more accurate benchmarking
			if (new.len() as u32) > T::MaxInvulnerables::get() {
				log::warn!(
					"invulnerables > T::MaxInvulnerables; you might need to run benchmarks again"
				);
			}
			<Invulnerables<T>>::put(&new);
			Self::deposit_event(Event::NewInvulnerables(new));
			Ok(().into())
		}

		/// Set how many candidate collator are allowed.
		///
		/// `max`: The max number of candidates.
		#[pallet::weight(T::WeightInfo::set_desired_candidates())]
		pub fn set_desired_candidates(
			origin: OriginFor<T>,
			max: u32,
		) -> DispatchResultWithPostInfo {
			T::UpdateOrigin::ensure_origin(origin)?;
			// we trust origin calls, this is just a for more accurate benchmarking
			if max > T::MaxCandidates::get() {
				log::warn!("max > T::MaxCandidates; you might need to run benchmarks again");
			}
			<DesiredCandidates<T>>::put(&max);
			Self::deposit_event(Event::NewDesiredCandidates(max));
			Ok(().into())
		}

		/// Set the amount held on reserved for candidate collator.
		///
		/// `bond`: The amount held on reserved.
		#[pallet::weight(T::WeightInfo::set_candidacy_bond())]
		pub fn set_candidacy_bond(
			origin: OriginFor<T>,
			bond: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			T::UpdateOrigin::ensure_origin(origin)?;
			<CandidacyBond<T>>::put(&bond);
			Self::deposit_event(Event::NewCandidacyBond(bond));
			Ok(().into())
		}

		/// Register as candidate collator.
		#[pallet::weight(T::WeightInfo::register_as_candidate(T::MaxCandidates::get()))]
		pub fn register_as_candidate(origin: OriginFor<T>) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			// ensure we are below limit.
			let length = <Candidates<T>>::decode_len().unwrap_or_default();
			ensure!(
				(length as u32) < Self::desired_candidates(),
				Error::<T>::TooManyCandidates
			);
			ensure!(
				!Self::invulnerables().contains(&who),
				Error::<T>::AlreadyInvulnerable
			);

			let validator_key = T::ValidatorIdOf::convert(who.clone())
				.ok_or(Error::<T>::NoAssociatedValidatorId)?;
			ensure!(
				T::ValidatorRegistration::is_registered(&validator_key),
				Error::<T>::ValidatorNotRegistered
			);

			let deposit = Self::candidacy_bond();
			// First authored block is current block plus kick threshold to handle session delay
			let incoming = CandidateInfo {
				who: who.clone(),
				deposit,
			};

			let current_count =
				<Candidates<T>>::try_mutate(|candidates| -> Result<usize, DispatchError> {
					if candidates.iter_mut().any(|candidate| candidate.who == who) {
						Err(Error::<T>::AlreadyCandidate.into())
					} else {
						T::Currency::reserve(&who, deposit)?;
						candidates.push(incoming);
						// <BlocksPerCollatorThisSession<T>>::insert(who.clone(), 0u32); // TODO: This must happen when the candidate becomes active as a collator, not here
						Ok(candidates.len())
					}
				})?;

			Self::deposit_event(Event::CandidateAdded(who, deposit));
			Ok(Some(T::WeightInfo::register_as_candidate(current_count as u32)).into())
		}

		/// Register an specified candidate as collator.
		///
		/// - `new_candidate`: Who is going to be collator.
		#[pallet::weight(T::WeightInfo::register_candidate(T::MaxCandidates::get()))]
		pub fn register_candidate(
			origin: OriginFor<T>,
			new_candidate: T::AccountId,
		) -> DispatchResultWithPostInfo {
			T::UpdateOrigin::ensure_origin(origin)?;

			// ensure we are below limit.
			let length = <Candidates<T>>::decode_len().unwrap_or_default();
			ensure!(
				(length as u32) < Self::desired_candidates(),
				Error::<T>::TooManyCandidates
			);
			ensure!(
				!Self::invulnerables().contains(&new_candidate),
				Error::<T>::AlreadyInvulnerable
			);

			let validator_key = T::ValidatorIdOf::convert(new_candidate.clone())
				.ok_or(Error::<T>::NoAssociatedValidatorId)?;
			ensure!(
				T::ValidatorRegistration::is_registered(&validator_key),
				Error::<T>::ValidatorNotRegistered
			);

			let deposit = Self::candidacy_bond();
			// First authored block is current block plus kick threshold to handle session delay
			let incoming = CandidateInfo {
				who: new_candidate.clone(),
				deposit,
			};

			let current_count =
				<Candidates<T>>::try_mutate(|candidates| -> Result<usize, DispatchError> {
					if candidates
						.iter_mut()
						.any(|candidate| candidate.who == new_candidate)
					{
						Err(Error::<T>::AlreadyCandidate.into())
					} else {
						T::Currency::reserve(&new_candidate, deposit)?;
						candidates.push(incoming);
						// <BlocksPerCollatorThisSession<T>>::insert(new_candidate.clone(), 0u32);
						Ok(candidates.len())
					}
				})?;

			Self::deposit_event(Event::CandidateAdded(new_candidate, deposit));
			Ok(Some(T::WeightInfo::register_candidate(current_count as u32)).into())
		}

		/// Leave from collator set.
		#[pallet::weight(T::WeightInfo::leave_intent(T::MaxCandidates::get()))]
		pub fn leave_intent(origin: OriginFor<T>) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let current_count = Self::try_remove_candidate(&who)?;

			Ok(Some(T::WeightInfo::leave_intent(current_count as u32)).into())
		}

		/// Remove an specified collator.
		///
		/// - `collator`: Who is going to be remove from collators set.
		#[pallet::weight(T::WeightInfo::remove_collator(T::MaxCandidates::get()))]
		pub fn remove_collator(
			origin: OriginFor<T>,
			collator: T::AccountId,
		) -> DispatchResultWithPostInfo {
			T::UpdateOrigin::ensure_origin(origin)?;

			// not allow to remove invulnerables
			ensure!(
				!<Invulnerables<T>>::get().contains(&collator),
				Error::<T>::NotAllowRemoveInvulnerable
			);

			let current_count = Self::try_remove_candidate(&collator)?;

			Ok(Some(T::WeightInfo::remove_collator(current_count as u32)).into())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Get a unique, inaccessible account id from the `PotId`.
		pub fn account_id() -> T::AccountId {
			T::PotId::get().into_account()
		}

		/// Removes a candidate if they exist and sends them back their deposit
		fn try_remove_candidate(who: &T::AccountId) -> Result<usize, DispatchError> {
			let current_count =
				<Candidates<T>>::try_mutate(|candidates| -> Result<usize, DispatchError> {
					let index = candidates
						.iter()
						.position(|candidate| candidate.who == *who)
						.ok_or(Error::<T>::NotCandidate)?;
					T::Currency::unreserve(who, candidates[index].deposit);
					candidates.remove(index);
					<BlocksPerCollatorThisSession<T>>::remove(who.clone());
					Ok(candidates.len())
				})?;
			Self::deposit_event(Event::CandidateRemoved(who.clone()));
			Ok(current_count)
		}

		/// Assemble the current set of candidates and invulnerables into the next collator set.
		///
		/// This is done on the fly, as frequent as we are told to do so, as the session manager.
		pub fn assemble_collators(candidates: Vec<T::AccountId>) -> Vec<T::AccountId> {
			let mut collators = Self::invulnerables();
			collators.extend(candidates.into_iter().collect::<Vec<_>>());
			collators
		}

		/// Removes collators with unsatisfactory performance
		/// Returns the removed AccountIds
		pub fn kick_stale_candidates() -> Vec<T::AccountId> {
			// 0. TODO: All sanity checks
			let mut collator_perf_this_session =
				<BlocksPerCollatorThisSession<T>>::iter().collect::<Vec<_>>();
			if collator_perf_this_session.is_empty() {
				return Vec::new();
			}
			// 1. Sort collator performance list
			collator_perf_this_session.sort_unstable_by_key(|k| k.1); // XXX: don't like the tuple accessor, could this be a struct?
														  // collator_perf_this_session.reverse();
			let no_of_candidates = collator_perf_this_session.len();

			// 2. get percentile by _exclusive_ nearest rank method https://en.wikipedia.org/wiki/Percentile#The_nearest-rank_method (rust percentile API is feature gated)
			let ordinal_rank = (((T::PerformancePercentileToConsiderForKick::get() as f64) / 100.0
				* no_of_candidates as f64) as usize)
				.saturating_sub(1); // Note: -1 to accomodate 0-index counting
					// 3. Block number at rank is the percentile and our kick performance benchmark
			let blocks_created_at_percentile: BlockCount =
				collator_perf_this_session[ordinal_rank].1; // XXX: don't like the tuple accessor, could this be a struct?
											// 4. We kick if a collator produced UnderperformPercentileByPercentToKick fewer blocks than the percentile
			let threshold_factor =
				1.0 - T::UnderperformPercentileByPercentToKick::get() as f64 / 100.0;
			let kick_threshold =
				(threshold_factor * (blocks_created_at_percentile as f64)) as BlockCount;
			log::info!("Session Performance stats: {}-th percentile: {blocks_created_at_percentile} blocks\nWill kick under {kick_threshold} blocks",T::PerformancePercentileToConsiderForKick::get());

			// 5. Walk the percentile slice, call try_remove_candidate if a collator is under threshold
			let mut removed_account_ids: Vec<T::AccountId> = Vec::new();
			let kick_candidates = collator_perf_this_session[..ordinal_rank] // ordinal-rank exclusive, the collator with percentile perf is safe
				.iter()
				.map(|acc_info| acc_info.0.clone())
				.collect::<Vec<_>>();
			kick_candidates.into_iter().for_each(|acc_id| {
				let my_blocks_this_session = <BlocksPerCollatorThisSession<T>>::get(&acc_id); // RAD: read storage or find in collator_perf_this_session vec
				if my_blocks_this_session <= kick_threshold {
					if !Self::invulnerables().contains(&acc_id) {
						Self::try_remove_candidate(&acc_id)
							.and_then(|_| {
								removed_account_ids.push(acc_id.clone());
								Ok(())
							})
							.unwrap_or_else(|why| -> () {
								log::warn!("Failed to remove candidate {:?}", why);
								debug_assert!(false, "failed to remove candidate {:?}", why);
							});
					}
				}
			});
			removed_account_ids
		}
		pub fn reset_collator_performance() {
			// FIXME: 0 the map and add new collators or drop and recreate from scratch?
			<BlocksPerCollatorThisSession<T>>::remove_all(None);
			let validators = T::ValidatorRegistration::validators();
			// for v in validators {
			// 	if !<BlocksPerCollatorThisSession<T>>::contains_key(v) {
			// 		<BlocksPerCollatorThisSession<T>>::insert((v as T::AccountId).clone(), 0u32);
			// 	}
			// }
			// RAD: Does this need a call to register_extra_weight too?
		}
	}

	/// Keep track of number of authored blocks per authority, uncles are counted as well since
	/// they're a valid proof of being online.
	impl<T: Config + pallet_authorship::Config>
		pallet_authorship::EventHandler<T::AccountId, T::BlockNumber> for Pallet<T>
	{
		fn note_author(author: T::AccountId) {
			let pot = Self::account_id();
			// assumes an ED will be sent to pot.
			let reward = T::Currency::free_balance(&pot)
				.checked_sub(&T::Currency::minimum_balance())
				.unwrap_or_else(Zero::zero)
				.div(2u32.into());
			// `reward` is half of pot account minus ED, this should never fail.
			let _success = T::Currency::transfer(&pot, &author, reward, KeepAlive);
			debug_assert!(_success.is_ok());

			// increment blocks this node authored // RAD: Do sanity checks
			let mut authored_blocks = <BlocksPerCollatorThisSession<T>>::get(&author);
			// 	.ok_or(Error::<T>::NotCandidate)?;
			authored_blocks = authored_blocks.saturating_add(1u32);
			<BlocksPerCollatorThisSession<T>>::insert(&author, authored_blocks);

			frame_system::Pallet::<T>::register_extra_weight_unchecked(
				T::WeightInfo::note_author(),
				DispatchClass::Mandatory,
			);
		}

		fn note_uncle(_author: T::AccountId, _age: T::BlockNumber) {
			// RAD: Can't really have uncles in a PoA round-robin (Aura) system
			//TODO can we ignore this?
		}
	}

	/// Play the role of the session manager.
	impl<T: Config> SessionManager<T::AccountId> for Pallet<T> {
		fn new_session(index: SessionIndex) -> Option<Vec<T::AccountId>> {
			log::info!(
				"assembling new collators for new session {} at #{:?}",
				index,
				<frame_system::Pallet<T>>::block_number(),
			);

			let candidates = Self::candidates();
			let candidates_len_before = candidates.len();
			let removed_collators = Self::kick_stale_candidates();
			let active_candidates = candidates // XXX: This could mutate candidates in place
				.iter()
				.filter_map(|x| {
					if removed_collators.contains(&x.who) {
						None
					} else {
						Some(x.who.clone())
					}
				})
				.collect();
			let result = Self::assemble_collators(active_candidates);

			frame_system::Pallet::<T>::register_extra_weight_unchecked(
				T::WeightInfo::new_session(
					candidates_len_before as u32,
					removed_collators.len() as u32,
				),
				DispatchClass::Mandatory,
			);
			Some(result)
		}
		fn start_session(_: SessionIndex) {
			// Reset collator block counts to 0
			Self::reset_collator_performance();
		}
		fn end_session(_: SessionIndex) {
			// we don't care.
		}
	}
}
