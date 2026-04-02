use crate::l1::{
    bindings::{BlobReference, Multicall, ProofType, ProposeInput, RealTimeInbox, SubProof},
    config::ContractAddresses,
};
use crate::node::proposal_manager::{
    bridge_handler::{L1Call, UserOp},
    proposal::Proposal,
};
use crate::shared_abi::bindings::Bridge;
use alloy::{
    consensus::SidecarBuilder,
    eips::eip4844::BlobTransactionSidecar,
    network::TransactionBuilder4844,
    primitives::{
        Address, Bytes, U256,
        aliases::{U24, U48},
    },
    providers::{DynProvider, Provider},
    rpc::types::TransactionRequest,
    sol_types::SolValue,
};
use anyhow::Error;
use common::l1::fees_per_gas::FeesPerGas;
use taiko_protocol::shasta::{
    BlobCoder,
    manifest::{BlockManifest, DerivationSourceManifest},
};
use tracing::{info, warn};

pub struct ProposalTxBuilder {
    provider: DynProvider,
    extra_gas_percentage: u64,
    proof_type: ProofType,
}

impl ProposalTxBuilder {
    pub fn new(provider: DynProvider, extra_gas_percentage: u64, proof_type: ProofType) -> Self {
        Self {
            provider,
            extra_gas_percentage,
            proof_type,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn build_propose_tx(
        &self,
        batch: Proposal,
        from: Address,
        contract_addresses: ContractAddresses,
    ) -> Result<TransactionRequest, Error> {
        let tx_blob = self
            .build_propose_blob(batch, from, contract_addresses)
            .await?;
        let tx_blob_gas = match self.provider.estimate_gas(tx_blob.clone()).await {
            Ok(gas) => gas,
            Err(e) => {
                warn!(
                    "Build proposeBatch: Failed to estimate gas for blob transaction: {}. Force-sending with 500000 gas.",
                    e
                );
                5_000_000
            }
        };
        let tx_blob_gas = tx_blob_gas + tx_blob_gas * self.extra_gas_percentage / 100;

        let fees_per_gas = match FeesPerGas::get_fees_per_gas(&self.provider).await {
            Ok(fees_per_gas) => fees_per_gas,
            Err(e) => {
                warn!("Build proposeBatch: Failed to get fees per gas: {}", e);
                return Ok(tx_blob);
            }
        };

        let tx_blob = fees_per_gas.update_eip4844(tx_blob, tx_blob_gas);

        Ok(tx_blob)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn build_propose_blob(
        &self,
        batch: Proposal,
        from: Address,
        contract_addresses: ContractAddresses,
    ) -> Result<TransactionRequest, Error> {
        let mut multicalls: Vec<Multicall::Call> = vec![];

        // Build the propose call and blob sidecar
        let (propose_call, blob_sidecar) = self
            .build_propose_call(&batch, contract_addresses.realtime_inbox)
            .await?;

        // If no user ops or L1 calls, send directly to inbox (skip multicall)
        if batch.user_ops.is_empty() && batch.l1_calls.is_empty() {
            info!("Sending proposal directly to RealTimeInbox (no multicall)");
            let tx = TransactionRequest::default()
                .to(contract_addresses.realtime_inbox)
                .from(from)
                .input(propose_call.data.into())
                .with_blob_sidecar(blob_sidecar);
            return Ok(tx);
        }

        // Check if any user op targets a SyncL1Proxy (has a ProofStore configured).
        // If so, use the sync multicall structure:
        //   [ProofStore.store(propose+processMessage), user_ops...]
        // Otherwise, use the standard structure:
        //   [user_ops..., propose, l1_calls...]
        let sync_proof_store = std::env::var("SYNC_PROOF_STORE_ADDRESS").ok();

        if let Some(proof_store_hex) = sync_proof_store {
            let proof_store_addr: Address = proof_store_hex.parse()
                .map_err(|e| Error::msg(format!("Invalid SYNC_PROOF_STORE_ADDRESS: {e}")))?;

            info!("🔄 SYNC MODE V2: ProofStore with direct return verification");

            let call_id = U256::ZERO;

            // Check if SYNC_MODE_V2 is set — use ProofStoreV2 (propose + return proof)
            let use_v2 = std::env::var("SYNC_MODE_V2").is_ok();

            if use_v2 {
                // V2: Store propose calldata + return message hash + hop proof
                // No processMessage needed — proxy verifies return via SignalService directly
                let (return_msg_id, return_success, return_data, hop_proof) =
                    if let Some(l1_call) = batch.l1_calls.first() {
                        let return_msg_id = l1_call.message_from_l2.id;
                        // Decode the message data to extract the inner return value.
                        // msg.data = onMessageInvocation(abi.encode(callId, success, retData))
                        // Layout: 4 (selector) + 32 (offset) + 32 (length) + payload
                        // payload = abi.encode(uint256 callId, bool success, bytes retData)
                        let msg_data = l1_call.message_from_l2.data.clone();
                        let (decoded_success, decoded_ret_data) = {
                            // Skip onMessageInvocation selector (4) + ABI bytes wrapper (offset 32 + length 32)
                            if msg_data.len() > 68 {
                                let inner = &msg_data[68..]; // abi.encode(callId, success, retData, l1Origin)
                                // callId at [0..32], success at [32..64], retData offset at [64..96], l1Origin at [96..128]
                                if inner.len() >= 96 {
                                    let success_word = inner[32..64].iter().any(|&b| b != 0);
                                    let data_offset = u64::from_be_bytes(
                                        inner[88..96].try_into().unwrap_or([0u8; 8])
                                    ) as usize;
                                    if data_offset < inner.len() && inner.len() >= data_offset + 32 {
                                        let data_len = u64::from_be_bytes(
                                            inner[data_offset + 24..data_offset + 32].try_into().unwrap_or([0u8; 8])
                                        ) as usize;
                                        let data_start = data_offset + 32;
                                        if data_start + data_len <= inner.len() {
                                            (success_word, Bytes::copy_from_slice(&inner[data_start..data_start + data_len]))
                                        } else {
                                            tracing::warn!("Return data decode failed: data slice out of bounds");
                                            (false, Bytes::new())
                                        }
                                    } else {
                                        tracing::warn!("Return data decode failed: invalid data offset");
                                        (false, Bytes::new())
                                    }
                                } else {
                                    tracing::warn!("Return data decode failed: inner payload too short ({})", msg_data.len());
                                    (false, Bytes::new())
                                }
                            } else {
                                tracing::warn!("Return data decode failed: msg_data too short ({})", msg_data.len());
                                (false, Bytes::new())
                            }
                        };
                        tracing::info!(
                            "Decoded return: success={}, data_len={}",
                            decoded_success,
                            decoded_ret_data.len()
                        );
                        (
                            return_msg_id,
                            decoded_success,
                            decoded_ret_data,
                            l1_call.signal_slot_proof.clone(),
                        )
                    } else {
                        (0u64, false, Bytes::new(), Bytes::new())
                    };

                let store_calldata = alloy::sol_types::SolCall::abi_encode(&ProofStoreV2Store {
                    callId: call_id,
                    proposeTarget: contract_addresses.realtime_inbox,
                    proposeCalldata: propose_call.data.clone(),
                    returnMsgId: return_msg_id,
                    returnSuccess: return_success,
                    returnData: return_data,
                    hopProof: hop_proof,
                });

                multicalls.push(Multicall::Call {
                    target: proof_store_addr,
                    value: U256::ZERO,
                    data: Bytes::from(store_calldata),
                });
                info!("Added ProofStoreV2.store() to Multicall (V2 sync mode, no processMessage)");
            } else {
                // V1: Store propose + processMessage calldata
                let process_message_calls: Vec<Multicall::Call> = batch.l1_calls.iter()
                    .map(|l1_call| self.build_l1_call_call(l1_call.clone(), contract_addresses.bridge))
                    .collect();

                let process_msg_data = if let Some(first_l1_call) = process_message_calls.first() {
                    first_l1_call.data.clone()
                } else {
                    Bytes::new()
                };

                let store_calldata = alloy::sol_types::SolCall::abi_encode(&ProofStoreStore {
                    callId: call_id,
                    proposeTarget: contract_addresses.realtime_inbox,
                    proposeCalldata: propose_call.data.clone(),
                    processMessageTarget: contract_addresses.bridge,
                    processMessageCalldata: process_msg_data,
                });

                multicalls.push(Multicall::Call {
                    target: proof_store_addr,
                    value: U256::ZERO,
                    data: Bytes::from(store_calldata),
                });
                info!("Added ProofStore.store() to Multicall (V1 sync mode)");
            }

            // Then add user ops
            for user_op in &batch.user_ops {
                let user_op_call = self.build_user_op_call(user_op.clone());
                info!("Added user op to Multicall: {:?}", &user_op_call);
                multicalls.push(user_op_call);
            }
        } else {
            // Standard (async) mode: [user_ops..., propose, l1_calls...]
            for user_op in &batch.user_ops {
                let user_op_call = self.build_user_op_call(user_op.clone());
                info!("Added user op to Multicall: {:?}", &user_op_call);
                multicalls.push(user_op_call);
            }

            info!("Added proposal to Multicall: {:?}", &propose_call);
            multicalls.push(propose_call.clone());

            // Add all L1 calls
            for l1_call in &batch.l1_calls {
                let l1_call_call = self.build_l1_call_call(l1_call.clone(), contract_addresses.bridge);
                info!("Added L1 call to Multicall: {:?}", &l1_call_call);
                multicalls.push(l1_call_call);
            }
        }

        let multicall = Multicall::new(contract_addresses.proposer_multicall, &self.provider);
        let call = multicall.multicall(multicalls);

        let tx = TransactionRequest::default()
            .to(contract_addresses.proposer_multicall)
            .from(from)
            .input(call.calldata().clone().into())
            .with_blob_sidecar(blob_sidecar);

        Ok(tx)
    }

    fn build_user_op_call(&self, user_op_data: UserOp) -> Multicall::Call {
        Multicall::Call {
            target: user_op_data.submitter,
            value: U256::ZERO,
            data: user_op_data.calldata,
        }
    }

    async fn build_propose_call(
        &self,
        batch: &Proposal,
        inbox_address: Address,
    ) -> Result<(Multicall::Call, BlobTransactionSidecar), anyhow::Error> {
        let mut block_manifests = <Vec<BlockManifest>>::with_capacity(batch.l2_blocks.len());
        for l2_block in &batch.l2_blocks {
            block_manifests.push(BlockManifest {
                timestamp: l2_block.timestamp_sec,
                coinbase: l2_block.coinbase,
                anchor_block_number: l2_block.anchor_block_number,
                gas_limit: l2_block.gas_limit_without_anchor,
                transactions: l2_block
                    .prebuilt_tx_list
                    .tx_list
                    .iter()
                    .map(|tx| tx.clone().into())
                    .collect(),
            });
        }

        let manifest = DerivationSourceManifest {
            blocks: block_manifests,
        };

        let manifest_data = manifest
            .encode_and_compress()
            .map_err(|e| Error::msg(format!("Can't encode and compress manifest: {e}")))?;

        let sidecar_builder: SidecarBuilder<BlobCoder> = SidecarBuilder::from_slice(&manifest_data);
        let sidecar: BlobTransactionSidecar = sidecar_builder.build()?;

        let inbox = RealTimeInbox::new(inbox_address, self.provider.clone());

        // Encode the raw proof as SubProof[] for the SurgeVerifier
        let raw_proof = batch
            .zk_proof
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ZK proof not set on proposal"))?
            .clone();

        let sub_proofs = vec![SubProof {
            proofBitFlag: self.proof_type.proof_bit_flag(),
            data: Bytes::from(raw_proof),
        }];
        let proof = Bytes::from(sub_proofs.abi_encode());

        // Build ProposeInput and ABI-encode it as the _data parameter
        let blob_reference = BlobReference {
            blobStartIndex: 0,
            numBlobs: sidecar.blobs.len().try_into()?,
            offset: U24::ZERO,
        };

        let propose_input = ProposeInput {
            blobReference: blob_reference,
            signalSlots: batch.signal_slots.clone(),
            maxAnchorBlockNumber: U48::from(batch.max_anchor_block_number),
        };

        let encoded_input = Bytes::from(propose_input.abi_encode());

        // Convert L1 Checkpoint type for the propose call
        let checkpoint = crate::l1::bindings::ICheckpointStore::Checkpoint {
            blockNumber: batch.checkpoint.blockNumber,
            blockHash: batch.checkpoint.blockHash,
            stateRoot: batch.checkpoint.stateRoot,
        };

        let call = inbox.propose(encoded_input, checkpoint, proof);

        Ok((
            Multicall::Call {
                target: inbox_address,
                value: U256::ZERO,
                data: call.calldata().clone(),
            },
            sidecar,
        ))
    }

    fn build_l1_call_call(&self, l1_call: L1Call, bridge_address: Address) -> Multicall::Call {
        let bridge = Bridge::new(bridge_address, &self.provider);
        let call = bridge.processMessage(l1_call.message_from_l2, l1_call.signal_slot_proof);

        Multicall::Call {
            target: bridge_address,
            value: U256::ZERO,
            data: call.calldata().clone(),
        }
    }
}

// ABI binding for ProofStore V1 store()
alloy::sol! {
    #[sol(rpc)]
    function store(
        uint256 callId,
        address proposeTarget,
        bytes calldata proposeCalldata,
        address processMessageTarget,
        bytes calldata processMessageCalldata
    ) external;
}
type ProofStoreStore = storeCall;

// ABI binding for ProofStoreV2.store() — 7-param version
// Note: function name must be unique in the sol! scope, so we use a module.
mod proof_store_v2_abi {
    alloy::sol! {
        function store(
            uint256 callId,
            address proposeTarget,
            bytes calldata proposeCalldata,
            uint64 returnMsgId,
            bool returnSuccess,
            bytes calldata returnData,
            bytes calldata hopProof
        ) external;
    }
}
type ProofStoreV2Store = proof_store_v2_abi::storeCall;

