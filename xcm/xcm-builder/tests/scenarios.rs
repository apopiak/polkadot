// Copyright 2020 Parity Technologies (UK) Ltd.
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

mod mock;

use frame_support::{pallet_prelude::Weight, weights::constants::WEIGHT_PER_SECOND};
use mock::{
	fake_message_hash, kusama_like_with_balances, AccountId, Balance, Balances, BaseXcmWeight, KsmPerSecond,
	System, XcmConfig, XcmPallet, CENTS,
};
use polkadot_parachain::primitives::Id as ParaId;
use pretty_assertions::assert_eq;
use sp_core::H256;
use sp_runtime::traits::{AccountIdConversion, BlakeTwo256, Hash};
use xcm::{latest::prelude::*, VersionedMultiAssets};
use xcm_executor::XcmExecutor;

pub const ALICE: AccountId = AccountId::new([0u8; 32]);
pub const PARA_ID: u32 = 2000;
pub const INITIAL_BALANCE: u128 = 100_000_000_000;
pub const REGISTER_AMOUNT: Balance = 10 * CENTS;

// Construct a `BuyExecution` order.
fn buy_execution<C>() -> Instruction<C> {
	BuyExecution { fees: (Here, REGISTER_AMOUNT).into(), weight_limit: Unlimited }
}

// Calculate fees based on the weight.
fn fees(weight: Weight) -> Balance {
	let (_, rate) = KsmPerSecond::get();
	let fees = weight as u128 * rate / (WEIGHT_PER_SECOND as u128);
	assert!(fees > 0);
	fees
}

// Determine the hash for assets expected to be have been trapped.
fn determine_hash<M>(origin: &MultiLocation, assets: M) -> H256
where
	M: Into<MultiAssets>,
{
	let versioned = VersionedMultiAssets::from(assets.into());
	BlakeTwo256::hash_of(&(origin, &versioned))
}

/// Scenario:
/// A parachain transfers funds on the relay-chain to another parachain's account.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn withdraw_and_deposit_works() {
	let para_acc: AccountId = ParaId::from(PARA_ID).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let other_para_id = 3000;
		let amount = REGISTER_AMOUNT;
		let weight = 3 * BaseXcmWeight::get();
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: Parachain(other_para_id).into(),
			},
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(r, Outcome::Complete(weight));
		let other_para_acc: AccountId = ParaId::from(other_para_id).into_account_truncating();
		assert_eq!(Balances::free_balance(para_acc), INITIAL_BALANCE - amount);
		assert_eq!(Balances::free_balance(other_para_acc), amount - fees(weight));
	});
}

/// Scenario:
/// Alice simply wants to transfer funds to Bob's account via XCM.
///
/// Asserts that the balances are updated correctly and the correct events are fired.
#[test]
fn transfer_asset_works() {
	let bob = AccountId::new([1u8; 32]);
	let balances = vec![(ALICE, INITIAL_BALANCE), (bob.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let amount = REGISTER_AMOUNT;
		let weight = BaseXcmWeight::get();
		let message = Xcm(vec![TransferAsset {
			assets: (Here, amount).into(),
			beneficiary: AccountId32 { network: None, id: bob.clone().into() }.into(),
		}]);
		let hash = fake_message_hash(&message);
		// Use `execute_xcm_in_credit` here to pass through the barrier
		let r = XcmExecutor::<XcmConfig>::execute_xcm_in_credit(
			AccountId32 { network: None, id: ALICE.into() },
			message,
			hash,
			weight,
			weight,
		);
		System::assert_last_event(
			pallet_balances::Event::Transfer { from: ALICE, to: bob.clone(), amount }.into(),
		);
		assert_eq!(r, Outcome::Complete(weight));
		assert_eq!(Balances::free_balance(ALICE), INITIAL_BALANCE - amount);
		assert_eq!(Balances::free_balance(bob), INITIAL_BALANCE + amount);
	});
}

/// Scenario:
/// A parachain wants to be notified that a transfer worked correctly.
/// It includes a `QueryHolding` order after the deposit to get notified on success.
/// This somewhat abuses `QueryHolding` as an indication of execution success. It works because
/// order execution halts on error (so no `QueryResponse` will be sent if the previous order failed).
/// The inner response sent due to the query is not used.
///
/// Asserts that the balances are updated correctly and the expected XCM is sent.
#[test]
fn report_holding_works() {
	let para_acc: AccountId = ParaId::from(PARA_ID).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let other_para_id = 3000;
		let amount = REGISTER_AMOUNT;
		let weight = 4 * BaseXcmWeight::get();
		let response_info = QueryResponseInfo {
			destination: Parachain(PARA_ID).into(),
			query_id: 1234,
			max_weight: 1_000_000_000,
		};
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: OnlyChild.into(), // invalid destination
			},
			// is not triggered becasue the deposit fails
			ReportHolding { response_info: response_info.clone(), assets: All.into() },
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(
			r,
			Outcome::Incomplete(
				weight - BaseXcmWeight::get(),
				XcmError::FailedToTransactAsset("AccountIdConversionFailed")
			)
		);
		// there should be no query response sent for the failed deposit
		assert_eq!(mock::sent_xcm(), vec![]);
		assert_eq!(Balances::free_balance(para_acc.clone()), INITIAL_BALANCE - amount);

		// now do a successful transfer
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: Parachain(other_para_id).into(),
			},
			// used to get a notification in case of success
			ReportHolding { response_info: response_info.clone(), assets: AllCounted(1).into() },
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(r, Outcome::Complete(weight));
		let other_para_acc: AccountId = ParaId::from(other_para_id).into_account_truncating();
		assert_eq!(Balances::free_balance(other_para_acc), amount - fees(weight));
		assert_eq!(Balances::free_balance(para_acc), INITIAL_BALANCE - 2 * amount);
		let expected_msg = Xcm(vec![QueryResponse {
			query_id: response_info.query_id,
			response: Response::Assets(vec![].into()),
			max_weight: response_info.max_weight,
			querier: Some(Here.into()),
		}]);
		let expected_hash = fake_message_hash(&expected_msg);
		assert_eq!(
			mock::sent_xcm(),
			vec![(Parachain(PARA_ID).into(), expected_msg, expected_hash,)]
		);
	});
}

/// Scenario:
/// A parachain wants to move KSM from Kusama to Statemine.
/// The parachain sends an XCM to withdraw funds combined with a teleport to the destination.
///
/// This way of moving funds from a relay to a parachain will only work for trusted chains.
/// Reserve based transfer should be used to move KSM to a community parachain.
///
/// Asserts that the balances are updated accordingly and the correct XCM is sent.
#[test]
fn teleport_to_statemine_works() {
	let para_acc: AccountId = ParaId::from(PARA_ID).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let statemine_id = 1000;
		let other_para_id = 3000;
		let amount = REGISTER_AMOUNT;
		let teleport_effects = vec![
			buy_execution(), // unchecked mock value
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: (Parent, Parachain(PARA_ID)).into(),
			},
		];
		let weight = 3 * BaseXcmWeight::get();

		// teleports are allowed to community chains, even in the absence of trust from their side.
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			InitiateTeleport {
				assets: All.into(),
				dest: Parachain(other_para_id).into(),
				xcm: Xcm(teleport_effects.clone()),
			},
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(r, Outcome::Complete(weight));
		let expected_msg = Xcm(vec![ReceiveTeleportedAsset((Parent, amount - fees(weight)).into()), ClearOrigin]
			.into_iter()
			.chain(teleport_effects.clone().into_iter())
			.collect());
		let expected_hash = fake_message_hash(&expected_msg);
		assert_eq!(
			mock::sent_xcm(),
			vec![(Parachain(other_para_id).into(), expected_msg, expected_hash,)]
		);

		// teleports are allowed from statemine to kusama.
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			InitiateTeleport {
				assets: All.into(),
				dest: Parachain(statemine_id).into(),
				xcm: Xcm(teleport_effects.clone()),
			},
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(r, Outcome::Complete(weight));
		// 2 * amount because of the other teleport above
		assert_eq!(Balances::free_balance(para_acc), INITIAL_BALANCE - 2 * amount);
		let expected_msg = Xcm(vec![ReceiveTeleportedAsset((Parent, amount - fees(weight)).into()), ClearOrigin]
			.into_iter()
			.chain(teleport_effects.clone().into_iter())
			.collect());
		let expected_hash = fake_message_hash(&expected_msg);
		assert_eq!(
			mock::sent_xcm(),
			vec![
				(Parachain(other_para_id).into(), expected_msg.clone(), expected_hash,),
				(Parachain(statemine_id).into(), expected_msg, expected_hash,)
			]
		);
	});
}

/// Scenario:
/// A parachain wants to move KSM from Kusama to the parachain.
/// It withdraws funds and then deposits them into the reserve account of the destination chain.
/// to the destination.
///
/// Asserts that the balances are updated accordingly and the correct XCM is sent.
#[test]
fn reserve_based_transfer_works() {
	let para_acc: AccountId = ParaId::from(PARA_ID).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let other_para_id = 3000;
		let amount = REGISTER_AMOUNT;
		let transfer_effects = vec![
			buy_execution(), // unchecked mock value
			DepositAsset {
				assets: AllCounted(1).into(),
				beneficiary: (Parent, Parachain(PARA_ID)).into(),
			},
		];
		let message = Xcm(vec![
			WithdrawAsset((Here, amount).into()),
			buy_execution(),
			DepositReserveAsset {
				assets: AllCounted(1).into(),
				dest: Parachain(other_para_id).into(),
				xcm: Xcm(transfer_effects.clone()),
			},
		]);
		let hash = fake_message_hash(&message);
		let weight = 3 * BaseXcmWeight::get();
		let r = XcmExecutor::<XcmConfig>::execute_xcm(Parachain(PARA_ID), message, hash, weight);
		assert_eq!(r, Outcome::Complete(weight));
		assert_eq!(Balances::free_balance(para_acc), INITIAL_BALANCE - amount);
		let expected_msg = Xcm(vec![ReserveAssetDeposited((Parent, amount - fees(weight)).into()), ClearOrigin]
			.into_iter()
			.chain(transfer_effects.into_iter())
			.collect());
		let expected_hash = fake_message_hash(&expected_msg);
		assert_eq!(
			mock::sent_xcm(),
			vec![(Parachain(other_para_id).into(), expected_msg, expected_hash,)]
		);
	});
}

/// Scenario:
/// A parachain sends an XCM with an unknown asset to Kusama.
///
/// Asserts that asset ends up in the asset trap.
#[test]
fn unknown_tokens_are_trapped_on_failed_buy_execution() {
	let para_acc: AccountId = ParaId::from(PARA_ID).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let amount = REGISTER_AMOUNT;
		let weight = 4 * BaseXcmWeight::get();
		let asset: MultiAsset = (Parachain(PARA_ID), amount).into();
		let assets: MultiAssets = vec![asset.clone()].into();
		let origin = Parachain(PARA_ID);
		let message = Xcm(vec![
			ReserveAssetDeposited(assets.clone()),
			ClearOrigin,
			BuyExecution { fees: asset, weight_limit: Limited(weight) },
			DepositAsset { assets: All.into(), beneficiary: Here.into() },
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(
			origin.clone(),
			message,
			hash,
			weight,
		);
		assert_eq!(r, Outcome::Incomplete(3 * BaseXcmWeight::get(), XcmError::TooExpensive));
		let origin: MultiLocation = origin.into();
		let hash = determine_hash(&origin, assets);
		assert_eq!(XcmPallet::asset_trap(hash), 1);
	});
}

/// Scenario:
/// Statemine sends an XCM with KSM and two unknown assets to Kusama.
///
/// Note: This is a bit of a convoluted way of triggering the error in `deposit_asset` and does not
/// represent a realistic scenario that would be encountered in practice.
///
/// Asserts that those unknown tokens end up in the asset trap.
#[test]
fn unknown_tokens_are_trapped_on_failed_deposit() {
	let statemine_id = 1000;
	let para_acc: AccountId = ParaId::from(statemine_id).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let amount = REGISTER_AMOUNT;
		let weight = 4 * BaseXcmWeight::get();
		let para_asset: MultiAsset = (X2(Parachain(statemine_id), GeneralIndex(1)), amount).into();
		let other_para_asset: MultiAsset =
			(X2(Parachain(statemine_id), GeneralIndex(2)), amount).into();
		let ksm: MultiAsset = (Here, amount).into();
		let assets: MultiAssets =
			vec![ksm.clone(), para_asset.clone(), other_para_asset.clone()].into();
		let origin = Parachain(statemine_id);
		let message = Xcm(vec![
			ReserveAssetDeposited(assets.clone()),
			ClearOrigin,
			BuyExecution { fees: ksm, weight_limit: Limited(weight) },
			DepositAsset {
				assets: All.into(),
				beneficiary: Parachain(statemine_id).into(),
			},
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(
			origin.clone(),
			message,
			hash,
			weight,
		);
		assert_eq!(r, Outcome::Incomplete(4 * BaseXcmWeight::get(), XcmError::AssetNotFound));
		let origin: MultiLocation = origin.into();
		let hash = determine_hash(&origin, vec![para_asset.clone(), other_para_asset.clone()]);
		assert_eq!(XcmPallet::asset_trap(hash), 1);
	});
}

/// Scenario:
/// Statemine sends an XCM with KSM and two unknown assets to Kusama.
///
/// Note: This is a bit of a convoluted way of triggering the error in `deposit_asset` and does not
/// represent a realistic scenario that would be encountered in practice.
///
/// Asserts that those funds end up in the asset trap.
#[test]
fn unknown_tokens_are_trapped_on_failed_reserve_deposit() {
	let statemine_id = 1000;
	let para_acc: AccountId = ParaId::from(statemine_id).into_account_truncating();
	let balances = vec![(ALICE, INITIAL_BALANCE), (para_acc.clone(), INITIAL_BALANCE)];
	kusama_like_with_balances(balances).execute_with(|| {
		let amount = REGISTER_AMOUNT;
		let weight = 4 * BaseXcmWeight::get();
		let para_asset: MultiAsset = (X2(Parachain(statemine_id), GeneralIndex(1)), amount).into();
		let other_para_asset: MultiAsset =
			(X2(Parachain(statemine_id), GeneralIndex(2)), amount).into();
		let ksm: MultiAsset = (Here, amount).into();
		let assets: MultiAssets =
			vec![ksm.clone(), para_asset.clone(), other_para_asset.clone()].into();
		let origin = Parachain(statemine_id);
		let message = Xcm(vec![
			ReserveAssetDeposited(assets.clone()),
			ClearOrigin,
			BuyExecution { fees: ksm, weight_limit: Limited(weight) },
			DepositReserveAsset {
				assets: All.into(),
				dest: Parachain(statemine_id).into(),
				xcm: Xcm(vec![]),
			},
		]);
		let hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::execute_xcm(
			origin.clone(),
			message,
			hash,
			weight,
		);
		assert_eq!(r, Outcome::Incomplete(4 * BaseXcmWeight::get(), XcmError::AssetNotFound));
		let origin: MultiLocation = origin.into();
		let hash = determine_hash(&origin, vec![para_asset.clone(), other_para_asset.clone()]);
		assert_eq!(XcmPallet::asset_trap(hash), 1);
	});
}
