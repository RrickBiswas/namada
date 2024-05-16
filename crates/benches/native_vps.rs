use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::Deref;
use std::rc::Rc;
use std::str::FromStr;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use masp_primitives::sapling::redjubjub::PublicKey;
use masp_primitives::sapling::Node;
use masp_primitives::transaction::sighash::{signature_hash, SignableInput};
use masp_primitives::transaction::txid::TxIdDigester;
use masp_primitives::transaction::TransactionData;
use masp_proofs::group::GroupEncoding;
use masp_proofs::sapling::BatchValidator;
use namada::core::address::{self, Address, InternalAddress};
use namada::core::collections::HashMap;
use namada::core::eth_bridge_pool::{GasFee, PendingTransfer};
use namada::core::masp::{TransferSource, TransferTarget};
use namada::eth_bridge::storage::eth_bridge_queries::is_bridge_comptime_enabled;
use namada::eth_bridge::storage::whitelist;
use namada::governance::pgf::storage::steward::StewardDetail;
use namada::governance::storage::proposal::ProposalType;
use namada::governance::storage::vote::ProposalVote;
use namada::governance::{InitProposalData, VoteProposalData};
use namada::ibc::core::channel::types::channel::Order;
use namada::ibc::core::channel::types::msgs::MsgChannelOpenInit;
use namada::ibc::core::channel::types::Version as ChannelVersion;
use namada::ibc::core::commitment_types::commitment::CommitmentPrefix;
use namada::ibc::core::connection::types::msgs::MsgConnectionOpenInit;
use namada::ibc::core::connection::types::version::Version;
use namada::ibc::core::connection::types::Counterparty;
use namada::ibc::core::host::types::identifiers::{
    ClientId, ConnectionId, PortId,
};
use namada::ibc::primitives::ToProto;
use namada::ibc::{IbcActions, NftTransferModule, TransferModule};
use namada::ledger::eth_bridge::read_native_erc20_address;
use namada::ledger::gas::{TxGasMeter, VpGasMeter};
use namada::ledger::governance::GovernanceVp;
use namada::ledger::native_vp::ethereum_bridge::bridge_pool_vp::BridgePoolVp;
use namada::ledger::native_vp::ethereum_bridge::nut::NonUsableTokens;
use namada::ledger::native_vp::ethereum_bridge::vp::EthBridge;
use namada::ledger::native_vp::ibc::context::PseudoExecutionContext;
use namada::ledger::native_vp::ibc::Ibc;
use namada::ledger::native_vp::masp::MaspVp;
use namada::ledger::native_vp::multitoken::MultitokenVp;
use namada::ledger::native_vp::parameters::ParametersVp;
use namada::ledger::native_vp::{Ctx, NativeVp};
use namada::ledger::pgf::PgfVp;
use namada::ledger::pos::PosVP;
use namada::proof_of_stake;
use namada::proof_of_stake::KeySeg;
use namada::sdk::masp::{partial_deauthorize, preload_verifying_keys, PVKs};
use namada::sdk::masp_primitives::merkle_tree::CommitmentTree;
use namada::sdk::masp_primitives::transaction::Transaction;
use namada::sdk::masp_proofs::sapling::SaplingVerificationContextInner;
use namada::state::{Epoch, StorageRead, StorageWrite, TxIndex};
use namada::token::{Amount, Transfer};
use namada::tx::{Code, Section, Tx};
use namada_apps::bench_utils::{
    generate_foreign_key_tx, BenchShell, BenchShieldedCtx,
    ALBERT_PAYMENT_ADDRESS, ALBERT_SPENDING_KEY, BERTHA_PAYMENT_ADDRESS,
    TX_BRIDGE_POOL_WASM, TX_IBC_WASM, TX_INIT_PROPOSAL_WASM, TX_RESIGN_STEWARD,
    TX_TRANSFER_WASM, TX_UPDATE_STEWARD_COMMISSION, TX_VOTE_PROPOSAL_WASM,
};
use namada_apps::wallet::defaults;
use rand_core::OsRng;

fn governance(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_governance");

    for bench_name in [
        "foreign_key_write",
        "delegator_vote",
        "validator_vote",
        "minimal_proposal",
        "complete_proposal",
    ] {
        let mut shell = BenchShell::default();

        let signed_tx = match bench_name {
            "foreign_key_write" => {
                generate_foreign_key_tx(&defaults::albert_keypair())
            }
            "delegator_vote" => {
                // Advance to the proposal voting period
                shell.advance_epoch();
                shell.generate_tx(
                    TX_VOTE_PROPOSAL_WASM,
                    VoteProposalData {
                        id: 0,
                        vote: ProposalVote::Yay,
                        voter: defaults::albert_address(),
                    },
                    None,
                    None,
                    vec![&defaults::albert_keypair()],
                )
            }
            "validator_vote" => {
                // Advance to the proposal voting period
                shell.advance_epoch();
                shell.generate_tx(
                    TX_VOTE_PROPOSAL_WASM,
                    VoteProposalData {
                        id: 0,
                        vote: ProposalVote::Nay,
                        voter: defaults::validator_address(),
                    },
                    None,
                    None,
                    vec![&defaults::albert_keypair()],
                )
            }
            "minimal_proposal" => {
                let content_section =
                    Section::ExtraData(Code::new(vec![], None));
                let params =
                    proof_of_stake::storage::read_pos_params(&shell.state)
                        .unwrap();
                let voting_start_epoch =
                    Epoch(2 + params.pipeline_len + params.unbonding_len);
                // Must start after current epoch
                debug_assert_eq!(
                    shell.state.get_block_epoch().unwrap().next(),
                    voting_start_epoch
                );
                shell.generate_tx(
                    TX_INIT_PROPOSAL_WASM,
                    InitProposalData {
                        content: content_section.get_hash(),
                        author: defaults::albert_address(),
                        r#type: ProposalType::Default,
                        voting_start_epoch,
                        voting_end_epoch: voting_start_epoch
                            .unchecked_add(3_u64),
                        activation_epoch: voting_start_epoch
                            .unchecked_add(9_u64),
                    },
                    None,
                    Some(vec![content_section]),
                    vec![&defaults::albert_keypair()],
                )
            }
            "complete_proposal" => {
                let max_code_size_key =
                namada::governance::storage::keys::get_max_proposal_code_size_key();
                let max_proposal_content_key =
                    namada::governance::storage::keys::get_max_proposal_content_key();
                let max_code_size: u64 = shell
                    .state
                    .read(&max_code_size_key)
                    .expect("Error while reading from storage")
                    .expect("Missing max_code_size parameter in storage");
                let max_proposal_content_size: u64 = shell
                    .state
                    .read(&max_proposal_content_key)
                    .expect("Error while reading from storage")
                    .expect(
                        "Missing max_proposal_content parameter in storage",
                    );
                let content_section = Section::ExtraData(Code::new(
                    vec![0; max_proposal_content_size as _],
                    None,
                ));
                let wasm_code_section = Section::ExtraData(Code::new(
                    vec![0; max_code_size as _],
                    None,
                ));

                let params =
                    proof_of_stake::storage::read_pos_params(&shell.state)
                        .unwrap();
                let voting_start_epoch =
                    Epoch(2 + params.pipeline_len + params.unbonding_len);
                // Must start after current epoch
                debug_assert_eq!(
                    shell.state.get_block_epoch().unwrap().next(),
                    voting_start_epoch
                );
                shell.generate_tx(
                    TX_INIT_PROPOSAL_WASM,
                    InitProposalData {
                        content: content_section.get_hash(),
                        author: defaults::albert_address(),
                        r#type: ProposalType::DefaultWithWasm(
                            wasm_code_section.get_hash(),
                        ),
                        voting_start_epoch,
                        voting_end_epoch: voting_start_epoch
                            .unchecked_add(3_u64),
                        activation_epoch: voting_start_epoch
                            .unchecked_add(9_u64),
                    },
                    None,
                    Some(vec![content_section, wasm_code_section]),
                    vec![&defaults::albert_keypair()],
                )
            }
            _ => panic!("Unexpected bench test"),
        };

        // Run the tx to validate
        let verifiers_from_tx = shell.execute_tx(&signed_tx);

        let (verifiers, keys_changed) = shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let governance = GovernanceVp {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Governance),
                &shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    governance
                        .validate_tx(
                            &signed_tx,
                            governance.ctx.keys_changed,
                            governance.ctx.verifiers,
                        )
                        .is_ok()
                )
            })
        });
    }

    group.finish();
}

// TODO: uncomment when SlashFund internal address is brought back
// fn slash_fund(c: &mut Criterion) {
//      let mut group = c.benchmark_group("vp_slash_fund");

//      // Write a random key under a foreign subspace
//      let foreign_key_write =
//          generate_foreign_key_tx(&defaults::albert_keypair());

//      let content_section = Section::ExtraData(Code::new(vec![]));
//      let governance_proposal = shell.generate_tx(
//          TX_INIT_PROPOSAL_WASM,
//          InitProposalData {
//              id: 0,
//              content: content_section.get_hash(),
//              author: defaults::albert_address(),
//              r#type: ProposalType::Default(None),
//              voting_start_epoch: 12.into(),
//              voting_end_epoch: 15.into(),
//              activation_epoch: 18.into(),
//          },
//          None,
//          Some(vec![content_section]),
//          Some(&defaults::albert_keypair()),
//      );

//      for (tx, bench_name) in [foreign_key_write, governance_proposal]
//          .into_iter()
//          .zip(["foreign_key_write", "governance_proposal"])
//      {
//          let mut shell = BenchShell::default();

//          // Run the tx to validate
//          let verifiers_from_tx = shell.execute_tx(&tx);

//          let (verifiers, keys_changed) = shell
//              .state
//              .write_log
//              .verifiers_and_changed_keys(&verifiers_from_tx);

//          let slash_fund = SlashFundVp {
//              ctx: Ctx::new(
//                  &Address::Internal(InternalAddress::SlashFund),
//                  &shell.state.storage,
//                  &shell.state.write_log,
//                  &tx,
//                  &TxIndex(0),
//
// VpGasMeter::new_from_tx_meter(&TxGasMeter::new_from_sub_limit(
// u64::MAX.into(),                  )),
//                  &keys_changed,
//                  &verifiers,
//                  shell.vp_wasm_cache.clone(),
//              ),
//          };

//          group.bench_function(bench_name, |b| {
//              b.iter(|| {
//                  assert!(
//                      slash_fund
//                          .validate_tx(
//                              &tx,
//                              slash_fund.ctx.keys_changed,
//                              slash_fund.ctx.verifiers,
//                          )
//                          .unwrap()
//                  )
//              })
//          });
//      }

//      group.finish();
//  }

fn prepare_ibc_tx_and_ctx(bench_name: &str) -> (BenchShieldedCtx, Tx) {
    match bench_name {
        "open_connection" => {
            let mut shielded_ctx = BenchShieldedCtx::default();
            let _ = shielded_ctx.shell.init_ibc_client_state(
                namada::core::storage::Key::from(
                    Address::Internal(InternalAddress::Ibc).to_db_key(),
                ),
            );
            let msg = MsgConnectionOpenInit {
                client_id_on_a: ClientId::new("07-tendermint", 1).unwrap(),
                counterparty: Counterparty::new(
                    ClientId::from_str("07-tendermint-1").unwrap(),
                    None,
                    CommitmentPrefix::try_from(b"ibc".to_vec()).unwrap(),
                ),
                version: Some(Version::compatibles().first().unwrap().clone()),
                delay_period: std::time::Duration::new(100, 0),
                signer: defaults::albert_address().to_string().into(),
            };
            let mut data = vec![];
            prost::Message::encode(&msg.to_any(), &mut data).unwrap();
            let open_connection =
                shielded_ctx.shell.generate_ibc_tx(TX_IBC_WASM, data);

            (shielded_ctx, open_connection)
        }
        "open_channel" => {
            let mut shielded_ctx = BenchShieldedCtx::default();
            let _ = shielded_ctx.shell.init_ibc_connection();
            // Channel handshake
            let msg = MsgChannelOpenInit {
                port_id_on_a: PortId::transfer(),
                connection_hops_on_a: vec![ConnectionId::new(1)],
                port_id_on_b: PortId::transfer(),
                ordering: Order::Unordered,
                signer: defaults::albert_address().to_string().into(),
                version_proposal: ChannelVersion::new("ics20-1".to_string()),
            };

            // Avoid serializing the data again with borsh
            let mut data = vec![];
            prost::Message::encode(&msg.to_any(), &mut data).unwrap();
            let open_channel =
                shielded_ctx.shell.generate_ibc_tx(TX_IBC_WASM, data);

            (shielded_ctx, open_channel)
        }
        "outgoing_transfer" => {
            let mut shielded_ctx = BenchShieldedCtx::default();
            shielded_ctx.shell.init_ibc_channel();
            shielded_ctx.shell.enable_ibc_transfer();
            let outgoing_transfer =
                shielded_ctx.shell.generate_ibc_transfer_tx();

            (shielded_ctx, outgoing_transfer)
        }
        "outgoing_shielded_action" => {
            let mut shielded_ctx = BenchShieldedCtx::default();
            shielded_ctx.shell.init_ibc_channel();
            shielded_ctx.shell.enable_ibc_transfer();

            let albert_payment_addr = shielded_ctx
                .wallet
                .find_payment_addr(ALBERT_PAYMENT_ADDRESS)
                .unwrap()
                .to_owned();
            let albert_spending_key = shielded_ctx
                .wallet
                .find_spending_key(ALBERT_SPENDING_KEY, None)
                .unwrap()
                .to_owned();
            // Shield some tokens for Albert
            let (mut shielded_ctx, shield_tx) = shielded_ctx.generate_masp_tx(
                Amount::native_whole(500),
                TransferSource::Address(defaults::albert_address()),
                TransferTarget::PaymentAddress(albert_payment_addr),
            );
            shielded_ctx.shell.execute_tx(&shield_tx);
            shielded_ctx.shell.commit_masp_tx(shield_tx);
            shielded_ctx.shell.commit_block();
            shielded_ctx.generate_shielded_action(
                Amount::native_whole(10),
                TransferSource::ExtendedSpendingKey(albert_spending_key),
                TransferTarget::Address(defaults::bertha_address()),
            )
        }
        _ => panic!("Unexpected bench test"),
    }
}

fn ibc(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_ibc");

    // NOTE: Ibc encompass a variety of different messages that can be executed,
    // here we only benchmark a few of those Connection handshake

    for bench_name in [
        "open_connection",
        "open_channel",
        "outgoing_transfer",
        "outgoing_shielded_action",
    ] {
        // Initialize the state according to the target tx
        let (mut shielded_ctx, signed_tx) = prepare_ibc_tx_and_ctx(bench_name);

        let verifiers_from_tx = shielded_ctx.shell.execute_tx(&signed_tx);
        let (verifiers, keys_changed) = shielded_ctx
            .shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let ibc = Ibc {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Ibc),
                &shielded_ctx.shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shielded_ctx.shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    ibc.validate_tx(
                        &signed_tx,
                        ibc.ctx.keys_changed,
                        ibc.ctx.verifiers,
                    )
                    .is_ok()
                )
            })
        });
    }

    group.finish();
}

fn vp_multitoken(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_multitoken");
    let shell = BenchShell::default();

    let foreign_key_write =
        generate_foreign_key_tx(&defaults::albert_keypair());

    let transfer = shell.generate_tx(
        TX_TRANSFER_WASM,
        Transfer {
            source: defaults::albert_address(),
            target: defaults::bertha_address(),
            token: address::testing::nam(),
            amount: Amount::native_whole(1000).native_denominated(),
            shielded: None,
        },
        None,
        None,
        vec![&defaults::albert_keypair()],
    );

    for (signed_tx, bench_name) in [foreign_key_write, transfer]
        .iter()
        .zip(["foreign_key_write", "transfer"])
    {
        let mut shell = BenchShell::default();
        let verifiers_from_tx = shell.execute_tx(signed_tx);
        let (verifiers, keys_changed) = shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let multitoken = MultitokenVp {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Multitoken),
                &shell.state,
                signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    multitoken
                        .validate_tx(
                            signed_tx,
                            multitoken.ctx.keys_changed,
                            multitoken.ctx.verifiers,
                        )
                        .is_ok()
                )
            })
        });
    }
}

// Generate and run masp transaction to be verified. Returns the verifier set
// from tx and the tx.
fn setup_storage_for_masp_verification(
    bench_name: &str,
) -> (BenchShieldedCtx, BTreeSet<Address>, Tx) {
    let amount = Amount::native_whole(500);
    let mut shielded_ctx = BenchShieldedCtx::default();

    let albert_spending_key = shielded_ctx
        .wallet
        .find_spending_key(ALBERT_SPENDING_KEY, None)
        .unwrap()
        .to_owned();
    let albert_payment_addr = shielded_ctx
        .wallet
        .find_payment_addr(ALBERT_PAYMENT_ADDRESS)
        .unwrap()
        .to_owned();
    let bertha_payment_addr = shielded_ctx
        .wallet
        .find_payment_addr(BERTHA_PAYMENT_ADDRESS)
        .unwrap()
        .to_owned();

    // Shield some tokens for Albert
    let (mut shielded_ctx, shield_tx) = shielded_ctx.generate_masp_tx(
        amount,
        TransferSource::Address(defaults::albert_address()),
        TransferTarget::PaymentAddress(albert_payment_addr),
    );

    shielded_ctx.shell.execute_tx(&shield_tx);
    shielded_ctx.shell.commit_masp_tx(shield_tx);

    // Update the anchor in storage
    let tree_key = namada::token::storage_key::masp_commitment_tree_key();
    let updated_tree: CommitmentTree<Node> =
        shielded_ctx.shell.state.read(&tree_key).unwrap().unwrap();
    let anchor_key = namada::token::storage_key::masp_commitment_anchor_key(
        updated_tree.root(),
    );
    shielded_ctx.shell.state.write(&anchor_key, ()).unwrap();
    shielded_ctx.shell.commit_block();

    let (mut shielded_ctx, signed_tx) = match bench_name {
        "shielding" => shielded_ctx.generate_masp_tx(
            amount,
            TransferSource::Address(defaults::albert_address()),
            TransferTarget::PaymentAddress(albert_payment_addr),
        ),
        "unshielding" => shielded_ctx.generate_masp_tx(
            amount,
            TransferSource::ExtendedSpendingKey(albert_spending_key),
            TransferTarget::Address(defaults::albert_address()),
        ),
        "shielded" => shielded_ctx.generate_masp_tx(
            amount,
            TransferSource::ExtendedSpendingKey(albert_spending_key),
            TransferTarget::PaymentAddress(bertha_payment_addr),
        ),
        _ => panic!("Unexpected bench test"),
    };
    let verifiers_from_tx = shielded_ctx.shell.execute_tx(&signed_tx);

    (shielded_ctx, verifiers_from_tx, signed_tx)
}

fn masp(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_masp");

    for bench_name in ["shielding", "unshielding", "shielded"] {
        group.bench_function(bench_name, |b| {
            let (shielded_ctx, verifiers_from_tx, signed_tx) =
                setup_storage_for_masp_verification(bench_name);
            let (verifiers, keys_changed) = shielded_ctx
                .shell
                .state
                .write_log()
                .verifiers_and_changed_keys(&verifiers_from_tx);

            let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
                &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
            ));
            let masp = MaspVp {
                ctx: Ctx::new(
                    &Address::Internal(InternalAddress::Masp),
                    &shielded_ctx.shell.state,
                    &signed_tx,
                    &TxIndex(0),
                    &gas_meter,
                    &keys_changed,
                    &verifiers,
                    shielded_ctx.shell.vp_wasm_cache.clone(),
                ),
            };

            b.iter(|| {
                assert!(
                    masp.validate_tx(
                        &signed_tx,
                        masp.ctx.keys_changed,
                        masp.ctx.verifiers,
                    )
                    .is_ok()
                );
            })
        });
    }

    group.finish();
}

// Instead of benchmarking BatchValidator::check_bundle we benchmark the 4
// functions that are called internally for better resolution
fn masp_check_spend(c: &mut Criterion) {
    c.bench_function("vp_masp_check_spend", |b| {
        b.iter_batched(
            || {
                let (_, _verifiers_from_tx, signed_tx) =
                    setup_storage_for_masp_verification("shielded");

                let transaction = signed_tx
                    .sections
                    .into_iter()
                    .filter_map(|section| match section {
                        Section::MaspTx(transaction) => Some(transaction),
                        _ => None,
                    })
                    .collect::<Vec<Transaction>>()
                    .first()
                    .unwrap()
                    .to_owned();
                let spend = transaction
                    .sapling_bundle()
                    .unwrap()
                    .shielded_spends
                    .first()
                    .unwrap()
                    .to_owned();
                let ctx = SaplingVerificationContextInner::new();
                let tx_data = transaction.deref();
                // Partially deauthorize the transparent bundle
                let unauth_tx_data = partial_deauthorize(tx_data).unwrap();
                let txid_parts = unauth_tx_data.digest(TxIdDigester);
                let sighash = signature_hash(
                    &unauth_tx_data,
                    &SignableInput::Shielded,
                    &txid_parts,
                );
                let zkproof = masp_proofs::bellman::groth16::Proof::read(
                    spend.zkproof.as_slice(),
                )
                .unwrap();

                (ctx, spend, sighash, zkproof)
            },
            |(mut ctx, spend, sighash, zkproof)| {
                assert!(ctx.check_spend(
                    spend.cv,
                    spend.anchor,
                    &spend.nullifier.0,
                    PublicKey(spend.rk.0),
                    sighash.as_ref(),
                    spend.spend_auth_sig,
                    zkproof,
                    &mut (),
                    // We do sig and proofs verification in parallel, so just
                    // use dummy verifiers here
                    |_, _, _, _| true,
                    |_, _, _| true
                ));
            },
            BatchSize::SmallInput,
        )
    });
}

fn masp_check_convert(c: &mut Criterion) {
    c.bench_function("vp_masp_check_convert", |b| {
        b.iter_batched(
            || {
                let (_, _verifiers_from_tx, signed_tx) =
                    setup_storage_for_masp_verification("shielded");

                let transaction = signed_tx
                    .sections
                    .into_iter()
                    .filter_map(|section| match section {
                        Section::MaspTx(transaction) => Some(transaction),
                        _ => None,
                    })
                    .collect::<Vec<Transaction>>()
                    .first()
                    .unwrap()
                    .to_owned();
                let convert = transaction
                    .sapling_bundle()
                    .unwrap()
                    .shielded_converts
                    .first()
                    .unwrap()
                    .to_owned();
                let ctx = SaplingVerificationContextInner::new();
                let zkproof = masp_proofs::bellman::groth16::Proof::read(
                    convert.zkproof.as_slice(),
                )
                .unwrap();

                (ctx, convert, zkproof)
            },
            |(mut ctx, convert, zkproof)| {
                assert!(ctx.check_convert(
                    convert.cv,
                    convert.anchor,
                    zkproof,
                    &mut (),
                    // We do proofs verification in parallel, so just use dummy
                    // verifier here
                    |_, _, _| true,
                ));
            },
            BatchSize::SmallInput,
        )
    });
}

fn masp_check_output(c: &mut Criterion) {
    c.bench_function("masp_vp_check_output", |b| {
        b.iter_batched(
            || {
                let (_, _verifiers_from_tx, signed_tx) =
                    setup_storage_for_masp_verification("shielded");

                let transaction = signed_tx
                    .sections
                    .into_iter()
                    .filter_map(|section| match section {
                        Section::MaspTx(transaction) => Some(transaction),
                        _ => None,
                    })
                    .collect::<Vec<Transaction>>()
                    .first()
                    .unwrap()
                    .to_owned();
                let output = transaction
                    .sapling_bundle()
                    .unwrap()
                    .shielded_outputs
                    .first()
                    .unwrap()
                    .to_owned();
                let ctx = SaplingVerificationContextInner::new();
                let zkproof = masp_proofs::bellman::groth16::Proof::read(
                    output.zkproof.as_slice(),
                )
                .unwrap();
                let epk = masp_proofs::jubjub::ExtendedPoint::from_bytes(
                    &output.ephemeral_key.0,
                )
                .unwrap();

                (ctx, output, epk, zkproof)
            },
            |(mut ctx, output, epk, zkproof)| {
                assert!(ctx.check_output(
                    output.cv,
                    output.cmu,
                    epk,
                    zkproof,
                    // We do proofs verification in parallel, so just use dummy
                    // verifier here
                    |_, _| true
                ));
            },
            BatchSize::SmallInput,
        )
    });
}

fn masp_final_check(c: &mut Criterion) {
    let (_, _verifiers_from_tx, signed_tx) =
        setup_storage_for_masp_verification("shielded");

    let transaction = signed_tx
        .sections
        .into_iter()
        .filter_map(|section| match section {
            Section::MaspTx(transaction) => Some(transaction),
            _ => None,
        })
        .collect::<Vec<Transaction>>()
        .first()
        .unwrap()
        .to_owned();
    let sapling_bundle = transaction.sapling_bundle().unwrap();
    // Partially deauthorize the transparent bundle
    let unauth_tx_data = partial_deauthorize(transaction.deref()).unwrap();
    let txid_parts = unauth_tx_data.digest(TxIdDigester);
    let sighash =
        signature_hash(&unauth_tx_data, &SignableInput::Shielded, &txid_parts);
    let mut ctx = SaplingVerificationContextInner::new();

    // Check spends, converts and outputs before the final check
    assert!(sapling_bundle.shielded_spends.iter().all(|spend| {
        let zkproof = masp_proofs::bellman::groth16::Proof::read(
            spend.zkproof.as_slice(),
        )
        .unwrap();

        ctx.check_spend(
            spend.cv,
            spend.anchor,
            &spend.nullifier.0,
            PublicKey(spend.rk.0),
            sighash.as_ref(),
            spend.spend_auth_sig,
            zkproof,
            &mut (),
            |_, _, _, _| true,
            |_, _, _| true,
        )
    }));
    assert!(sapling_bundle.shielded_converts.iter().all(|convert| {
        let zkproof = masp_proofs::bellman::groth16::Proof::read(
            convert.zkproof.as_slice(),
        )
        .unwrap();
        ctx.check_convert(
            convert.cv,
            convert.anchor,
            zkproof,
            &mut (),
            |_, _, _| true,
        )
    }));
    assert!(sapling_bundle.shielded_outputs.iter().all(|output| {
        let zkproof = masp_proofs::bellman::groth16::Proof::read(
            output.zkproof.as_slice(),
        )
        .unwrap();
        let epk = masp_proofs::jubjub::ExtendedPoint::from_bytes(
            &output.ephemeral_key.0,
        )
        .unwrap();
        ctx.check_output(
            output.cv,
            output.cmu,
            epk.to_owned(),
            zkproof,
            |_, _| true,
        )
    }));

    c.bench_function("vp_masp_final_check", |b| {
        b.iter(|| {
            assert!(ctx.final_check(
                sapling_bundle.value_balance.clone(),
                sighash.as_ref(),
                sapling_bundle.authorization.binding_sig,
                // We do sig verification in parallel, so just use dummy
                // verifier here
                |_, _, _| true
            ))
        })
    });
}

#[derive(Debug)]
enum RequestedItem {
    Signature,
    Spend,
    Convert,
    Output,
}

// Removes the unneeded notes from the generated transaction and replicates the
// remaining ones if needed
fn customize_masp_tx_data(
    multi: bool,
    request: &RequestedItem,
) -> Option<(
    TransactionData<masp_primitives::transaction::Authorized>,
    Transaction,
)> {
    let (_, _, tx) = setup_storage_for_masp_verification("unshielding");
    let transaction = tx
        .sections
        .into_iter()
        .filter_map(|section| match section {
            Section::MaspTx(transaction) => Some(transaction),
            _ => None,
        })
        .collect::<Vec<Transaction>>()
        .first()
        .unwrap()
        .to_owned();
    let mut sapling_bundle = transaction.sapling_bundle().unwrap().to_owned();

    match request {
        RequestedItem::Signature => {
            if multi {
                // No multisig benchmark
                return None;
            }
            // ensure we only have one signature to verify (the binding one) and
            // no proofs
            sapling_bundle.shielded_spends.clear();
            sapling_bundle.shielded_converts.clear();
            sapling_bundle.shielded_outputs.clear();
            assert_eq!(sapling_bundle.shielded_spends.len(), 0);
            assert_eq!(sapling_bundle.shielded_converts.len(), 0);
            assert_eq!(sapling_bundle.shielded_outputs.len(), 0);
        }
        RequestedItem::Spend => {
            if multi {
                // ensure we only have two spend proofs
                sapling_bundle.shielded_spends = [
                    sapling_bundle.shielded_spends.clone(),
                    sapling_bundle.shielded_spends,
                ]
                .concat();
                sapling_bundle.shielded_outputs.clear();
                sapling_bundle.shielded_converts.clear();
                assert_eq!(sapling_bundle.shielded_spends.len(), 2);
            } else {
                // ensure we only have one spend proof
                sapling_bundle.shielded_outputs.clear();
                sapling_bundle.shielded_converts.clear();
                assert_eq!(sapling_bundle.shielded_spends.len(), 1);
            }
            assert_eq!(sapling_bundle.shielded_converts.len(), 0);
            assert_eq!(sapling_bundle.shielded_outputs.len(), 0);
        }
        RequestedItem::Convert => {
            if multi {
                // ensure we only have two convert proofs
                sapling_bundle.shielded_converts = [
                    sapling_bundle.shielded_converts.clone(),
                    sapling_bundle.shielded_converts,
                ]
                .concat();
                sapling_bundle.shielded_spends.clear();
                sapling_bundle.shielded_outputs.clear();
                assert_eq!(sapling_bundle.shielded_converts.len(), 2);
            } else {
                // ensure we only have one convert proof
                sapling_bundle.shielded_spends.clear();
                sapling_bundle.shielded_outputs.clear();
                assert_eq!(sapling_bundle.shielded_converts.len(), 1);
            }
            assert_eq!(sapling_bundle.shielded_spends.len(), 0);
            assert_eq!(sapling_bundle.shielded_outputs.len(), 0);
        }
        RequestedItem::Output => {
            // From the cost remove the cost of signature(s) validation
            if multi {
                // ensure we only have two output proofs
                sapling_bundle.shielded_outputs = [
                    sapling_bundle.shielded_outputs.clone(),
                    sapling_bundle.shielded_outputs,
                ]
                .concat();
                sapling_bundle.shielded_spends.clear();
                sapling_bundle.shielded_converts.clear();
                assert_eq!(sapling_bundle.shielded_outputs.len(), 2);
            } else {
                // ensure we only have one output proof
                sapling_bundle.shielded_spends.clear();
                sapling_bundle.shielded_converts.clear();
                assert_eq!(sapling_bundle.shielded_outputs.len(), 1);
            }
            assert_eq!(sapling_bundle.shielded_spends.len(), 0);
            assert_eq!(sapling_bundle.shielded_converts.len(), 0);
        }
    };

    Some((
        TransactionData::from_parts(
            transaction.version(),
            transaction.consensus_branch_id(),
            transaction.lock_time(),
            transaction.expiry_height(),
            transaction.transparent_bundle().cloned(),
            Some(sapling_bundle),
        ),
        transaction,
    ))
}

// For signatures: it's impossible to benchmark more than one signature without
// pulling in the cost of a proof, so we just benchmark the cost of one binding
// signature and then use its cost. We don't discount by the numebr of cores
// because there might be the need for a call to the fallback single
// verification.. For proofs: benchmarks both one and two proofs and take the
// difference as the variable cost for every proofs/sigs. Compute the cost of
// the non parallel parts (with the diff) and charge that if at least one note
// is present, then charge the variable cost multiplied by the number of notes
// and divided by the number of cores
fn masp_batch_validate(c: &mut Criterion) {
    let mut group = c.benchmark_group("masp_batch_validate");
    let PVKs {
        spend_vk,
        convert_vk,
        output_vk,
    } = preload_verifying_keys();

    for multi in [true, false] {
        for bench in [
            RequestedItem::Signature,
            RequestedItem::Spend,
            RequestedItem::Convert,
            RequestedItem::Output,
        ] {
            // From the cost of proofs remove the cost of signature(s)
            // validation
            let (tx_data, transaction) =
                if let Some(data) = customize_masp_tx_data(multi, &bench) {
                    data
                } else {
                    continue;
                };

            // Partially deauthorize the transparent bundle
            let unauth_tx_data =
                partial_deauthorize(transaction.deref()).unwrap();
            let txid_parts = unauth_tx_data.digest(TxIdDigester);
            let sighash = signature_hash(
                &unauth_tx_data,
                &SignableInput::Shielded,
                &txid_parts,
            )
            .as_ref()
            .to_owned();
            let sapling_bundle = tx_data.sapling_bundle().unwrap();

            let bench_name = if multi {
                format!("{:#?}_multi", bench)
            } else {
                format!("{:#?}_single", bench)
            };
            group.bench_function(bench_name, |b| {
                b.iter_batched(
                    || {
                        let mut ctx = BatchValidator::new();
                        // Check bundle first
                        if !ctx.check_bundle(sapling_bundle.to_owned(), sighash)
                        {
                            panic!("Failed check bundle");
                        }

                        ctx
                    },
                    |ctx| {
                        assert!(
                            ctx.validate(
                                spend_vk, convert_vk, output_vk, OsRng
                            )
                        )
                    },
                    BatchSize::SmallInput,
                )
            });
        }
    }
}

fn pgf(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_pgf");

    for bench_name in [
        "foreign_key_write",
        "remove_steward",
        "steward_inflation_rate",
    ] {
        let mut shell = BenchShell::default();
        namada::governance::pgf::storage::keys::stewards_handle()
            .insert(
                &mut shell.state,
                defaults::albert_address(),
                StewardDetail::base(defaults::albert_address()),
            )
            .unwrap();

        let signed_tx = match bench_name {
            "foreign_key_write" => {
                generate_foreign_key_tx(&defaults::albert_keypair())
            }
            "remove_steward" => shell.generate_tx(
                TX_RESIGN_STEWARD,
                defaults::albert_address(),
                None,
                None,
                vec![&defaults::albert_keypair()],
            ),
            "steward_inflation_rate" => {
                let data = namada::tx::data::pgf::UpdateStewardCommission {
                    steward: defaults::albert_address(),
                    commission: HashMap::from([(
                        defaults::albert_address(),
                        namada::core::dec::Dec::zero(),
                    )]),
                };
                shell.generate_tx(
                    TX_UPDATE_STEWARD_COMMISSION,
                    data,
                    None,
                    None,
                    vec![&defaults::albert_keypair()],
                )
            }
            _ => panic!("Unexpected bench test"),
        };

        // Run the tx to validate
        let verifiers_from_tx = shell.execute_tx(&signed_tx);

        let (verifiers, keys_changed) = shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let pgf = PgfVp {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Pgf),
                &shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    pgf.validate_tx(
                        &signed_tx,
                        pgf.ctx.keys_changed,
                        pgf.ctx.verifiers,
                    )
                    .is_ok()
                )
            })
        });
    }

    group.finish();
}

fn eth_bridge_nut(c: &mut Criterion) {
    if !is_bridge_comptime_enabled() {
        return;
    }

    let mut shell = BenchShell::default();
    let native_erc20_addres = read_native_erc20_address(&shell.state).unwrap();

    let signed_tx = {
        let data = PendingTransfer {
            transfer: namada::core::eth_bridge_pool::TransferToEthereum {
                kind:
                    namada::core::eth_bridge_pool::TransferToEthereumKind::Erc20,
                asset: native_erc20_addres,
                recipient: namada::core::ethereum_events::EthAddress([1u8; 20]),
                sender: defaults::albert_address(),
                amount: Amount::from(1),
            },
            gas_fee: GasFee {
                amount: Amount::from(100),
                payer: defaults::albert_address(),
                token: shell.state.in_mem().native_token.clone(),
            },
        };
        shell.generate_tx(
            TX_BRIDGE_POOL_WASM,
            data,
            None,
            None,
            vec![&defaults::albert_keypair()],
        )
    };

    // Run the tx to validate
    let verifiers_from_tx = shell.execute_tx(&signed_tx);

    let (verifiers, keys_changed) = shell
        .state
        .write_log()
        .verifiers_and_changed_keys(&verifiers_from_tx);

    let vp_address =
        Address::Internal(InternalAddress::Nut(native_erc20_addres));
    let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
        &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
    ));
    let nut = NonUsableTokens {
        ctx: Ctx::new(
            &vp_address,
            &shell.state,
            &signed_tx,
            &TxIndex(0),
            &gas_meter,
            &keys_changed,
            &verifiers,
            shell.vp_wasm_cache.clone(),
        ),
    };

    c.bench_function("vp_eth_bridge_nut", |b| {
        b.iter(|| {
            assert!(
                nut.validate_tx(
                    &signed_tx,
                    nut.ctx.keys_changed,
                    nut.ctx.verifiers,
                )
                .is_ok()
            )
        })
    });
}

fn eth_bridge(c: &mut Criterion) {
    if !is_bridge_comptime_enabled() {
        return;
    }

    let mut shell = BenchShell::default();
    let native_erc20_addres = read_native_erc20_address(&shell.state).unwrap();

    let signed_tx = {
        let data = PendingTransfer {
            transfer: namada::core::eth_bridge_pool::TransferToEthereum {
                kind:
                    namada::core::eth_bridge_pool::TransferToEthereumKind::Erc20,
                asset: native_erc20_addres,
                recipient: namada::core::ethereum_events::EthAddress([1u8; 20]),
                sender: defaults::albert_address(),
                amount: Amount::from(1),
            },
            gas_fee: GasFee {
                amount: Amount::from(100),
                payer: defaults::albert_address(),
                token: shell.state.in_mem().native_token.clone(),
            },
        };
        shell.generate_tx(
            TX_BRIDGE_POOL_WASM,
            data,
            None,
            None,
            vec![&defaults::albert_keypair()],
        )
    };

    // Run the tx to validate
    let verifiers_from_tx = shell.execute_tx(&signed_tx);

    let (verifiers, keys_changed) = shell
        .state
        .write_log()
        .verifiers_and_changed_keys(&verifiers_from_tx);

    let vp_address = Address::Internal(InternalAddress::EthBridge);
    let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
        &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
    ));
    let eth_bridge = EthBridge {
        ctx: Ctx::new(
            &vp_address,
            &shell.state,
            &signed_tx,
            &TxIndex(0),
            &gas_meter,
            &keys_changed,
            &verifiers,
            shell.vp_wasm_cache.clone(),
        ),
    };

    c.bench_function("vp_eth_bridge", |b| {
        b.iter(|| {
            assert!(
                eth_bridge
                    .validate_tx(
                        &signed_tx,
                        eth_bridge.ctx.keys_changed,
                        eth_bridge.ctx.verifiers,
                    )
                    .is_ok()
            )
        })
    });
}

fn eth_bridge_pool(c: &mut Criterion) {
    if !is_bridge_comptime_enabled() {
        return;
    }

    // NOTE: this vp is one of the most expensive but its cost comes from the
    // numerous accesses to storage that we already account for, so no need to
    // benchmark specific sections of it like for the ibc native vp
    let mut shell = BenchShell::default();
    let native_erc20_addres = read_native_erc20_address(&shell.state).unwrap();

    // Whitelist NAM token
    let cap_key = whitelist::Key {
        asset: native_erc20_addres,
        suffix: whitelist::KeyType::Cap,
    }
    .into();
    shell.state.write(&cap_key, Amount::from(1_000)).unwrap();

    let whitelisted_key = whitelist::Key {
        asset: native_erc20_addres,
        suffix: whitelist::KeyType::Whitelisted,
    }
    .into();
    shell.state.write(&whitelisted_key, true).unwrap();

    let denom_key = whitelist::Key {
        asset: native_erc20_addres,
        suffix: whitelist::KeyType::Denomination,
    }
    .into();
    shell.state.write(&denom_key, 0).unwrap();

    let signed_tx = {
        let data = PendingTransfer {
            transfer: namada::core::eth_bridge_pool::TransferToEthereum {
                kind:
                    namada::core::eth_bridge_pool::TransferToEthereumKind::Erc20,
                asset: native_erc20_addres,
                recipient: namada::core::ethereum_events::EthAddress([1u8; 20]),
                sender: defaults::albert_address(),
                amount: Amount::from(1),
            },
            gas_fee: GasFee {
                amount: Amount::from(100),
                payer: defaults::albert_address(),
                token: shell.state.in_mem().native_token.clone(),
            },
        };
        shell.generate_tx(
            TX_BRIDGE_POOL_WASM,
            data,
            None,
            None,
            vec![&defaults::albert_keypair()],
        )
    };

    // Run the tx to validate
    let verifiers_from_tx = shell.execute_tx(&signed_tx);

    let (verifiers, keys_changed) = shell
        .state
        .write_log()
        .verifiers_and_changed_keys(&verifiers_from_tx);

    let vp_address = Address::Internal(InternalAddress::EthBridgePool);
    let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
        &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
    ));
    let bridge_pool = BridgePoolVp {
        ctx: Ctx::new(
            &vp_address,
            &shell.state,
            &signed_tx,
            &TxIndex(0),
            &gas_meter,
            &keys_changed,
            &verifiers,
            shell.vp_wasm_cache.clone(),
        ),
    };

    c.bench_function("vp_eth_bridge_pool", |b| {
        b.iter(|| {
            assert!(
                bridge_pool
                    .validate_tx(
                        &signed_tx,
                        bridge_pool.ctx.keys_changed,
                        bridge_pool.ctx.verifiers,
                    )
                    .is_ok()
            )
        })
    });
}

fn parameters(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_parameters");

    for bench_name in ["foreign_key_write", "parameter_change"] {
        let mut shell = BenchShell::default();

        let (verifiers_from_tx, signed_tx) = match bench_name {
            "foreign_key_write" => {
                let tx = generate_foreign_key_tx(&defaults::albert_keypair());
                // Run the tx to validate
                let verifiers_from_tx = shell.execute_tx(&tx);
                (verifiers_from_tx, tx)
            }
            "parameter_change" => {
                // Simulate governance proposal to modify a parameter
                let min_proposal_fund_key =
            namada::governance::storage::keys::get_min_proposal_fund_key();
                shell.state.write(&min_proposal_fund_key, 1_000).unwrap();

                let proposal_key = namada::governance::storage::keys::get_proposal_execution_key(0);
                shell.state.write(&proposal_key, 0).unwrap();

                // Return a dummy tx for validation
                let mut tx = Tx::from_type(namada::tx::data::TxType::Raw);
                tx.set_data(namada::tx::Data::new(borsh::to_vec(&0).unwrap()));
                let verifiers_from_tx = BTreeSet::default();
                (verifiers_from_tx, tx)
            }
            _ => panic!("Unexpected bench test"),
        };

        let (verifiers, keys_changed) = shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let vp_address = Address::Internal(InternalAddress::Parameters);
        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let parameters = ParametersVp {
            ctx: Ctx::new(
                &vp_address,
                &shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    parameters
                        .validate_tx(
                            &signed_tx,
                            parameters.ctx.keys_changed,
                            parameters.ctx.verifiers,
                        )
                        .is_ok()
                )
            })
        });
    }

    group.finish();
}

fn pos(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_pos");

    for bench_name in ["foreign_key_write", "parameter_change"] {
        let mut shell = BenchShell::default();

        let (verifiers_from_tx, signed_tx) = match bench_name {
            "foreign_key_write" => {
                let tx = generate_foreign_key_tx(&defaults::albert_keypair());
                // Run the tx to validate
                let verifiers_from_tx = shell.execute_tx(&tx);
                (verifiers_from_tx, tx)
            }
            "parameter_change" => {
                // Simulate governance proposal to modify a parameter
                let min_proposal_fund_key =
            namada::governance::storage::keys::get_min_proposal_fund_key();
                shell.state.write(&min_proposal_fund_key, 1_000).unwrap();

                let proposal_key = namada::governance::storage::keys::get_proposal_execution_key(0);
                shell.state.write(&proposal_key, 0).unwrap();

                // Return a dummy tx for validation
                let mut tx = Tx::from_type(namada::tx::data::TxType::Raw);
                tx.set_data(namada::tx::Data::new(borsh::to_vec(&0).unwrap()));
                let verifiers_from_tx = BTreeSet::default();
                (verifiers_from_tx, tx)
            }
            _ => panic!("Unexpected bench test"),
        };

        let (verifiers, keys_changed) = shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let vp_address = Address::Internal(InternalAddress::PoS);
        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let pos = PosVP {
            ctx: Ctx::new(
                &vp_address,
                &shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shell.vp_wasm_cache.clone(),
            ),
        };

        group.bench_function(bench_name, |b| {
            b.iter(|| {
                assert!(
                    pos.validate_tx(
                        &signed_tx,
                        pos.ctx.keys_changed,
                        pos.ctx.verifiers,
                    )
                    .is_ok()
                )
            })
        });
    }

    group.finish();
}

fn ibc_vp_validate_action(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_ibc_validate_action");

    for bench_name in [
        "open_connection",
        "open_channel",
        "outgoing_transfer",
        "outgoing_shielded_action",
    ] {
        let (mut shielded_ctx, signed_tx) = prepare_ibc_tx_and_ctx(bench_name);

        let verifiers_from_tx = shielded_ctx.shell.execute_tx(&signed_tx);
        let tx_data = signed_tx.data().unwrap();
        let (verifiers, keys_changed) = shielded_ctx
            .shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let ibc = Ibc {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Ibc),
                &shielded_ctx.shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shielded_ctx.shell.vp_wasm_cache.clone(),
            ),
        };
        // Use an empty verifiers set placeholder for validation, this is only
        // needed in actual txs to addresses whose VPs should be triggered
        let verifiers = Rc::new(RefCell::new(BTreeSet::<Address>::new()));

        let exec_ctx = PseudoExecutionContext::new(ibc.ctx.pre());
        let ctx = Rc::new(RefCell::new(exec_ctx));
        let mut actions = IbcActions::new(ctx.clone(), verifiers.clone());
        actions.set_validation_params(ibc.validation_params().unwrap());

        let module = TransferModule::new(ctx.clone(), verifiers);
        actions.add_transfer_module(module);
        let module = NftTransferModule::new(ctx);
        actions.add_transfer_module(module);

        group.bench_function(bench_name, |b| {
            b.iter(|| actions.validate(&tx_data).unwrap())
        });
    }

    group.finish();
}

fn ibc_vp_execute_action(c: &mut Criterion) {
    let mut group = c.benchmark_group("vp_ibc_execute_action");

    for bench_name in [
        "open_connection",
        "open_channel",
        "outgoing_transfer",
        "outgoing_shielded_action",
    ] {
        let (mut shielded_ctx, signed_tx) = prepare_ibc_tx_and_ctx(bench_name);

        let verifiers_from_tx = shielded_ctx.shell.execute_tx(&signed_tx);
        let tx_data = signed_tx.data().unwrap();
        let (verifiers, keys_changed) = shielded_ctx
            .shell
            .state
            .write_log()
            .verifiers_and_changed_keys(&verifiers_from_tx);

        let gas_meter = RefCell::new(VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(u64::MAX.into()),
        ));
        let ibc = Ibc {
            ctx: Ctx::new(
                &Address::Internal(InternalAddress::Ibc),
                &shielded_ctx.shell.state,
                &signed_tx,
                &TxIndex(0),
                &gas_meter,
                &keys_changed,
                &verifiers,
                shielded_ctx.shell.vp_wasm_cache.clone(),
            ),
        };
        // Use an empty verifiers set placeholder for validation, this is only
        // needed in actual txs to addresses whose VPs should be triggered
        let verifiers = Rc::new(RefCell::new(BTreeSet::<Address>::new()));

        let exec_ctx = PseudoExecutionContext::new(ibc.ctx.pre());
        let ctx = Rc::new(RefCell::new(exec_ctx));

        let mut actions = IbcActions::new(ctx.clone(), verifiers.clone());
        actions.set_validation_params(ibc.validation_params().unwrap());

        let module = TransferModule::new(ctx.clone(), verifiers);
        actions.add_transfer_module(module);
        let module = NftTransferModule::new(ctx);
        actions.add_transfer_module(module);

        group.bench_function(bench_name, |b| {
            b.iter(|| actions.execute(&tx_data).unwrap())
        });
    }

    group.finish();
}

criterion_group!(
    native_vps,
    governance,
    // slash_fund,
    ibc,
    masp,
    masp_check_spend,
    masp_check_convert,
    masp_check_output,
    masp_final_check,
    masp_batch_validate,
    vp_multitoken,
    pgf,
    eth_bridge_nut,
    eth_bridge,
    eth_bridge_pool,
    parameters,
    pos,
    ibc_vp_validate_action,
    ibc_vp_execute_action
);
criterion_main!(native_vps);
