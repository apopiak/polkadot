// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Parathread and parachains leasing system. Allows para IDs to be claimed, the code and data to be initialized and
//! parachain slots (i.e. continuous scheduling) to be leased. Also allows for parachains and parathreads to be
//! swapped.
//!
//! This doesn't handle the mechanics of determining which para ID actually ends up with a parachain lease. This
//! must handled by a separately, through the trait interface that this pallet provides or the root dispatchables.

use sp_std::{prelude::*, mem::swap, convert::TryInto};
use sp_runtime::traits::{
	CheckedSub, StaticLookup, Zero, One, CheckedConversion, Hash, AccountIdConversion,
};
use parity_scale_codec::{Encode, Decode, Codec};
use frame_support::{
	decl_module, decl_storage, decl_event, decl_error, ensure, dispatch::DispatchResult,
	traits::{Currency, ReservableCurrency, WithdrawReasons, ExistenceRequirement, Get, Randomness},
	weights::{DispatchClass, Weight},
};
use primitives::v1::{
	Id as ParaId, ValidationCode, HeadData,
};
use frame_system::{ensure_signed, ensure_root};
use crate::slot_range::{SlotRange, SLOT_RANGE_COUNT};

type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// The module's configuration trait.
pub trait Config: frame_system::Config {
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;

	/// The currency type used for bidding.
	type Currency: ReservableCurrency<Self::AccountId>;

	/// The amount required for a basic parathread deposit.
	type ParaDeposit: Get<BalanceOf<Self>>;

	/// The parachain registrar type.
	type Parachains: Registrar<Self::AccountId>;

	/// The number of blocks over which a single period lasts.
	type LeasePeriod: Get<Self::BlockNumber>;
}

/// Parachain registration API.
pub trait Registrar<AccountId> {
	/// Create a new unique parachain identity for later registration.
	fn new_id() -> ParaId;

	/// Checks whether the given initial head data size falls within the limit.
	fn head_data_size_allowed(head_data_size: u32) -> bool;

	/// Checks whether the given validation code falls within the limit.
	fn code_size_allowed(code_size: u32) -> bool;

	/// Register a parachain with given `code` and `initial_head_data`. `id` must not yet be registered or it will
	/// result in a error.
	///
	/// This does not enforce any code size or initial head data limits, as these
	/// are governable and parameters for parachain initialization are often
	/// determined long ahead-of-time. Not checking these values ensures that changes to limits
	/// do not invalidate in-progress auction winners.
	fn register_para(
		id: ParaId,
		_parachain: bool,
		code: ValidationCode,
		initial_head_data: HeadData,
	) -> DispatchResult;

	/// Deregister a parachain with given `id`. If `id` is not currently registered, an error is returned.
	fn deregister_para(id: ParaId) -> DispatchResult;
}

/// Lease manager. Used by the auction module to handle parachain slot leases.
pub trait Leaser {
	type AccountId;

	type LeasePeriod;

	type Currency: ReservableCurrency<Self::AccountId>;

	/// Lease a new parachain slot for `para`.
	///
	/// `leaser` shall have a total of `amount` balance reserved by the implementor of this trait.
	///
	/// Note: The implementor of the trait (the leasing system) is expected to do all reserve/unreserve calls. The
	/// caller of this trait *SHOULD NOT* pre-reserve the deposit (though should ensure that it is reservable).
	///
	/// The lease will last from `period_begin` for `period_count` lease periods. It is undefined if the `para`
	/// already has a slot leased during those periods.
	///
	/// Returns `Err` if the `amount` was unable to be reserved.
	fn lease_out(
		para: ParaId,
		leaser: Self::AccountId,
		amount: <Self::Currency as Currency<Self::AccountId>>::Balance,
		period_begin: Self::LeasePeriod,
		period_count: Self::LeasePeriod,
	) -> DispatchResult;

	/// Return the amount of balance currently held in reserve on `leaser`'s account for leasing `para`. This won't
	/// go down outside of a lease period.
	fn deposit_held(para: ParaId, leaser: Self::AccountId) -> <Self::Currency as Currency<Self::AccountId>>::Balance;

	/// The lease period. This is constant, but can't be a `const` due to it being a runtime configurable quantity.
	fn lease_period() -> Self::LeasePeriod;

	/// Returns the current lease period.
	fn lease_period_index() -> Self::LeasePeriod;
}

/// Auxilliary for when there's an attempt to swap two parachains/parathreads.
pub trait SwapAux {
	/// Result describing whether it is possible to swap two parachains. Doesn't mutate state.
	fn ensure_can_swap(one: ParaId, other: ParaId) -> Result<(), &'static str>;

	/// Updates any needed state/references to enact a logical swap of two parachains. Identity,
	/// code and `head_data` remain equivalent for all parachains/threads, however other properties
	/// such as leases, deposits held and thread/chain nature are swapped.
	///
	/// May only be called on a state that `ensure_can_swap` has previously returned `Ok` for: if this is
	/// not the case, the result is undefined. May only return an error if `ensure_can_swap` also returns
	/// an error.
	fn on_swap(one: ParaId, other: ParaId) -> Result<(), &'static str>;
}

/// A bidder identifier, which is just the combination of an account ID and a sub-bidder ID.
/// This is called `NewBidder` in order to distinguish between bidders that would deploy a *new*
/// parachain and pre-existing parachains bidding to renew themselves.
#[derive(Clone, Eq, PartialEq, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct NewBidder<AccountId> {
	/// The bidder's account ID; this is the account that funds the bid.
	pub who: AccountId,
	/// An additional ID to allow the same account ID (and funding source) to have multiple
	/// logical bidders.
	pub sub: SubId,
}

/// The desired target of a bidder in an auction.
#[derive(Clone, Eq, PartialEq, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum Bidder<AccountId> {
	/// An account ID, funds coming from that account.
	New(NewBidder<AccountId>),

	/// An existing parachain, funds coming from the amount locked as part of a previous bid topped
	/// up with funds administered by the parachain.
	Existing(ParaId),
}

impl<AccountId: Clone + Default + Codec> Bidder<AccountId> {
	/// Get the account that will fund this bid.
	fn funding_account(&self) -> AccountId {
		match self {
			Bidder::New(new_bidder) => new_bidder.who.clone(),
			Bidder::Existing(para_id) => para_id.into_account(),
		}
	}
}

/// Information regarding a parachain that will be deployed.
///
/// We store either the bidder that will be able to set the final deployment information or the
/// information itself.
#[derive(Clone, Eq, PartialEq, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum IncomingParachain<AccountId, Hash> {
	/// Deploy information not yet set; just the bidder identity.
	Unset(NewBidder<AccountId>),
	/// Deploy information set only by code hash; so we store the code hash, code size, and head data.
	///
	/// The code size must be included so that checks against a maximum code size
	/// can be done. If the size of the preimage of the code hash does not match
	/// the given code size, it will not be possible to register the parachain.
	Fixed { code_hash: Hash, code_size: u32, initial_head_data: HeadData },
	/// Deploy information fully set; so we store the code and head data.
	Deploy { code: ValidationCode, initial_head_data: HeadData },
}

type LeasePeriodOf<T> = <T as frame_system::Config>::BlockNumber;
// Winning data type. This encodes the top bidders of each range together with their bid.
type WinningData<T> =
	[Option<(Bidder<<T as frame_system::Config>::AccountId>, BalanceOf<T>)>; SLOT_RANGE_COUNT];
// Winners data type. This encodes each of the final winners of a parachain auction, the parachain
// index assigned to them, their winning bid and the range that they won.
type WinnersData<T> =
	Vec<(Option<NewBidder<<T as frame_system::Config>::AccountId>>, ParaId, BalanceOf<T>, SlotRange)>;

/// The funding mode for the bid; either a continuation of a previous funder or a completely new funder.
#[derive(Clone, Eq, PartialEq, Encode, Decode, RuntimeDebug)]
pub enum Funding<AccountId> {
	/// Indicates a bid was made by the same source as the slot winner immediately prior, and thus is a continuation.
	/// The same reserved funds are used between the bids, so once the previous slot ends, the funds reserved for it
	/// will not be (entirely) unreserved.
	Continuation,
	/// Indicates a bid is made by a different source as immediately prior, and different. At the end of the previous
	/// slot, the reserved funds may be freed. The full bid will have been reserved from the funds of the given account
	/// ID.
	New(AccountId),
}

// This module's storage items.
decl_storage! {
	trait Store for Module<T: Config> as Slots {
		/// Ordered list of all `ParaId` values that are managed by this module. This includes
		/// chains that are not yet deployed (but have won an auction in the future).
		pub ManagedIds get(fn managed_ids): Vec<ParaId>;

		/// Amount held on deposit for each para and the original depositor. Any items here should also
		/// be in `ManagedIds`.
		/// The given account ID is responsible for registering the code and initial head data, but may only do
		/// so if it isn't yet registered. (After that, it's up to governance to do so.)
		Paras: ParaId => Option<(BalanceOf<T>, T::AccountId)>;

		/// Various amounts on deposit for each parachain. An entry in `ManagedIds` implies a non-
		/// default entry here.
		///
		/// The actual amount locked on its behalf at any time is the maximum item in this list. The
		/// first item in the list is the amount locked for the current Lease Period. Following
		/// items are for the subsequent lease periods.
		///
		/// The default value (an empty list) implies that the parachain no longer exists (or never
		/// existed) as far as this module is concerned.
		///
		/// If a parachain doesn't exist *yet* but is scheduled to exist in the future, then it
		/// will be left-padded with one or more zeroes to denote the fact that nothing is held on
		/// deposit for the non-existent chain currently, but is held at some point in the future.
		///
		/// Each entry represents a (depositor, deposit) pair. If it's `None` for a particular lease
		/// period, then the para isn't a parachain during that period.
		pub Deposits get(fn deposits): map hasher(twox_64_concat) ParaId => Vec<Option(T::AccountId, BalanceOf<T>)>;

		/// The set of Para IDs that should be full parachains for the given lease period.
		pub Leased: map hasher(twox_64_concat) LeasePeriodOf<T> => Vec<ParaId>;
	}
}

decl_event!(
	pub enum Event<T> where
		AccountId = <T as frame_system::Config>::AccountId,
		BlockNumber = <T as frame_system::Config>::BlockNumber,
		LeasePeriod = LeasePeriodOf<T>,
		ParaId = ParaId,
		Balance = BalanceOf<T>,
	{
		/// A new [lease_period] is beginning.
		NewLeasePeriod(LeasePeriod),
		/// Someone won the right to deploy a parachain. Balance amount is deducted for deposit.
		/// [bidder, range, parachain_id, amount]
		WonDeploy(NewBidder<AccountId>, SlotRange, ParaId, Balance),
		/// An existing parachain won the right to continue.
		/// First balance is the extra amount reseved. Second is the total amount reserved.
		/// [parachain_id, range, extra_reseved, total_amount]
		WonRenewal(ParaId, SlotRange, Balance, Balance),
		/// Funds were reserved for a winning bid. First balance is the extra amount reserved.
		/// Second is the total. [bidder, extra_reserved, total_amount]
		Reserved(AccountId, Balance, Balance),
		/// Funds were unreserved since bidder is no longer active. [bidder, amount]
		Unreserved(AccountId, Balance),
	}
);

decl_error! {
	pub enum Error for Module<T: Config> {
		/// The lease period is in the past.
		LeasePeriodInPast,
		/// The origin for this call must be a parachain.
		NotParaOrigin,
		/// The parachain ID is not onboarding.
		ParaNotOnboarding,
		/// The origin for this call must be the origin who registered the parachain.
		InvalidOrigin,
		/// Parachain is already registered.
		AlreadyRegistered,
		/// The code must correspond to the hash.
		InvalidCode,
		/// Deployment data has not been set for this parachain.
		UnsetDeployData,
		/// The bid must overlap all intersecting ranges.
		NonIntersectingRange,
		/// Given code size is too large.
		CodeTooLarge,
		/// Given initial head data is too large.
		HeadDataTooLarge,
	}
}

decl_module! {
	pub struct Module<T: Config> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		fn deposit_event() = default;

		fn on_initialize(n: T::BlockNumber) -> Weight {
			let lease_period = T::LeasePeriod::get();
			let lease_period_index: LeasePeriodOf<T> = (n / lease_period).into();

			// If we're beginning a new lease period then handle that, too.
			if (n % lease_period).is_zero() {
				Self::manage_lease_period_start(lease_period_index);
			}

			0
		}

		#[weight = 0]
		fn register(origin, id: ParaId) -> DispatchResult {
			// TODO: ...
			ManagedIds::mutate(|ids|
				if let Err(pos) = ids.binary_search(&para_id) {
					ids.insert(pos, para_id)
				} else {
					// This can't happen as it's a winner being newly
					// deployed and thus the para_id shouldn't already be being managed.
				}
			);
			Ok(())
		}

		#[weight = 0]
		fn unregister(origin, id: ParaId) {
			// TODO
		}

		/// Set the deploy information for a successful bid to deploy a new parachain.
		///
		/// - `origin` must be the successful bidder account.
		/// - `sub` is the sub-bidder ID of the bidder.
		/// - `para_id` is the parachain ID allotted to the winning bidder.
		/// - `code_hash` is the hash of the parachain's Wasm validation function.
		/// - `initial_head_data` is the parachain's initial head data.
		///
		/// Use in concert with `elaborate_deploy_data`. The two are different functions to allow for parachains
		/// sending this dispatch (which should be quite small) and a third-party sending the full code to the
		/// relay-chain directly.
		#[weight = 500_000_000]
		pub fn fix_deploy_data(origin,
			#[compact] para_id: ParaId,
			code_hash: T::Hash,
			code_size: u32,
			initial_head_data: HeadData,
		) {
			let who = ensure_signed(origin)?;
			let (starts, details) = <Onboarding<T>>::get(&para_id)
				.ok_or(Error::<T>::ParaNotOnboarding)?;
			if let IncomingParachain::Unset(ref nb) = details {
				ensure!(nb.who == who && nb.sub == sub, Error::<T>::InvalidOrigin);
			} else {
				Err(Error::<T>::AlreadyRegistered)?
			}

			ensure!(
				T::Parachains::head_data_size_allowed(initial_head_data.0.len() as _),
				Error::<T>::HeadDataTooLarge,
			);
			ensure!(
				T::Parachains::code_size_allowed(code_size),
				Error::<T>::CodeTooLarge,
			);

			let item = (starts, IncomingParachain::Fixed{code_hash, code_size, initial_head_data});
			<Onboarding<T>>::insert(&para_id, item);
		}

		/// Note a new para's code.
		///
		/// This must be called after `fix_deploy_data` and `code` must be the preimage of the
		/// `code_hash` passed there for the same `para_id`.
		///
		/// This may be called before or after the beginning of the parachain's first lease period.
		/// If called before then the parachain will become active at the first block of its
		/// starting lease period. If after, then it will become active immediately after this call.
		///
		/// - `_origin` is irrelevant.
		/// - `para_id` is the parachain ID whose code will be elaborated.
		/// - `code` is the preimage of the registered `code_hash` of `para_id`.
		#[weight = 5_000_000_000]
		pub fn elaborate_deploy_data(
			_origin,
			#[compact] para_id: ParaId,
			code: ValidationCode,
		) -> DispatchResult {
			let (starts, details) = <Onboarding<T>>::get(&para_id)
				.ok_or(Error::<T>::ParaNotOnboarding)?;
			if let IncomingParachain::Fixed{code_hash, code_size, initial_head_data} = details {
				ensure!(code.0.len() as u32 == code_size, Error::<T>::InvalidCode);
				ensure!(<T as frame_system::Config>::Hashing::hash(&code.0) == code_hash, Error::<T>::InvalidCode);

				if starts > Self::lease_period_index() {
					// Hasn't yet begun. Replace the on-boarding entry with the new information.
					let item = (starts, IncomingParachain::Deploy{code, initial_head_data});
					<Onboarding<T>>::insert(&para_id, item);
				} else {
					// Should have already begun. Remove the on-boarding entry and register the
					// parachain for its immediate start.
					<Onboarding<T>>::remove(&para_id);
					let _ = T::Parachains::
						register_para(para_id, true, code, initial_head_data);
				}

				Ok(())
			} else {
				Err(Error::<T>::UnsetDeployData)?
			}
		}
	}
}

impl<T: Config> Module<T> {
	/// A new lease period is beginning. We're at the start of the first block of it.
	///
	/// We need to on-board and off-board parachains as needed. We should also handle reducing/
	/// returning deposits.
	fn manage_lease_period_start(lease_period_index: LeasePeriodOf<T>) {
		Self::deposit_event(RawEvent::NewLeasePeriod(lease_period_index));
		// First, bump off old deposits and decommission any managed chains that are coming
		// to a close.
		ManagedIds::mutate(|ids| {
			let new = ids.drain(..).filter(|id| {
				let mut d = <Deposits<T>>::get(id);
				if !d.is_empty() {
					// ^^ should always be true, since we would have deleted the entry otherwise.

					if d.len() == 1 {
						// Just one entry, which corresponds to the now-ended lease period. Time
						// to decommission this chain.
						if <Onboarding<T>>::take(id).is_none() {
							// Only unregister it if it was actually registered in the first place.
							// If the on-boarding entry still existed, then it was never actually
							// commissioned.
							let _ = T::Parachains::deregister_para(id.clone());
						}
						// Return the full deposit to the off-boarding account.
						T::Currency::deposit_creating(&<Offboarding<T>>::take(id), d[0]);
						// Remove the now-empty deposits set and don't keep the ID around.
						<Deposits<T>>::remove(id);
						false
					} else {
						// The parachain entry is continuing into the next lease period.
						// We need to pop the first deposit entry, which corresponds to the now-
						// ended lease period.
						let outgoing = d[0];
						d.remove(0);
						<Deposits<T>>::insert(id, &d);
						// Then we need to get the new amount that should continue to be held on
						// deposit for the parachain.
						let new_held = d.into_iter().max().unwrap_or_default();
						// If this is less than what we were holding previously, then return it
						// to the parachain itself.
						if let Some(rebate) = outgoing.checked_sub(&new_held) {
							T::Currency::deposit_creating(
								&id.into_account(),
								rebate
							);
						}
						// We keep this entry around until the next lease period.
						true
					}
				} else {
					false
				}
			}).collect::<Vec<_>>();
			*ids = new;
		});

		// Deploy any new chains that are due to be commissioned.
		for para_id in <OnboardQueue<T>>::take(lease_period_index) {
			if let Some((_, IncomingParachain::Deploy{code, initial_head_data}))
				= <Onboarding<T>>::get(&para_id)
			{
				// The chain's deployment data is set; go ahead and register it, and remove the
				// now-redundant on-boarding entry.
				let _ = T::Parachains::
					register_para(para_id.clone(), true, code, initial_head_data);
				// ^^ not much we can do if it fails for some reason.
				<Onboarding<T>>::remove(para_id)
			}
		}
	}
}

impl<T: Config> Leaser for Module<T> {
	type AccountId = T::AccountId;
	type LeasePeriod = T::BlockNumber;
	type Currency = T::Currency;

	fn lease_out(
		para: ParaId,
		leaser: Self::AccountId,
		amount: <Self::Currency as Currency<Self::AccountId>>::Balance,
		period_begin: Self::LeasePeriod,
		period_count: Self::LeasePeriod,
	) -> DispatchResult {

		// Figure out whether we already have some funds of `who` held in reserve for `para_id`.
		//  If so, then we can deduct those from the amount that we need to reserve.
		let maybe_additional = amount.checked_sub(&T::Leaser::deposit_held(para_id, &who));
		if let Some(additional) = maybe_additional {
			T::Currency::reserve(&who, additional)?;
		}

		// Remember that this should be leased.
		for p in period_begin..(period_begin + period_count) {
			// Schedule the para for full parachain status.
			Leased::<T>::mutate(p, |paras| paras.push(para));
		}

		Self::deposit_event(RawEvent::WonRenewal(para_id, range, extra, amount));

		// Finally, we update the deposit held so it is `amount` for the new lease period
		// indices that were won in the auction.
		let maybe_offset = period_begin
			.checked_sub(&Self::lease_period_index())
			.and_then(|x| x.checked_into::<usize>());
		if let Some(offset) = maybe_offset {
			// Should always succeed; for it to fail it would mean we auctioned a lease period
			// that already ended.

			// offset is the amount into the `Deposits` items list that our lease begins. `period_count`
			// is the number of items that it lasts for.

			// The lease period index range (begin, end) that newly belongs to this parachain
			// ID. We need to ensure that it features in `Deposits` to prevent it from being
			// reaped too early (any managed parachain whose `Deposits` set runs low will be
			// removed).
			Deposits::<T>::mutate(para_id, |d| {
				// Left-pad with zeroes as necessary.
				if d.len() < offset {
					d.resize_with(offset, None);
				}
				// Then place the deposit values for as long as the chain should exist.
				for i in offset .. (offset + period_count as usize) {
					if d.len() > i {
						// Already exists but it's `None`. That means a later slot was already leased.
						// No problem.
						if d[i] == None {
							d[i] = Some((leaser.clone(), amount))
						} else {
							// The chain bought the same lease period twice. How dumb. Safe to ignore.
						}
					} else if d.len() == i {
						// Doesn't exist. This is usual.
						d.push(Some((leaser.clone(), amount)));
					} else {
						unreachable!("earlier resize means it must be >= i; qed")
					}
				}
			});
		}
		Ok(())
	}

	fn deposit_held(para: ParaId, leaser: &Self::AccountId) -> <Self::Currency as Currency<Self::AccountId>>::Balance {
		Deposits::<T>::get(para)
			.into_iter()
			.map(|(who, amount)| if &who == leaser { amount } else { Zero::zero() })
			.max()
			.unwrap_or_else(Zero::zero)
	}

	fn lease_period() -> Self::LeasePeriod {
		T::LeasePeriod::get()
	}

	fn lease_period_index() -> Self::LeasePeriod {
		(<frame_system::Module<T>>::block_number() / T::LeasePeriod::get()).into()
	}
}

/// Swap the existence of two items, provided by value, within an ordered list.
///
/// If neither item exists, or if both items exist this will do nothing. If exactly one of the
/// items exists, then it will be removed and the other inserted.
fn swap_ordered_existence<T: PartialOrd + Ord + Copy>(ids: &mut [T], one: T, other: T) {
	let maybe_one_pos = ids.binary_search(&one);
	let maybe_other_pos = ids.binary_search(&other);
	match (maybe_one_pos, maybe_other_pos) {
		(Ok(one_pos), Err(_)) => ids[one_pos] = other,
		(Err(_), Ok(other_pos)) => ids[other_pos] = one,
		_ => return,
	};
	ids.sort();
}

impl<T: Config> SwapAux for Module<T> {
	fn ensure_can_swap(one: ParaId, other: ParaId) -> Result<(), &'static str> {
		if <Onboarding<T>>::contains_key(one) || <Onboarding<T>>::contains_key(other) {
			Err("can't swap an undeployed parachain")?
		}
		Ok(())
	}
	fn on_swap(one: ParaId, other: ParaId) -> Result<(), &'static str> {
		<Offboarding<T>>::swap(one, other);
		<Deposits<T>>::swap(one, other);
		ManagedIds::mutate(|ids| swap_ordered_existence(ids, one, other));
		Ok(())
	}
}

/// tests for this module
#[cfg(test)]
mod tests {
	use super::*;
	use std::{collections::HashMap, cell::RefCell};

	use sp_core::H256;
	use sp_runtime::traits::{BlakeTwo256, Hash, IdentityLookup};
	use frame_support::{
		impl_outer_origin, parameter_types, assert_ok, assert_noop,
		traits::{OnInitialize, OnFinalize}
	};
	use pallet_balances;
	use primitives::v1::{BlockNumber, Header, Id as ParaId};

	impl_outer_origin! {
		pub enum Origin for Test {}
	}

	// For testing the module, we construct most of a mock runtime. This means
	// first constructing a configuration type (`Test`) which `impl`s each of the
	// configuration traits of modules we want to use.
	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	parameter_types! {
		pub const BlockHashCount: u32 = 250;
	}
	impl frame_system::Config for Test {
		type BaseCallFilter = ();
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type Origin = Origin;
		type Call = ();
		type Index = u64;
		type BlockNumber = BlockNumber;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
		type BlockHashCount = BlockHashCount;
		type Version = ();
		type PalletInfo = ();
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type Event = ();
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type WeightInfo = ();
	}

	thread_local! {
		pub static PARACHAIN_COUNT: RefCell<u32> = RefCell::new(0);
		pub static PARACHAINS:
			RefCell<HashMap<u32, (ValidationCode, HeadData)>> = RefCell::new(HashMap::new());
	}

	const MAX_CODE_SIZE: u32 = 100;
	const MAX_HEAD_DATA_SIZE: u32 = 10;

	pub struct TestParachains;
	impl Registrar<u64> for TestParachains {
		fn new_id() -> ParaId {
			PARACHAIN_COUNT.with(|p| {
				*p.borrow_mut() += 1;
				(*p.borrow() - 1).into()
			})
		}

		fn head_data_size_allowed(head_data_size: u32) -> bool {
			head_data_size <= MAX_HEAD_DATA_SIZE
		}

		fn code_size_allowed(code_size: u32) -> bool {
			code_size <= MAX_CODE_SIZE
		}

		fn register_para(
			id: ParaId,
			_parachain: bool,
			code: ValidationCode,
			initial_head_data: HeadData,
		) -> DispatchResult {
			PARACHAINS.with(|p| {
				if p.borrow().contains_key(&id.into()) {
					panic!("ID already exists")
				}
				p.borrow_mut().insert(id.into(), (code, initial_head_data));
				Ok(())
			})
		}
		fn deregister_para(id: ParaId) -> DispatchResult {
			PARACHAINS.with(|p| {
				if !p.borrow().contains_key(&id.into()) {
					panic!("ID doesn't exist")
				}
				p.borrow_mut().remove(&id.into());
				Ok(())
			})
		}
	}

	fn reset_count() {
		PARACHAIN_COUNT.with(|p| *p.borrow_mut() = 0);
	}

	fn with_parachains<T>(f: impl FnOnce(&HashMap<u32, (ValidationCode, HeadData)>) -> T) -> T {
		PARACHAINS.with(|p| f(&*p.borrow()))
	}

	parameter_types!{
		pub const LeasePeriod: BlockNumber = 10;
		pub const EndingPeriod: BlockNumber = 3;
	}

	impl Config for Test {
		type Event = ();
		type Currency = Balances;
		type Parachains = TestParachains;
		type LeasePeriod = LeasePeriod;
		type EndingPeriod = EndingPeriod;
		type Randomness = RandomnessCollectiveFlip;
	}

	type System = frame_system::Module<Test>;
	type Balances = pallet_balances::Module<Test>;
	type Slots = Module<Test>;
	type RandomnessCollectiveFlip = pallet_randomness_collective_flip::Module<Test>;

	// This function basically just builds a genesis storage key/value store according to
	// our desired mock up.
	fn new_test_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test>{
			balances: vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
		}.assimilate_storage(&mut t).unwrap();
		t.into()
	}

	fn run_to_block(n: BlockNumber) {
		while System::block_number() < n {
			Slots::on_finalize(System::block_number());
			Balances::on_finalize(System::block_number());
			System::on_finalize(System::block_number());
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());
			Balances::on_initialize(System::block_number());
			Slots::on_initialize(System::block_number());
		}
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(Slots::auction_counter(), 0);
			assert_eq!(Slots::deposit_held(&0u32.into()), 0);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(10);

			assert_eq!(Slots::auction_counter(), 0);
			assert_eq!(Slots::deposit_held(&0u32.into()), 0);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn can_start_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			assert_eq!(Slots::auction_counter(), 1);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn auction_proceeds_correctly() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			assert_eq!(Slots::auction_counter(), 1);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(2);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(3);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(4);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(5);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(6);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(0));

			run_to_block(7);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(1));

			run_to_block(8);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(2));

			run_to_block(9);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn can_win_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Balances::reserved_balance(1), 0);
			assert_eq!(Balances::free_balance(1), 9);
		});
	}

	#[test]
	fn offboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));
			assert_eq!(Balances::free_balance(1), 9);

			run_to_block(9);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Slots::deposits(ParaId::from(0))[0], 0);

			run_to_block(50);
			assert_eq!(Slots::deposit_held(&0.into()), 0);
			assert_eq!(Balances::free_balance(1), 10);
		});
	}

	#[test]
	fn set_offboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(9);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Slots::deposits(ParaId::from(0))[0], 0);

			run_to_block(49);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_ok!(Slots::set_offboarding(Origin::signed(ParaId::from(0).into_account()), 10));

			run_to_block(50);
			assert_eq!(Slots::deposit_held(&0.into()), 0);
			assert_eq!(Balances::free_balance(10), 1);
		});
	}

	#[test]
	fn onboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(9);
			let h = BlakeTwo256::hash(&[42u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![69].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![42].into()));

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![42].into(), vec![69].into()));
			});
		});
	}

	#[test]
	fn late_onboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});

			run_to_block(11);
			let h = BlakeTwo256::hash(&[42u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![69].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![42].into()));
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![42].into(), vec![69].into()));
			});
		});
	}

	#[test]
	fn under_bidding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 5));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(2), 0);
			assert_eq!(Balances::free_balance(2), 20);
			assert_eq!(
				Slots::winning(0).unwrap()[SlotRange::ZeroThree as u8 as usize],
				Some((Bidder::New(NewBidder{who: 1, sub: 0}), 5))
			);
		});
	}

	#[test]
	fn should_choose_best_combination() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 2, 3, 1));
			assert_ok!(Slots::bid(Origin::signed(3), 0, 1, 4, 4, 2));
			assert_ok!(Slots::bid(Origin::signed(1), 1, 1, 1, 4, 1));
			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(2), vec![1.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(1)),
				Some((2, IncomingParachain::Unset(NewBidder { who: 2, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(4), vec![2.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(2)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);
		});
	}

	#[test]
	fn independent_bids_should_fail() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 2, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 2, 4, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 2, 2, 1));
			assert_noop!(
				Slots::bid(Origin::signed(1), 0, 1, 3, 3, 1),
				Error::<Test>::NonIntersectingRange
			);
		});
	}

	#[test]
	fn multiple_onboards_offboards_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 2, 3, 1));
			assert_ok!(Slots::bid(Origin::signed(3), 0, 1, 4, 4, 1));

			run_to_block(5);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(4), 1, 2, 1, 2, 1));
			assert_ok!(Slots::bid(Origin::signed(5), 1, 2, 3, 4, 1));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into(), 3.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(
				Slots::onboarding(ParaId::from(3)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 4, sub: 1 })))
			);
			assert_eq!(Slots::onboard_queue(2), vec![1.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(1)),
				Some((2, IncomingParachain::Unset(NewBidder { who: 2, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(3), vec![4.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(4)),
				Some((3, IncomingParachain::Unset(NewBidder { who: 5, sub: 1 })))
			);
			assert_eq!(Slots::onboard_queue(4), vec![2.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(2)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);

			for &(para, sub, acc) in &[(0, 0, 1), (1, 0, 2), (2, 0, 3), (3, 1, 4), (4, 1, 5)] {
				let h = BlakeTwo256::hash(&[acc][..]);
				assert_ok!(Slots::fix_deploy_data(Origin::signed(acc as _), sub, para.into(), h, 1, vec![acc].into()));
				assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), para.into(), vec![acc].into()));
			}

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
				assert_eq!(p[&3], (vec![4].into(), vec![4].into()));
			});
			run_to_block(20);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&1], (vec![2].into(), vec![2].into()));
				assert_eq!(p[&3], (vec![4].into(), vec![4].into()));
			});
			run_to_block(30);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&1], (vec![2].into(), vec![2].into()));
				assert_eq!(p[&4], (vec![5].into(), vec![5].into()));
			});
			run_to_block(40);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&2], (vec![3].into(), vec![3].into()));
				assert_eq!(p[&4], (vec![5].into(), vec![5].into()));
			});
			run_to_block(50);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});
		});
	}

	#[test]
	fn extensions_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);
			let h = BlakeTwo256::hash(&[1u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![1].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![1].into()));

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 2, 2, 2, 1));

			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});

			run_to_block(20);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});
			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Balances::transfer(Origin::signed(1), ParaId::from(0).into_account(), 1));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 3, 3, 3, 2));

			run_to_block(30);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});

			run_to_block(40);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});
		});
	}

	#[test]
	fn renewal_with_lower_value_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);
			let h = BlakeTwo256::hash(&[1u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![1].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![1].into()));

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 2, 2, 2, 3));

			run_to_block(20);
			assert_eq!(Balances::free_balance(&ParaId::from(0u32).into_account()), 2);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 3, 3, 3, 4));

			run_to_block(30);
			assert_eq!(Balances::free_balance(&ParaId::from(0u32).into_account()), 1);
		});
	}

	#[test]
	fn can_win_incomplete_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 4, 4, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![]);
			assert_eq!(Slots::onboard_queue(2), vec![]);
			assert_eq!(Slots::onboard_queue(3), vec![]);
			assert_eq!(Slots::onboard_queue(4), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 5);
		});
	}

	#[test]
	fn multiple_bids_work_pre_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			for i in 1..6u64 {
				run_to_block(i as _);
				assert_ok!(Slots::bid(Origin::signed(i), 0, 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j == i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j == i { j * 9 } else { j * 10 });
				}
			}

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 5, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 5);
			assert_eq!(Balances::reserved_balance(5), 0);
			assert_eq!(Balances::free_balance(5), 45);
		});
	}

	#[test]
	fn multiple_bids_work_post_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			for i in 1..6u64 {
				run_to_block((i + 3) as _);
				assert_ok!(Slots::bid(Origin::signed(i), 0, 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j == i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j == i { j * 9 } else { j * 10 });
				}
			}

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 3);
			assert_eq!(Balances::reserved_balance(3), 0);
			assert_eq!(Balances::free_balance(3), 27);
		});
	}

	#[test]
	fn incomplete_calculate_winners_works() {
		let winning = [
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
		];
		let winners = vec![
			(Some(NewBidder{who: 1, sub: 0}), 0.into(), 1, SlotRange::ThreeThree)
		];

		assert_eq!(Slots::calculate_winners(winning, TestParachains::new_id), winners);
	}

	#[test]
	fn first_incomplete_calculate_winners_works() {
		let winning = [
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
		];
		let winners = vec![
			(Some(NewBidder{who: 1, sub: 0}), 0.into(), 1, SlotRange::ZeroZero)
		];

		assert_eq!(Slots::calculate_winners(winning, TestParachains::new_id), winners);
	}

	#[test]
	fn calculate_winners_works() {
		let mut winning = [
			/*0..0*/
			Some((Bidder::New(NewBidder{who: 2, sub: 0}), 2)),
			/*0..1*/
			None,
			/*0..2*/
			None,
			/*0..3*/
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
			/*1..1*/
			Some((Bidder::New(NewBidder{who: 3, sub: 0}), 1)),
			/*1..2*/
			None,
			/*1..3*/
			None,
			/*2..2*/
			//Some((Bidder::New(NewBidder{who: 4, sub: 0}), 1)),
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 53)),
			/*2..3*/
			None,
			/*3..3*/
			Some((Bidder::New(NewBidder{who: 5, sub: 0}), 1)),
		];
		let winners = vec![
			(Some(NewBidder{who: 2,sub: 0}), 0.into(), 2, SlotRange::ZeroZero),
			(Some(NewBidder{who: 3,sub: 0}), 1.into(), 1, SlotRange::OneOne),
			(Some(NewBidder{who: 1,sub: 0}), 2.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 3.into(), 1, SlotRange::ThreeThree)
		];

		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);

		reset_count();
		winning[SlotRange::ZeroThree as u8 as usize] = Some((Bidder::New(NewBidder{who: 1, sub: 0}), 2));
		let winners = vec![
			(Some(NewBidder{who: 2,sub: 0}), 0.into(), 2, SlotRange::ZeroZero),
			(Some(NewBidder{who: 3,sub: 0}), 1.into(), 1, SlotRange::OneOne),
			(Some(NewBidder{who: 1,sub: 0}), 2.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 3.into(), 1, SlotRange::ThreeThree)
		];
		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);

		reset_count();
		winning[SlotRange::ZeroOne as u8 as usize] = Some((Bidder::New(NewBidder{who: 4, sub: 0}), 3));
		let winners = vec![
			(Some(NewBidder{who: 4,sub: 0}), 0.into(), 3, SlotRange::ZeroOne),
			(Some(NewBidder{who: 1,sub: 0}), 1.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 2.into(), 1, SlotRange::ThreeThree)
		];
		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);
	}

	#[test]
	fn deploy_code_too_large() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; (MAX_CODE_SIZE + 1) as _];
			let h = BlakeTwo256::hash(&code[..]);
			assert_eq!(
				Slots::fix_deploy_data(
					Origin::signed(1), 0, 0.into(), h, code.len() as _, vec![1].into(),
				),
				Err(Error::<Test>::CodeTooLarge.into()),
			);
		});
	}

	#[test]
	fn deploy_maximum_ok() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; MAX_HEAD_DATA_SIZE as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_ok!(Slots::fix_deploy_data(
				Origin::signed(1), 0, 0.into(), h, code.len() as _, head_data,
			));
		});
	}

	#[test]
	fn deploy_head_data_too_large() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; (MAX_HEAD_DATA_SIZE + 1) as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_eq!(
				Slots::fix_deploy_data(
					Origin::signed(1), 0, 0.into(), h, code.len() as _, head_data,
				),
				Err(Error::<Test>::HeadDataTooLarge.into()),
			);
		});
	}

	#[test]
	fn code_size_must_be_correct() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; MAX_HEAD_DATA_SIZE as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_ok!(Slots::fix_deploy_data(
				Origin::signed(1), 0, 0.into(), h, (code.len() - 1) as _, head_data,
			));
			assert!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), code.into()).is_err());
		});
	}
}
