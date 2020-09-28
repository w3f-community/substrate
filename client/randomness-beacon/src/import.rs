use codec::Encode;
use futures::channel::mpsc::Sender;
use log::info;
use parking_lot::Mutex;
use sc_client_api::{backend::AuxStore, BlockOf};
use sp_api::ProvideRuntimeApi;
use sp_block_builder::BlockBuilder as BlockBuilderApi;
use sp_blockchain::{well_known_cache_keys::Id as CacheKeyId, HeaderBackend, ProvideCache};
use sp_consensus::{
    BlockCheckParams, BlockImport, BlockImportParams, Error as ConsensusError, ImportResult,
};
use sp_inherents::{InherentData, InherentDataProviders};
use sp_randomness_beacon::{register_rb_inherent_data_provider, InherentType};

use sp_runtime::{
    generic::BlockId,
    traits::{Block as BlockT, Header as HeaderT},
};
use std::{collections::HashMap, sync::Arc, thread, time::Duration};

#[derive(derive_more::Display, Debug)]
pub enum Error {
    TransmitErr,
    Client(sp_blockchain::Error),
    #[display(fmt = "Checking inherents failed: {}", _0)]
    CheckInherents(String),
}

impl std::convert::From<Error> for ConsensusError {
    fn from(error: Error) -> ConsensusError {
        ConsensusError::ClientImport(error.to_string())
    }
}

use super::{Nonce, RandomBytes};

pub struct RandomnessBeaconBlockImport<B: BlockT, I, C> {
    inner: I,
    client: Arc<C>,
    random_bytes: Arc<Mutex<InherentType>>,
    random_bytes_buf: HashMap<Nonce, Option<RandomBytes>>,
    randomness_nonce_tx: Sender<Nonce>,
    check_inherents_after: <<B as BlockT>::Header as HeaderT>::Number,
    inherent_data_providers: InherentDataProviders,
}

impl<B: BlockT, I: Clone, C> Clone for RandomnessBeaconBlockImport<B, I, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            client: self.client.clone(),
            random_bytes: self.random_bytes.clone(),
            random_bytes_buf: self.random_bytes_buf.clone(),
            randomness_nonce_tx: self.randomness_nonce_tx.clone(),
            check_inherents_after: self.check_inherents_after.clone(),
            inherent_data_providers: self.inherent_data_providers.clone(),
        }
    }
}

impl<B, I, C> RandomnessBeaconBlockImport<B, I, C>
where
    B: BlockT,
    I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync,
    I::Error: Into<ConsensusError>,
    C: ProvideRuntimeApi<B> + Send + Sync + HeaderBackend<B> + AuxStore + ProvideCache<B> + BlockOf,
    C::Api: BlockBuilderApi<B, Error = sp_blockchain::Error>,
{
    pub fn new(
        inner: I,
        client: Arc<C>,
        randomness_nonce_tx: Sender<Nonce>,
        check_inherents_after: <<B as BlockT>::Header as HeaderT>::Number,
        random_bytes: Arc<Mutex<InherentType>>,
        inherent_data_providers: InherentDataProviders,
    ) -> Self {
        register_rb_inherent_data_provider(&inherent_data_providers, random_bytes.clone());

        Self {
            inner,
            client,
            random_bytes,
            random_bytes_buf: HashMap::new(),
            randomness_nonce_tx,
            check_inherents_after,
            inherent_data_providers,
        }
    }

    fn check_inherents(&self, block: B, _inherent_data: InherentData) -> Result<(), Error> {
        if *block.header().number() < self.check_inherents_after {
            return Ok(());
        }

        // skip check if the node is not an authority
        /*
        if let Err(e) = self.can_author_with.can_author_with(&block_id) {
            debug!(
                target: "aura",
                "Skipping `check_inherents` as authoring version is not compatible: {}",
                e,
            );

            return Ok(());
        }
        */

        // The following method for checking if inherent data is correct is from timestamp pallet
        /*
        let block_id = BlockId::Hash(block.header().hash());

        let inherent_res = self
            .client
            .runtime_api()
            .check_inherents(&block_id, block, inherent_data)
            .map_err(Error::Client)?;




        if !inherent_res.ok() {
            inherent_res
                .into_errors()
                .try_for_each(|(i, e)| match TIError::try_from(&i, &e) {
                    Some(TIError::ValidAtTimestamp(timestamp)) => {
                        // halt import until timestamp is valid.
                        // reject when too far ahead.
                        if timestamp > timestamp_now + MAX_TIMESTAMP_DRIFT_SECS {
                            return Err(Error::TooFarInFuture);
                        }

                        let diff = timestamp.saturating_sub(timestamp_now);
                        thread::sleep(Duration::from_secs(diff));
                        Ok(())
                    }
                    Some(TIError::Other(e)) => Err(Error::Runtime(e.into())),
                    None => Err(Error::DataProvider(
                        self.inherent_data_providers.error_to_string(&i, &e),
                    )),
                })
        }
        */

        let parent_hash = block.header().parent_hash();
        let parent_nonce = <B as BlockT>::Hash::encode(&parent_hash);
        while self
            .random_bytes
            .lock()
            .iter()
            .find(|(nonce, _)| nonce[..] == parent_nonce[..])
            .is_none()
        {
            // the appropriate random bytes are not ready, let's wait
            // TODO: add some deadline
            // TODO: add some notification instead of dummy sleep
            info!(target: "import", "random bytes are not ready, waiting");
            thread::sleep(Duration::from_millis(100));
        }

        Ok(())
    }

    // Note: this works under the assumption that in a block there is seed corresponding to a
    // hash of the parent of the current block. If we were to allow skipping randomness in some
    // blocks, then we would need to read parent_nonce from inherents in current block.
    fn clear_old_random_bytes(&mut self, parent_hash: <B as BlockT>::Hash) {
        let parent_nonce = <B as BlockT>::Hash::encode(&parent_hash);

        self.random_bytes_buf.remove(&parent_nonce);
        self.random_bytes
            .lock()
            .retain(|(nonce, _)| nonce[..] != parent_nonce[..]);
    }

    // TODO: Nonce should be a hash so that Randomness-Beacon Pallet may choose the right one, but we
    // cannot make InherentType generic over BlockT. Figureout how to do it optimally. Current
    // approximation uses Vec<u8>.
    // Returns None is hash was already processed.
    fn hash_to_nonce(&mut self, hash: <B as BlockT>::Hash) -> Option<Nonce> {
        // Check if hash was already processed
        // TODO: is this check enough?
        match self.client.status(BlockId::Hash(hash)) {
            Ok(sp_blockchain::BlockStatus::InChain) => return None,
            _ => {}
        }
        let nonce = <B as BlockT>::Hash::encode(&hash);
        match self.random_bytes_buf.get(&nonce) {
            Some(_) => return None,
            None => return Some(nonce),
        }
    }
}

impl<B, I, C> BlockImport<B> for RandomnessBeaconBlockImport<B, I, C>
where
    B: BlockT,
    I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync,
    I::Error: Into<ConsensusError>,
    C: ProvideRuntimeApi<B> + Send + Sync + HeaderBackend<B> + AuxStore + ProvideCache<B> + BlockOf,
    C::Api: BlockBuilderApi<B, Error = sp_blockchain::Error>,
{
    type Error = ConsensusError;
    type Transaction = sp_api::TransactionFor<C, B>;

    fn check_block(&mut self, block: BlockCheckParams<B>) -> Result<ImportResult, Self::Error> {
        self.inner.check_block(block).map_err(Into::into)
    }

    fn import_block(
        &mut self,
        mut block: BlockImportParams<B, Self::Transaction>,
        new_cache: HashMap<CacheKeyId, Vec<u8>>,
    ) -> Result<ImportResult, Self::Error> {
        let parent_hash = *block.header.parent_hash();
        self.clear_old_random_bytes(parent_hash);

        if let Some(inner_body) = block.body.take() {
            let check_block = B::new(block.header.clone(), inner_body);

            let inherent_data = self
                .inherent_data_providers
                .create_inherent_data()
                .map_err(|e| e.into_string())?;
            self.check_inherents(check_block.clone(), inherent_data)?;

            block.body = Some(check_block.deconstruct().1);
        }

        if let Some(nonce) = self.hash_to_nonce(block.post_hash()) {
            if let Err(err) = self.randomness_nonce_tx.try_send(nonce.clone()) {
                info!(target: "import", "error when try_send topic through notifier {}", err);
                return Err(Error::TransmitErr.into());
            }
            self.random_bytes_buf.insert(nonce, None);
        }

        self.inner
            .import_block(block, new_cache)
            .map_err(Into::into)
    }
}