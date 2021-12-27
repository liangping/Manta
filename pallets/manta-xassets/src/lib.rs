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

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{
	dispatch::DispatchResult,
	pallet_prelude::*,
	traits::{Currency, Get, Hooks, IsType, ReservableCurrency},
	PalletId,
};
use frame_system::{
	ensure_signed,
	pallet_prelude::{BlockNumberFor, OriginFor},
};
use manta_primitives::{
	currency_id::{CurrencyId, TokenSymbol},
	traits::XCurrency,
	ParaId,
};
use sp_runtime::SaturatedConversion;
use sp_runtime::traits::{AccountIdConversion, Convert};
use sp_std::vec;
use xcm::{
	v1::{
		AssetId, Fungibility, Junction, Junctions, MultiAsset, MultiAssetFilter, MultiAssets,
		MultiLocation, WildMultiAsset,
	},
	v2::{ExecuteXcm, Instruction, Outcome, WeightLimit, Xcm as XcmV2, NetworkId},
};
use xcm_executor::traits::WeightBounds;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;
// Log filter
const MANTA_XASSETS: &str = "manta-xassets";
pub type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	/// The module configuration trait.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Something to execute an XCM message.
		type XcmExecutor: ExecuteXcm<Self::Call>;

		/// Convert AccountId to MultiLocation.
		type Conversion: Convert<Self::AccountId, MultiLocation>;

		/// This pallet id.
		type PalletId: Get<PalletId>;

		type Currency: ReservableCurrency<Self::AccountId>;

		/// Manta's parachain id.
		type SelfParaId: Get<ParaId>;

		/// Means of measuring the weight consumed by an XCM message locally.
		type Weigher: WeightBounds<Self::Call>;
	}

	// This is an workaround for depositing/withdrawing cross chain tokens
	// Finally, we'll utilize pallet-assets to handle these external tokens.
	#[pallet::storage]
	#[pallet::getter(fn xtokens)]
	pub type XTokens<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		CurrencyId,
		Blake2_128Concat,
		T::AccountId,
		BalanceOf<T>,
		ValueQuery,
	>;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	#[pallet::generate_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		Attempted(Outcome),
		/// Deposit success. [asset, to]
		Deposited(T::AccountId, CurrencyId, BalanceOf<T>),
		/// Withdraw success. [asset, from]
		Withdrawn(T::AccountId, CurrencyId, BalanceOf<T>),
	}

	#[pallet::error]
	pub enum Error<T> {
		BalanceLow,
		SelfChain,
		BadAccountIdToMultiLocation,
		UnweighableMessage,
		NotSupportedToken,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Transfer manta tokens to sibling parachain.
		///
		/// - `origin`: Must be capable of withdrawing the `assets` and executing XCM.
		/// - `para_id`: Sibling parachain id.
		/// - `dest`: Who will receive foreign tokens on sibling parachain.
		/// - `amount`: How many tokens will be transferred.
		/// - `weight`: Specify the weight of xcm.
		#[pallet::weight(10000)]
		pub fn transfer_to_parachain(
			origin: OriginFor<T>,
			#[pallet::compact] para_id: ParaId,
			dest: T::AccountId,
			currency_id: CurrencyId,
			#[pallet::compact] amount: BalanceOf<T>,
		) -> DispatchResult {
			let from = ensure_signed(origin)?;

			ensure!(T::SelfParaId::get() != para_id, Error::<T>::SelfChain);

			match currency_id {
				CurrencyId::Token(TokenSymbol::MANTA) | CurrencyId::Token(TokenSymbol::KMA) => {
					ensure!(
						T::Currency::free_balance(&from) >= amount,
						Error::<T>::BalanceLow
					);
				}
				CurrencyId::Token(TokenSymbol::ACA)
				| CurrencyId::Token(TokenSymbol::KAR)
				| CurrencyId::Token(TokenSymbol::SDN) => {
					ensure!(
						Self::account(currency_id, &from) >= amount,
						Error::<T>::BalanceLow
					);
				}
				_ => return Err(Error::<T>::NotSupportedToken.into()),
			}

			let xcm_origin = T::Conversion::convert(from);

			// create sibling parachain target
			let xcm_target = T::Conversion::convert(dest);

			let dest_junc = Junctions::X1(Junction::Parachain(para_id.into()));
			let destination = MultiLocation {
				parents: 1, // must be 1, no idea, will figure it out
				interior: dest_junc
			};

			let amount = amount.saturated_into::<u128>();
			let para_id = para_id.saturated_into::<u32>();

			let fungibility = Fungibility::Fungible(amount);
			let junctions = Junctions::X2(
				Junction::Parachain(para_id),
				Junction::GeneralKey(currency_id.encode()),
			);
			let multi_location = MultiLocation::new(1, junctions);
			let asset_id = AssetId::Concrete(multi_location.clone());
			let multi_asset = MultiAsset {
				id: asset_id,
				fun: fungibility,
			};
			// Todo, handle weight_limit
			let mut beneficiary = xcm_target;
			beneficiary.parents = 1;

			let mut xcm = XcmV2(vec![
				Instruction::WithdrawAsset(MultiAssets::from(vec![multi_asset.clone()])),
				Instruction::DepositReserveAsset {
					assets: MultiAssetFilter::Wild(WildMultiAsset::All),
					max_assets: 1,
					dest: destination.into(),
					xcm: XcmV2(vec![
						Instruction::BuyExecution {
							fees: multi_asset,
							weight_limit: WeightLimit::Limited(100_000_000_000),
						},
						Instruction::DepositAsset {
							assets: MultiAssetFilter::Wild(WildMultiAsset::All),
							max_assets: 1,
							beneficiary,
						},
					]),
				},
			]);

			log::info!(target: MANTA_XASSETS, "xcm = {:?}", xcm);

			let xcm_weight =
				T::Weigher::weight(&mut xcm).map_err(|()| Error::<T>::UnweighableMessage)?;

			// The last param is the weight we buy on target chain.
			let outcome =
				T::XcmExecutor::execute_xcm_in_credit(xcm_origin, xcm, xcm_weight, xcm_weight);
			log::info!(target: MANTA_XASSETS, "xcm_outcome = {:?}", outcome);

			Self::deposit_event(Event::Attempted(outcome));

			Ok(())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub place_holder: PhantomData<T>,
	}

	#[cfg(feature = "std")]
	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> GenesisConfig<T> {
			Self {
				place_holder: PhantomData,
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
		fn build(&self) {}
	}

	impl<T: Config> XCurrency<T::AccountId> for Pallet<T> {
		type Balance = BalanceOf<T>;
		type CurrencyId = CurrencyId;

		fn account(currency_id: Self::CurrencyId, who: &T::AccountId) -> Self::Balance {
			XTokens::<T>::get(currency_id, who)
		}

		/// Add `amount` to the balance of `who` under `currency_id`
		fn deposit(
			currency_id: Self::CurrencyId,
			who: &T::AccountId,
			amount: Self::Balance,
		) -> DispatchResult {
			XTokens::<T>::mutate(currency_id, who, |balance| {
				// *balance = balance.saturated_add(amount);
				*balance += amount;
			});

			Self::deposit_event(Event::Deposited(who.clone(), currency_id, amount));

			Ok(())
		}

		/// Remove `amount` from the balance of `who` under `currency_id`
		fn withdraw(
			currency_id: Self::CurrencyId,
			who: &T::AccountId,
			amount: Self::Balance,
		) -> DispatchResult {
			XTokens::<T>::mutate(currency_id, who, |balance| {
				// *balance = balance.saturated_add(amount);
				*balance -= amount;
			});

			Self::deposit_event(Event::Withdrawn(who.clone(), currency_id, amount));

			Ok(())
		}
	}
}