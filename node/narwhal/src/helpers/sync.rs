// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    helpers::{BFTSender, Pending, Storage, SyncReceiver},
    Gateway,
    Transport,
    MAX_BATCH_DELAY,
};
use snarkos_node_narwhal_events::{CertificateRequest, CertificateResponse, Event};
use snarkos_node_narwhal_ledger_service::LedgerService;
use snarkos_node_sync::{locators::BlockLocators, BlockSync, BlockSyncMode};
use snarkvm::{
    console::{network::Network, types::Field},
    ledger::{authority::Authority, block::Block, narwhal::BatchCertificate},
};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use std::{future::Future, net::SocketAddr, sync::Arc};
use tokio::{
    sync::{oneshot, Mutex as TMutex, OnceCell},
    task::{spawn_blocking, JoinHandle},
};

#[derive(Clone)]
pub struct Sync<N: Network> {
    /// The gateway.
    gateway: Gateway<N>,
    /// The storage.
    storage: Storage<N>,
    /// The ledger service.
    ledger: Arc<dyn LedgerService<N>>,
    /// The block sync module.
    block_sync: BlockSync<N>,
    /// The pending certificates queue.
    pending: Arc<Pending<Field<N>, BatchCertificate<N>>>,
    /// The BFT sender.
    bft_sender: Arc<OnceCell<BFTSender<N>>>,
    /// The spawned handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The sync lock.
    lock: Arc<TMutex<()>>,
}

impl<N: Network> Sync<N> {
    /// Initializes a new sync instance.
    pub fn new(gateway: Gateway<N>, storage: Storage<N>, ledger: Arc<dyn LedgerService<N>>) -> Self {
        // Initialize the block sync module.
        let block_sync = BlockSync::new(BlockSyncMode::Gateway, ledger.clone());
        // Return the sync instance.
        Self {
            gateway,
            storage,
            ledger,
            block_sync,
            pending: Default::default(),
            bft_sender: Default::default(),
            handles: Default::default(),
            lock: Default::default(),
        }
    }

    /// Starts the sync module.
    pub async fn run(&self, bft_sender: Option<BFTSender<N>>, sync_receiver: SyncReceiver<N>) -> Result<()> {
        // If a BFT sender was provided, set it.
        if let Some(bft_sender) = bft_sender {
            self.bft_sender.set(bft_sender).expect("BFT sender already set in gateway");
        }

        // Sync the storage with the ledger.
        self.sync_storage_with_ledger().await?;

        info!("Starting the sync module...");

        // Start the block sync loop.
        let self_ = self.clone();
        self.handles.lock().push(tokio::spawn(async move {
            loop {
                // Sleep briefly to avoid triggering spam detection.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                // Perform the sync routine.
                let communication = &self_.gateway;
                // let communication = &node.router;
                if let Err(error) = self_.block_sync.try_block_sync(communication).await {
                    warn!("Sync error - {error}");
                }
            }
        }));

        // Retrieve the sync receiver.
        let SyncReceiver {
            mut rx_block_sync_advance_with_sync_blocks,
            mut rx_block_sync_remove_peer,
            mut rx_block_sync_update_peer_locators,
            mut rx_certificate_request,
            mut rx_certificate_response,
        } = sync_receiver;

        // Process the block sync request to advance with sync blocks.
        let self_ = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, blocks, callback)) = rx_block_sync_advance_with_sync_blocks.recv().await {
                let block_sync_ = self_.block_sync.clone();
                // Advance with the sync blocks.
                let result = spawn_blocking(move || block_sync_.advance_with_sync_blocks(peer_ip, blocks)).await;
                // Convert the result to an `anyhow::Result`.
                let result = match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(anyhow!("[BlockResponse] {error}")),
                };
                // Send the result to the callback.
                callback.send(result).ok();
            }
        });

        // Process the block sync request to remove the peer.
        let self_ = self.clone();
        self.spawn(async move {
            while let Some(peer_ip) = rx_block_sync_remove_peer.recv().await {
                self_.block_sync.remove_peer(&peer_ip);
            }
        });

        // Process the block sync request to update peer locators.
        let self_ = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, locators, callback)) = rx_block_sync_update_peer_locators.recv().await {
                let self_clone = self_.clone();
                tokio::spawn(async move {
                    // Update the peer locators.
                    let result = self_clone.block_sync.update_peer_locators(peer_ip, locators);
                    // Send the result to the callback.
                    callback.send(result).ok();
                });
            }
        });

        // Process the certificate request.
        let self_ = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, certificate_request)) = rx_certificate_request.recv().await {
                self_.send_certificate_response(peer_ip, certificate_request);
            }
        });

        // Process the certificate response.
        let self_ = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, certificate_response)) = rx_certificate_response.recv().await {
                self_.finish_certificate_request(peer_ip, certificate_response)
            }
        });

        Ok(())
    }
}

// Methods to manage storage.
impl<N: Network> Sync<N> {
    /// Syncs the storage with the ledger.
    pub async fn sync_storage_with_ledger(&self) -> Result<()> {
        // Retrieve the latest block in the ledger.
        let latest_block = self.ledger.latest_block();
        // Sync the storage with the block.
        self.sync_storage_with_block(latest_block).await
    }

    /// Syncs the storage with the block.
    pub async fn sync_storage_with_block(&self, block: Block<N>) -> Result<()> {
        // Retrieve the block height.
        let block_height = block.height();
        // Determine the earliest height, conservatively set to the block height minus the max GC rounds.
        // By virtue of the BFT protocol, we can guarantee that all GC range blocks will be loaded.
        let gc_height = block_height.saturating_sub(u32::try_from(self.storage.max_gc_rounds())?);
        // Retrieve the blocks.
        let blocks = self.ledger.get_blocks(gc_height..block_height.saturating_add(1))?;

        // Sync the height with the block.
        self.storage.sync_height_with_block(&block);
        // Sync the round with the block.
        self.storage.sync_round_with_block(block.round());

        // Acquire the sync lock.
        let _lock = self.lock.lock().await;
        // Iterate over the blocks.
        for block in blocks {
            // If the block authority is a subdag, then sync the batch certificates with the block.
            if let Authority::Quorum(subdag) = block.authority() {
                // Iterate over the certificates.
                for certificate in subdag.values().flatten() {
                    // Sync the batch certificate with the block.
                    self.storage.sync_certificate_with_block(&block, certificate);
                    // If a BFT sender was provided, send the certificate to the BFT.
                    if let Some(bft_sender) = self.bft_sender.get() {
                        // Await the callback to continue.
                        if let Err(e) = bft_sender.send_primary_certificate_to_bft(certificate.clone()).await {
                            warn!("Failed to update the BFT DAG from sync: {e}");
                            return Err(e);
                        };
                    }
                }
            }
        }
        Ok(())
    }
}

// Methods to assist with block synchronization.
impl<N: Network> Sync<N> {
    /// Returns `true` if the node is synced.
    pub fn is_synced(&self) -> bool {
        self.block_sync.is_block_synced()
    }

    /// Returns `true` if the node is in gateway mode.
    pub const fn is_gateway_mode(&self) -> bool {
        self.block_sync.mode().is_gateway()
    }

    /// Returns the current block locators of the node.
    pub fn get_block_locators(&self) -> Result<BlockLocators<N>> {
        self.block_sync.get_block_locators()
    }
}

// Methods to assist with fetching batch certificates from peers.
impl<N: Network> Sync<N> {
    /// Sends a certificate request to the specified peer.
    pub async fn send_certificate_request(
        &self,
        peer_ip: SocketAddr,
        certificate_id: Field<N>,
    ) -> Result<BatchCertificate<N>> {
        // Initialize a oneshot channel.
        let (callback_sender, callback_receiver) = oneshot::channel();
        // Insert the certificate ID into the pending queue.
        if self.pending.insert(certificate_id, peer_ip, Some(callback_sender)) {
            // Send the certificate request to the peer.
            if self.gateway.send(peer_ip, Event::CertificateRequest(certificate_id.into())).await.is_none() {
                bail!("Unable to fetch batch certificate - failed to send request")
            }
        }
        // Wait for the certificate to be fetched.
        match tokio::time::timeout(core::time::Duration::from_millis(MAX_BATCH_DELAY), callback_receiver).await {
            // If the certificate was fetched, return it.
            Ok(result) => Ok(result?),
            // If the certificate was not fetched, return an error.
            Err(e) => bail!("Unable to fetch batch certificate - (timeout) {e}"),
        }
    }

    /// Handles the incoming certificate request.
    fn send_certificate_response(&self, peer_ip: SocketAddr, request: CertificateRequest<N>) {
        // Attempt to retrieve the certificate.
        if let Some(certificate) = self.storage.get_certificate(request.certificate_id) {
            // Send the certificate response to the peer.
            let self_ = self.clone();
            tokio::spawn(async move {
                let _ = self_.gateway.send(peer_ip, Event::CertificateResponse(certificate.into())).await;
            });
        }
    }

    /// Handles the incoming certificate response.
    /// This method ensures the certificate response is well-formed and matches the certificate ID.
    fn finish_certificate_request(&self, peer_ip: SocketAddr, response: CertificateResponse<N>) {
        let certificate = response.certificate;
        // Check if the peer IP exists in the pending queue for the given certificate ID.
        let exists = self.pending.get(certificate.certificate_id()).unwrap_or_default().contains(&peer_ip);
        // If the peer IP exists, finish the pending request.
        if exists {
            // TODO: Validate the certificate.
            // Remove the certificate ID from the pending queue.
            self.pending.remove(certificate.certificate_id(), Some(certificate));
        }
    }
}

impl<N: Network> Sync<N> {
    /// Spawns a task with the given future; it should only be used for long-running tasks.
    fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
        self.handles.lock().push(tokio::spawn(future));
    }

    /// Shuts down the primary.
    pub async fn shut_down(&self) {
        trace!("Shutting down the sync module...");
        // Acquire the sync lock.
        let _lock = self.lock.lock().await;
        // Abort the tasks.
        self.handles.lock().iter().for_each(|handle| handle.abort());
    }
}