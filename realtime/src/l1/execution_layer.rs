use super::config::EthereumL1Config;
use super::proposal_tx_builder::ProposalTxBuilder;
use super::protocol_config::ProtocolConfig;
use crate::l1::bindings::RealTimeInbox::{self, RealTimeInboxInstance};
use crate::node::proposal_manager::proposal::Proposal;
use crate::raiko::RaikoClient;
use crate::shared_abi::bindings::{
    Bridge::MessageSent, IBridge::Message, SignalService::SignalSent,
};
use crate::{l1::config::ContractAddresses, node::proposal_manager::bridge_handler::UserOp};
use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{Address, B256, Bytes, FixedBytes},
    providers::{DynProvider, ext::DebugApi},
    rpc::types::{
        TransactionRequest,
        trace::geth::{
            GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingCallOptions,
            GethDebugTracingOptions,
        },
    },
    sol_types::SolEvent,
};
use anyhow::{Error, anyhow};
use common::{
    l1::{
        traits::{ELTrait, PreconferProvider},
        transaction_error::TransactionError,
    },
    metrics::Metrics,
    shared::{
        alloy_tools, execution_layer::ExecutionLayer as ExecutionLayerCommon,
        transaction_monitor::TransactionMonitor,
    },
};
use pacaya::l1::{operators_cache::OperatorError, traits::PreconfOperator};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tracing::info;

pub struct ExecutionLayer {
    common: ExecutionLayerCommon,
    provider: DynProvider,
    preconfer_address: Address,
    pub transaction_monitor: TransactionMonitor,
    contract_addresses: ContractAddresses,
    realtime_inbox: RealTimeInboxInstance<DynProvider>,
    #[allow(dead_code)]
    raiko_client: RaikoClient,
    proof_type: crate::l1::bindings::ProofType,
}

impl ELTrait for ExecutionLayer {
    type Config = EthereumL1Config;
    async fn new(
        common_config: common::l1::config::EthereumL1Config,
        specific_config: Self::Config,
        transaction_error_channel: Sender<TransactionError>,
        metrics: Arc<Metrics>,
    ) -> Result<Self, Error> {
        let provider = alloy_tools::construct_alloy_provider(
            &common_config.signer,
            common_config
                .execution_rpc_urls
                .first()
                .ok_or_else(|| anyhow!("L1 RPC URL is required"))?,
        )
        .await?;
        let common =
            ExecutionLayerCommon::new(provider.clone(), common_config.signer.get_address()).await?;

        let transaction_monitor = TransactionMonitor::new(
            provider.clone(),
            &common_config,
            transaction_error_channel,
            metrics.clone(),
            common.chain_id(),
        )
        .await
        .map_err(|e| Error::msg(format!("Failed to create TransactionMonitor: {e}")))?;

        let realtime_inbox = RealTimeInbox::new(specific_config.realtime_inbox, provider.clone());

        let config = realtime_inbox
            .getConfig()
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to call getConfig for RealTimeInbox: {e}"))?;

        tracing::info!(
            "RealTimeInbox: {}, proofVerifier: {}, signalService: {}",
            specific_config.realtime_inbox,
            config.proofVerifier,
            config.signalService,
        );

        let contract_addresses = ContractAddresses {
            realtime_inbox: specific_config.realtime_inbox,
            proposer_multicall: specific_config.proposer_multicall,
            bridge: specific_config.bridge,
        };

        let proof_type = specific_config.proof_type;
        let raiko_client = specific_config.raiko_client;

        Ok(Self {
            common,
            provider,
            preconfer_address: common_config.signer.get_address(),
            transaction_monitor,
            contract_addresses,
            realtime_inbox,
            raiko_client,
            proof_type,
        })
    }

    fn common(&self) -> &ExecutionLayerCommon {
        &self.common
    }
}

impl PreconferProvider for ExecutionLayer {
    async fn get_preconfer_wallet_eth(&self) -> Result<alloy::primitives::U256, Error> {
        self.common()
            .get_account_balance(self.preconfer_address)
            .await
    }

    async fn get_preconfer_nonce_pending(&self) -> Result<u64, Error> {
        self.common()
            .get_account_nonce(self.preconfer_address, BlockNumberOrTag::Pending)
            .await
    }

    async fn get_preconfer_nonce_latest(&self) -> Result<u64, Error> {
        self.common()
            .get_account_nonce(self.preconfer_address, BlockNumberOrTag::Latest)
            .await
    }

    fn get_preconfer_address(&self) -> Address {
        self.preconfer_address
    }
}

impl PreconfOperator for ExecutionLayer {
    fn get_preconfer_address(&self) -> Address {
        self.preconfer_address
    }

    async fn get_operators_for_current_and_next_epoch(
        &self,
        _current_epoch_timestamp: u64,
        _current_slot_timestamp: u64,
    ) -> Result<(Address, Address), OperatorError> {
        // RealTime: anyone can propose, but we still use operator tracking for slot management.
        // Return self as both current and next operator.
        Ok((self.preconfer_address, self.preconfer_address))
    }

    async fn is_preconf_router_specified_in_taiko_wrapper(&self) -> Result<bool, Error> {
        Ok(true)
    }

    async fn get_l2_height_from_taiko_inbox(&self) -> Result<u64, Error> {
        Ok(0)
    }

    async fn get_handover_window_slots(&self) -> Result<u64, Error> {
        Err(anyhow::anyhow!(
            "Not implemented for RealTime execution layer"
        ))
    }
}

impl ExecutionLayer {
    #[allow(dead_code)]
    pub fn get_raiko_client(&self) -> &RaikoClient {
        &self.raiko_client
    }

    pub async fn send_batch_to_l1(
        &self,
        batch: Proposal,
        tx_hash_notifier: Option<tokio::sync::oneshot::Sender<alloy::primitives::B256>>,
        tx_result_notifier: Option<tokio::sync::oneshot::Sender<bool>>,
    ) -> Result<(), Error> {
        info!(
            "📦 Proposing with {} blocks | user_ops: {:?} | signal_slots: {:?} | l1_calls: {:?} | zk_proof: {}",
            batch.l2_blocks.len(),
            batch.user_ops,
            batch.signal_slots,
            batch.l1_calls,
            batch.zk_proof.is_some(),
        );

        let builder = ProposalTxBuilder::new(self.provider.clone(), 10, self.proof_type);

        let tx = builder
            .build_propose_tx(
                batch,
                self.preconfer_address,
                self.contract_addresses.clone(),
            )
            .await?;

        let pending_nonce = self.get_preconfer_nonce_pending().await?;
        self.transaction_monitor
            .monitor_new_transaction(tx, pending_nonce, tx_hash_notifier, tx_result_notifier)
            .await
            .map_err(|e| Error::msg(format!("Sending batch to L1 failed: {e}")))?;

        Ok(())
    }

    pub async fn is_transaction_in_progress(&self) -> Result<bool, Error> {
        self.transaction_monitor.is_transaction_in_progress().await
    }

    pub async fn fetch_protocol_config(&self) -> Result<ProtocolConfig, Error> {
        let config = self
            .realtime_inbox
            .getConfig()
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to call getConfig for RealTimeInbox: {e}"))?;

        info!(
            "RealTimeInbox config: basefeeSharingPctg: {}",
            config.basefeeSharingPctg,
        );

        Ok(ProtocolConfig::from(&config))
    }

    pub async fn get_last_finalized_block_hash(&self) -> Result<B256, Error> {
        let result = self
            .realtime_inbox
            .getLastFinalizedBlockHash()
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to call getLastFinalizedBlockHash: {e}"))?;

        Ok(result)
    }
}

// Surge: L1 EL ops for Bridge Handler

use alloy::rpc::types::trace::geth::{CallFrame, CallLogFrame};

fn collect_logs_recursive(frame: &CallFrame) -> Vec<CallLogFrame> {
    let mut logs = frame.logs.clone();

    for subcall in &frame.calls {
        logs.extend(collect_logs_recursive(subcall));
    }

    logs
}

/// Extract bridge Message and signal slot from call outputs when events are unavailable
/// (e.g. when the proxy reverts). Walks the call tree looking for sendMessage calls
/// and extracts the filled Message from the return data.
fn extract_bridge_from_call_outputs(frame: &CallFrame) -> Option<(Message, FixedBytes<32>)> {
    use alloy::sol_types::SolType;

    // sendMessage selector = 0x1bdb0037
    // sendSignal selector = 0x66ca2bc0
    let send_message_sel = [0x1bu8, 0xdb, 0x00, 0x37];
    let send_signal_sel = [0x66u8, 0xca, 0x2b, 0xc0];

    let mut message: Option<Message> = None;
    let mut slot: Option<FixedBytes<32>> = None;

    fn walk(
        frame: &CallFrame,
        send_message_sel: &[u8; 4],
        send_signal_sel: &[u8; 4],
        message: &mut Option<Message>,
        slot: &mut Option<FixedBytes<32>>,
    ) {
        let input = frame.input.as_ref();
        let output = frame.output.as_deref();

        // Check if this is a sendMessage CALL (not delegatecall)
        let is_call = frame.typ.as_str() == "CALL";
        if is_call && input.len() >= 4 && &input[..4] == send_message_sel {
            if let Some(out) = output {
                // sendMessage returns (bytes32 msgHash, Message memory message_)
                // Layout: [0..32] msgHash, [32..64] offset=0x40, [64..] Message ABI tuple
                if out.len() > 96 {
                    // Manual parsing: sendMessage returns (bytes32, Message)
                    // Message starts at offset 64 (0x40) in the output.
                    // Parse each field as a 32-byte word.
                    let msg_start = 64usize;
                    let word = |i: usize| -> [u8; 32] {
                        let start = msg_start + i * 32;
                        if start + 32 <= out.len() {
                            out[start..start + 32].try_into().unwrap_or([0u8; 32])
                        } else {
                            [0u8; 32]
                        }
                    };
                    let u64_from = |w: [u8; 32]| -> u64 {
                        u64::from_be_bytes(w[24..32].try_into().unwrap_or([0u8; 8]))
                    };
                    let u32_from = |w: [u8; 32]| -> u32 {
                        u32::from_be_bytes(w[28..32].try_into().unwrap_or([0u8; 4]))
                    };
                    let addr_from = |w: [u8; 32]| -> Address {
                        Address::from_slice(&w[12..32])
                    };

                    let id = u64_from(word(0));
                    let fee = u64_from(word(1));
                    let gas_limit = u32_from(word(2));
                    let from = addr_from(word(3));
                    let src_chain_id = u64_from(word(4));
                    let src_owner = addr_from(word(5));
                    let dest_chain_id = u64_from(word(6));
                    let dest_owner = addr_from(word(7));
                    let to = addr_from(word(8));
                    let value = alloy::primitives::U256::from_be_bytes(word(9));
                    // word(10) = offset to bytes data (relative to msg_start)
                    let data_offset = u64_from(word(10)) as usize;
                    let data_len_pos = msg_start + data_offset;
                    let data = if data_len_pos + 32 <= out.len() {
                        let data_len = u64_from(
                            out[data_len_pos..data_len_pos + 32].try_into().unwrap_or([0u8; 32])
                        ) as usize;
                        let data_start = data_len_pos + 32;
                        if data_start + data_len <= out.len() {
                            Bytes::copy_from_slice(&out[data_start..data_start + data_len])
                        } else {
                            Bytes::new()
                        }
                    } else {
                        Bytes::new()
                    };

                    let msg = Message {
                        id, fee, gasLimit: gas_limit, from, srcChainId: src_chain_id,
                        srcOwner: src_owner, destChainId: dest_chain_id,
                        destOwner: dest_owner, to, value, data,
                    };
                    tracing::info!(
                        "Extracted bridge message from sendMessage output: id={}, to={}, data_len={}",
                        msg.id, msg.to, msg.data.len()
                    );
                    *message = Some(msg);
                }
            }
        }

        // Check if this is a sendSignal call
        if input.len() >= 4 && &input[..4] == send_signal_sel {
            if let Some(out) = output {
                // sendSignal returns bytes32 (the slot)
                if out.len() >= 32 {
                    let mut slot_bytes = [0u8; 32];
                    slot_bytes.copy_from_slice(&out[..32]);
                    *slot = Some(FixedBytes::from(slot_bytes));
                }
            }
        }

        // Recurse into subcalls
        for sub in &frame.calls {
            walk(sub, send_message_sel, send_signal_sel, message, slot);
        }
    }

    walk(frame, &send_message_sel, &send_signal_sel, &mut message, &mut slot);

    if let (Some(m), Some(s)) = (message, slot) {
        Some((m, s))
    } else {
        None
    }
}

/// If calldata starts with Safe's execTransaction selector (0x6a761202),
/// extract the inner (to, data) so we can trace the proxy directly.
/// Otherwise return the original submitter + calldata unchanged.
pub fn extract_exec_transaction_inner(submitter: Address, calldata: &Bytes) -> (Address, Bytes) {
    // execTransaction selector = 0x6a761202
    if calldata.len() >= 4 && calldata[..4] == [0x6a, 0x76, 0x12, 0x02] {
        // ABI: execTransaction(address to, uint256 value, bytes data, ...)
        // to is at offset 4..36 (right-padded address in 32 bytes)
        // data is dynamic: offset at 4+64..4+96, then length+content at that offset
        if calldata.len() >= 4 + 3 * 32 {
            // Extract 'to' (first param, address in last 20 bytes of 32-byte word)
            let to_bytes: [u8; 20] = calldata[4 + 12..4 + 32].try_into().unwrap_or([0u8; 20]);
            let inner_to = Address::from(to_bytes);

            // Extract 'data' (third param, dynamic bytes)
            // Offset to data is at position 4 + 64..4 + 96
            if calldata.len() >= 4 + 96 {
                let data_offset_bytes: [u8; 32] = calldata[4 + 64..4 + 96]
                    .try_into()
                    .unwrap_or([0u8; 32]);
                let data_offset =
                    u64::from_be_bytes(data_offset_bytes[24..32].try_into().unwrap_or([0u8; 8]))
                        as usize;
                let abs_offset = 4 + data_offset;

                if calldata.len() >= abs_offset + 32 {
                    let data_len_bytes: [u8; 32] = calldata[abs_offset..abs_offset + 32]
                        .try_into()
                        .unwrap_or([0u8; 32]);
                    let data_len = u64::from_be_bytes(
                        data_len_bytes[24..32].try_into().unwrap_or([0u8; 8]),
                    ) as usize;
                    let data_start = abs_offset + 32;

                    if calldata.len() >= data_start + data_len {
                        let inner_data = Bytes::copy_from_slice(&calldata[data_start..data_start + data_len]);
                        tracing::info!(
                            "Extracted inner call from execTransaction: to={}, data_len={}",
                            inner_to,
                            data_len
                        );
                        return (inner_to, inner_data);
                    }
                }
            }
        }
    }
    // Fallback: trace the original call
    (submitter, calldata.clone())
}

pub trait L1BridgeHandlerOps {
    async fn find_message_and_signal_slot(
        &self,
        user_op: UserOp,
    ) -> Result<Option<(Message, FixedBytes<32>)>, anyhow::Error>;
}

impl L1BridgeHandlerOps for ExecutionLayer {
    async fn find_message_and_signal_slot(
        &self,
        user_op_data: UserOp,
    ) -> Result<Option<(Message, FixedBytes<32>)>, anyhow::Error> {
        // Extract inner target+calldata from Safe's execTransaction if possible.
        // This traces the proxy directly so bridge messages are visible even if
        // the proxy reverts (e.g. with ProofNotLoaded).
        let (trace_to, trace_input) = extract_exec_transaction_inner(
            user_op_data.submitter,
            &user_op_data.calldata,
        );

        let tx_request = TransactionRequest::default()
            .from(user_op_data.submitter)
            .to(trace_to)
            .input(trace_input.into());

        let mut tracer_config = serde_json::Map::new();
        tracer_config.insert("withLog".to_string(), serde_json::Value::Bool(true));
        tracer_config.insert("onlyTopCall".to_string(), serde_json::Value::Bool(false));

        let tracing_options = GethDebugTracingOptions {
            tracer: Some(GethDebugTracerType::BuiltInTracer(
                GethDebugBuiltInTracerType::CallTracer,
            )),
            tracer_config: serde_json::Value::Object(tracer_config).into(),
            ..Default::default()
        };

        let call_options = GethDebugTracingCallOptions {
            tracing_options,
            ..Default::default()
        };

        let trace_result = self
            .provider
            .debug_trace_call(
                tx_request,
                BlockId::Number(BlockNumberOrTag::Latest),
                call_options,
            )
            .await
            .map_err(|e| anyhow!("Failed to simulate executeBatch on L1: {e}"))?;

        tracing::debug!("Received trace result: {:?}", trace_result);

        let mut message: Option<Message> = None;
        let mut slot: Option<FixedBytes<32>> = None;

        if let alloy::rpc::types::trace::geth::GethTrace::CallTracer(ref call_frame) = trace_result {
            let all_logs = collect_logs_recursive(call_frame);
            tracing::debug!("Collected {} logs from call trace", all_logs.len());

            for log in all_logs {
                if let Some(topics) = &log.topics
                    && !topics.is_empty()
                {
                    if topics[0] == MessageSent::SIGNATURE_HASH {
                        let log_data = alloy::primitives::LogData::new_unchecked(
                            topics.clone(),
                            log.data.clone().unwrap_or_default(),
                        );
                        let decoded = MessageSent::decode_log_data(&log_data)
                            .map_err(|e| anyhow!("Failed to decode MessageSent event L1: {e}"))?;

                        message = Some(decoded.message);
                    } else if topics[0] == SignalSent::SIGNATURE_HASH {
                        let log_data = alloy::primitives::LogData::new_unchecked(
                            topics.clone(),
                            log.data.clone().unwrap_or_default(),
                        );
                        let decoded = SignalSent::decode_log_data(&log_data)
                            .map_err(|e| anyhow!("Failed to decode SignalSent event L1: {e}"))?;

                        slot = Some(decoded.slot);
                    }
                }
            }

            // Fallback: if no events (proxy reverted), extract from call outputs
            if message.is_none() || slot.is_none() {
                tracing::info!("No bridge events in logs, trying call output extraction...");
                if let Some((m, s)) = extract_bridge_from_call_outputs(call_frame) {
                    message = Some(m);
                    slot = Some(s);
                }
            }
        }

        tracing::debug!("{:?} {:?}", message, slot);

        if let (Some(message), Some(slot)) = (message, slot) {
            return Ok(Some((message, slot)));
        }

        Ok(None)
    }
}
