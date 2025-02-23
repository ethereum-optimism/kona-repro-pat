//! This module contains concrete implementations of the data provider traits, using an alloy
//! provider on the backend.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use alloy_consensus::{Header, Receipt, ReceiptWithBloom, TxEnvelope, TxType};
use alloy_primitives::{Bytes, B256, U64};
use alloy_provider::{Provider, ReqwestProvider};
use alloy_rlp::{Buf, Decodable};
use alloy_transport::TransportResult;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use core::num::NonZeroUsize;
use kona_primitives::{
    Block, BlockInfo, L2BlockInfo, L2ExecutionPayloadEnvelope, OpBlock, RollupConfig, SystemConfig,
};
use lru::LruCache;

use crate::traits::{ChainProvider, L2ChainProvider};

const CACHE_SIZE: usize = 16;

/// The [AlloyChainProvider] is a concrete implementation of the [ChainProvider] trait, providing
/// data over Ethereum JSON-RPC using an alloy provider as the backend.
///
/// **Note**:
/// This provider fetches data using the `debug_getRawHeader`, `debug_getRawReceipts`, and
/// `debug_getRawBlock` methods. The RPC must support this namespace.
#[derive(Debug, Clone)]
pub struct AlloyChainProvider {
    /// The inner Ethereum JSON-RPC provider.
    inner: ReqwestProvider,
    /// `header_by_hash` LRU cache.
    header_by_hash_cache: LruCache<B256, Header>,
    /// `block_info_by_number` LRU cache.
    block_info_by_number_cache: LruCache<u64, BlockInfo>,
    /// `block_info_by_number` LRU cache.
    receipts_by_hash_cache: LruCache<B256, Vec<Receipt>>,
    /// `block_info_and_transactions_by_hash` LRU cache.
    block_info_and_transactions_by_hash_cache: LruCache<B256, (BlockInfo, Vec<TxEnvelope>)>,
}

impl AlloyChainProvider {
    /// Creates a new [AlloyChainProvider] with the given alloy provider.
    pub fn new(inner: ReqwestProvider) -> Self {
        Self {
            inner,
            header_by_hash_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
            block_info_by_number_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
            receipts_by_hash_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
            block_info_and_transactions_by_hash_cache: LruCache::new(
                NonZeroUsize::new(CACHE_SIZE).unwrap(),
            ),
        }
    }

    /// Creates a new [AlloyChainProvider] from the provided [reqwest::Url].
    pub fn new_http(url: reqwest::Url) -> Self {
        let inner = ReqwestProvider::new_http(url);
        Self::new(inner)
    }

    /// Returns the chain ID.
    pub async fn chain_id(&mut self) -> Result<u64> {
        let chain_id: TransportResult<alloc::string::String> =
            self.inner.raw_request("eth_chainId".into(), ()).await;
        let chain_id = match chain_id {
            Ok(s) => alloc::string::String::from(s.trim_start_matches("0x")),
            Err(e) => return Err(anyhow!(e)),
        };
        u64::from_str_radix(&chain_id, 16).map_err(|e| anyhow!(e))
    }
}

#[async_trait]
impl ChainProvider for AlloyChainProvider {
    async fn header_by_hash(&mut self, hash: B256) -> Result<Header> {
        crate::inc!(PROVIDER_CALLS, &["chain_provider", "header_by_hash"]);
        crate::timer!(START, PROVIDER_RESPONSE_TIME, &["chain_provider", "header_by_hash"], timer);
        if let Some(header) = self.header_by_hash_cache.get(&hash) {
            return Ok(header.clone());
        }

        let raw_header: TransportResult<Bytes> =
            self.inner.raw_request("debug_getRawHeader".into(), [hash]).await;
        let raw_header: Bytes = match raw_header.map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["chain_provider", "header_by_hash", "debug_getRawHeader"]
                );
                return Err(e);
            }
        };
        match Header::decode(&mut raw_header.as_ref()).map_err(|e| anyhow!(e)) {
            Ok(header) => {
                self.header_by_hash_cache.put(hash, header.clone());
                Ok(header)
            }
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(PROVIDER_ERRORS, &["chain_provider", "header_by_hash", "decode"]);
                Err(e)
            }
        }
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo> {
        crate::inc!(PROVIDER_CALLS, &["chain_provider", "block_info_by_number"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["chain_provider", "block_info_by_number"],
            timer
        );
        if let Some(block_info) = self.block_info_by_number_cache.get(&number) {
            return Ok(*block_info);
        }

        let raw_header: TransportResult<Bytes> =
            self.inner.raw_request("debug_getRawHeader".into(), [U64::from(number)]).await;
        let raw_header: Bytes = match raw_header.map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["chain_provider", "block_info_by_number", "debug_getRawHeader"]
                );
                return Err(e);
            }
        };
        let header = match Header::decode(&mut raw_header.as_ref()).map_err(|e| anyhow!(e)) {
            Ok(h) => h,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(PROVIDER_ERRORS, &["chain_provider", "block_info_by_number", "decode"]);
                return Err(e);
            }
        };

        let block_info = BlockInfo {
            hash: header.hash_slow(),
            number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        };
        self.block_info_by_number_cache.put(number, block_info);
        Ok(block_info)
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>> {
        crate::inc!(PROVIDER_CALLS, &["chain_provider", "receipts_by_hash"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["chain_provider", "receipts_by_hash"],
            timer
        );
        if let Some(receipts) = self.receipts_by_hash_cache.get(&hash) {
            return Ok(receipts.clone());
        }

        let raw_receipts: TransportResult<Vec<Bytes>> =
            self.inner.raw_request("debug_getRawReceipts".into(), [hash]).await;
        let raw_receipts: Vec<Bytes> = match raw_receipts.map_err(|e| anyhow!(e)) {
            Ok(r) => r,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["chain_provider", "receipts_by_hash", "debug_getRawReceipts"]
                );
                return Err(e);
            }
        };

        let receipts = match raw_receipts
            .iter()
            .map(|r| {
                let r = &mut r.as_ref();

                // Skip the transaction type byte if it exists
                if !r.is_empty() && r[0] <= TxType::Eip4844 as u8 {
                    r.advance(1);
                }

                Ok(ReceiptWithBloom::decode(r).map_err(|e| anyhow!(e))?.receipt)
            })
            .collect::<Result<Vec<_>>>()
        {
            Ok(r) => r,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(PROVIDER_ERRORS, &["chain_provider", "receipts_by_hash", "decode"]);
                return Err(e);
            }
        };
        self.receipts_by_hash_cache.put(hash, receipts.clone());
        Ok(receipts)
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>)> {
        crate::inc!(PROVIDER_CALLS, &["chain_provider", "block_info_and_transactions_by_hash"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["chain_provider", "block_info_and_transactions_by_hash"],
            timer
        );
        if let Some(block_info_and_txs) = self.block_info_and_transactions_by_hash_cache.get(&hash)
        {
            return Ok(block_info_and_txs.clone());
        }

        let raw_block: TransportResult<Bytes> =
            self.inner.raw_request("debug_getRawBlock".into(), [hash]).await;
        let raw_block: Bytes = match raw_block.map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["chain_provider", "block_info_and_transactions_by_hash", "debug_getRawBlock"]
                );
                return Err(e);
            }
        };
        let block = match Block::decode(&mut raw_block.as_ref()).map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["chain_provider", "block_info_and_transactions_by_hash", "decode"]
                );
                return Err(e);
            }
        };

        let block_info = BlockInfo {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        };
        self.block_info_and_transactions_by_hash_cache.put(hash, (block_info, block.body.clone()));
        Ok((block_info, block.body))
    }
}

/// The [AlloyL2ChainProvider] is a concrete implementation of the [L2ChainProvider] trait,
/// providing data over Ethereum JSON-RPC using an alloy provider as the backend.
///
/// **Note**:
/// This provider fetches data using the `debug_getRawBlock` method. The RPC must support this
/// namespace.
#[derive(Debug, Clone)]
pub struct AlloyL2ChainProvider {
    /// The inner Ethereum JSON-RPC provider.
    inner: ReqwestProvider,
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// `payload_by_number` LRU cache.
    payload_by_number_cache: LruCache<u64, L2ExecutionPayloadEnvelope>,
    /// `l2_block_info_by_number` LRU cache.
    l2_block_info_by_number_cache: LruCache<u64, L2BlockInfo>,
    /// `system_config_by_l2_hash` LRU cache.
    system_config_by_number_cache: LruCache<u64, SystemConfig>,
}

impl AlloyL2ChainProvider {
    /// Creates a new [AlloyL2ChainProvider] with the given alloy provider and [RollupConfig].
    pub fn new(inner: ReqwestProvider, rollup_config: Arc<RollupConfig>) -> Self {
        Self {
            inner,
            rollup_config,
            payload_by_number_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
            l2_block_info_by_number_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
            system_config_by_number_cache: LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap()),
        }
    }

    /// Returns the chain ID.
    pub async fn chain_id(&mut self) -> Result<u64> {
        let chain_id: TransportResult<alloc::string::String> =
            self.inner.raw_request("eth_chainId".into(), ()).await;
        let chain_id = match chain_id {
            Ok(s) => alloc::string::String::from(s.trim_start_matches("0x")),
            Err(e) => return Err(anyhow!(e)),
        };
        u64::from_str_radix(&chain_id, 16).map_err(|e| anyhow!(e))
    }

    /// Returns the latest L2 block number.
    pub async fn latest_block_number(&mut self) -> Result<u64> {
        let b: TransportResult<alloc::string::String> =
            self.inner.raw_request("eth_blockNumber".into(), ()).await;
        match b {
            Ok(s) => {
                let s = alloc::string::String::from(s.trim_start_matches("0x"));
                u64::from_str_radix(&s, 16).map_err(|e| anyhow!(e))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    /// Creates a new [AlloyL2ChainProvider] from the provided [reqwest::Url].
    pub fn new_http(url: reqwest::Url, rollup_config: Arc<RollupConfig>) -> Self {
        let inner = ReqwestProvider::new_http(url);
        Self::new(inner, rollup_config)
    }
}

#[async_trait]
impl L2ChainProvider for AlloyL2ChainProvider {
    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo> {
        crate::inc!(PROVIDER_CALLS, &["l2_chain_provider", "l2_block_info_by_number"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["l2_chain_provider", "l2_block_info_by_number"],
            timer
        );
        if let Some(l2_block_info) = self.l2_block_info_by_number_cache.get(&number) {
            return Ok(*l2_block_info);
        }

        let payload = match self.payload_by_number(number).await {
            Ok(p) => p,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["l2_chain_provider", "l2_block_info_by_number", "payload_by_number"]
                );
                return Err(e);
            }
        };
        let l2_block_info = match payload.to_l2_block_ref(self.rollup_config.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["l2_chain_provider", "l2_block_info_by_number", "to_l2_block_ref"]
                );
                return Err(e);
            }
        };
        self.l2_block_info_by_number_cache.put(number, l2_block_info);
        Ok(l2_block_info)
    }

    async fn payload_by_number(&mut self, number: u64) -> Result<L2ExecutionPayloadEnvelope> {
        crate::inc!(PROVIDER_CALLS, &["l2_chain_provider", "payload_by_number"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["l2_chain_provider", "payload_by_number"],
            timer
        );
        if let Some(payload) = self.payload_by_number_cache.get(&number) {
            return Ok(payload.clone());
        }

        let raw_block: TransportResult<Bytes> =
            self.inner.raw_request("debug_getRawBlock".into(), [U64::from(number)]).await;
        let raw_block: Bytes = match raw_block.map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["l2_chain_provider", "payload_by_number", "debug_getRawBlock"]
                );
                return Err(e);
            }
        };
        let block = match OpBlock::decode(&mut raw_block.as_ref()).map_err(|e| anyhow!(e)) {
            Ok(b) => b,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(PROVIDER_ERRORS, &["l2_chain_provider", "payload_by_number", "decode"]);
                return Err(e);
            }
        };
        let payload_envelope: L2ExecutionPayloadEnvelope = block.into();

        self.payload_by_number_cache.put(number, payload_envelope.clone());
        Ok(payload_envelope)
    }

    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig> {
        crate::inc!(PROVIDER_CALLS, &["l2_chain_provider", "system_config_by_number"]);
        crate::timer!(
            START,
            PROVIDER_RESPONSE_TIME,
            &["l2_chain_provider", "system_config_by_number"],
            timer
        );
        if let Some(system_config) = self.system_config_by_number_cache.get(&number) {
            return Ok(system_config.clone());
        }

        let envelope = match self.payload_by_number(number).await {
            Ok(e) => e,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["l2_chain_provider", "system_config_by_number", "payload_by_number"]
                );
                return Err(e);
            }
        };
        let sys_config = match envelope.to_system_config(&rollup_config) {
            Ok(s) => s,
            Err(e) => {
                crate::timer!(DISCARD, timer);
                crate::inc!(
                    PROVIDER_ERRORS,
                    &["l2_chain_provider", "system_config_by_number", "to_system_config"]
                );
                return Err(e);
            }
        };
        self.system_config_by_number_cache.put(number, sys_config.clone());
        Ok(sys_config)
    }
}
