use super::utils::StreamableTargetInformation;
use crate::account_loader::load_cnac_files;
use crate::backend::memory::MemoryBackend;
use crate::backend::utils::ObjectTransferStatus;
use crate::backend::BackendConnection;
use crate::client_account::{ClientNetworkAccount, MutualPeer};
use crate::directory_store::DirectoryStore;
use crate::misc::{AccountError, CNACMetadata};
use crate::prelude::CNAC_SERIALIZED_EXTENSION;
use async_trait::async_trait;
use citadel_crypt::stacked_ratchet::Ratchet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::StreamExt;

/// For handling I/O with the local filesystem
pub struct FilesystemBackend<R: Ratchet, Fcm: Ratchet> {
    memory_backend: MemoryBackend<R, Fcm>,
    directory_store: Option<DirectoryStore>,
    home_dir: String,
}

#[async_trait]
impl<R: Ratchet, Fcm: Ratchet> BackendConnection<R, Fcm> for FilesystemBackend<R, Fcm> {
    async fn connect(&mut self) -> Result<(), AccountError> {
        let directory_store = crate::directory_store::setup_directories(self.home_dir.clone())?;
        let map = load_cnac_files(&directory_store)?;
        // ensure the in-memory database has the clients loaded
        *self.memory_backend.clients.get_mut() = map;
        self.directory_store = Some(directory_store);

        Ok(())
    }

    async fn is_connected(&self) -> Result<bool, AccountError> {
        Ok(true)
    }

    #[allow(unused_results)]
    async fn save_cnac(&self, cnac: &ClientNetworkAccount<R, Fcm>) -> Result<(), AccountError> {
        // save to filesystem, then, synchronize to memory
        let bytes = cnac.generate_proper_bytes()?;
        let cid = cnac.get_cid();
        let path = self.generate_cnac_local_save_path(cid, cnac.is_personal());
        // TODO: The below line of code fails
        std::fs::write(path, bytes).map_err(|err| AccountError::Generic(err.to_string()))?;
        self.memory_backend.save_cnac(cnac).await
    }

    async fn get_cnac_by_cid(
        &self,
        cid: u64,
    ) -> Result<Option<ClientNetworkAccount<R, Fcm>>, AccountError> {
        self.memory_backend.get_cnac_by_cid(cid).await
    }

    async fn cid_is_registered(&self, cid: u64) -> Result<bool, AccountError> {
        self.memory_backend.cid_is_registered(cid).await
    }

    async fn delete_cnac_by_cid(&self, cid: u64) -> Result<(), AccountError> {
        let is_personal = self
            .memory_backend
            .clients
            .read()
            .get(&cid)
            .ok_or(AccountError::ClientNonExists(cid))?
            .is_personal();
        self.memory_backend.delete_cnac_by_cid(cid).await?;
        let path = self.generate_cnac_local_save_path(cid, is_personal);
        std::fs::remove_file(path).map_err(|err| AccountError::Generic(err.to_string()))
    }

    async fn purge(&self) -> Result<usize, AccountError> {
        let paths = {
            let mut write = self.memory_backend.clients.write();
            write
                .drain()
                .map(|(cid, cnac)| self.generate_cnac_local_save_path(cid, cnac.is_personal()))
                .collect::<Vec<PathBuf>>()
        };

        let count = paths.len();

        for path in paths {
            tokio::fs::remove_file(path)
                .await
                .map_err(|err| AccountError::Generic(err.to_string()))?;
        }

        // delete the home directory
        let home_dir = self.directory_store.as_ref().unwrap().hyxe_home.as_str();
        tokio::fs::remove_dir_all(home_dir)
            .await
            .map_err(|err| AccountError::Generic(err.to_string()))?;

        Ok(count)
    }

    async fn get_registered_impersonal_cids(
        &self,
        limit: Option<i32>,
    ) -> Result<Option<Vec<u64>>, AccountError> {
        self.memory_backend
            .get_registered_impersonal_cids(limit)
            .await
    }

    async fn get_username_by_cid(&self, cid: u64) -> Result<Option<String>, AccountError> {
        self.memory_backend.get_username_by_cid(cid).await
    }

    async fn register_p2p_as_server(&self, cid0: u64, cid1: u64) -> Result<(), AccountError> {
        self.memory_backend
            .register_p2p_as_server(cid0, cid1)
            .await?;
        let (cnac0, cnac1) = {
            let read = self.memory_backend.clients.read();
            let cnac0 = read
                .get(&cid0)
                .cloned()
                .ok_or(AccountError::ClientNonExists(cid0))?;
            let cnac1 = read
                .get(&cid1)
                .cloned()
                .ok_or(AccountError::ClientNonExists(cid1))?;
            (cnac0, cnac1)
        };

        self.save_cnac(&cnac0).await?;
        self.save_cnac(&cnac1).await
    }

    async fn register_p2p_as_client(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        peer_username: String,
    ) -> Result<(), AccountError> {
        self.memory_backend
            .register_p2p_as_client(implicated_cid, peer_cid, peer_username)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await
    }

    async fn deregister_p2p_as_server(&self, cid0: u64, cid1: u64) -> Result<(), AccountError> {
        self.memory_backend
            .deregister_p2p_as_server(cid0, cid1)
            .await?;
        let (cnac0, cnac1) = {
            let read = self.memory_backend.clients.read();
            let cnac0 = read
                .get(&cid0)
                .cloned()
                .ok_or(AccountError::ClientNonExists(cid0))?;
            let cnac1 = read
                .get(&cid1)
                .cloned()
                .ok_or(AccountError::ClientNonExists(cid1))?;
            (cnac0, cnac1)
        };

        self.save_cnac(&cnac0).await?;
        self.save_cnac(&cnac1).await
    }

    async fn deregister_p2p_as_client(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
    ) -> Result<Option<MutualPeer>, AccountError> {
        let res = self
            .memory_backend
            .deregister_p2p_as_client(implicated_cid, peer_cid)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await.map(|_| res)
    }

    async fn get_hyperlan_peer_list(
        &self,
        implicated_cid: u64,
    ) -> Result<Option<Vec<u64>>, AccountError> {
        self.memory_backend
            .get_hyperlan_peer_list(implicated_cid)
            .await
    }

    async fn get_client_metadata(
        &self,
        implicated_cid: u64,
    ) -> Result<Option<CNACMetadata>, AccountError> {
        self.memory_backend
            .get_client_metadata(implicated_cid)
            .await
    }

    async fn get_clients_metadata(
        &self,
        limit: Option<i32>,
    ) -> Result<Vec<CNACMetadata>, AccountError> {
        self.memory_backend.get_clients_metadata(limit).await
    }

    async fn get_hyperlan_peer_by_cid(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
    ) -> Result<Option<MutualPeer>, AccountError> {
        self.memory_backend
            .get_hyperlan_peer_by_cid(implicated_cid, peer_cid)
            .await
    }

    async fn hyperlan_peer_exists(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
    ) -> Result<bool, AccountError> {
        self.memory_backend
            .hyperlan_peer_exists(implicated_cid, peer_cid)
            .await
    }

    async fn hyperlan_peers_are_mutuals(
        &self,
        implicated_cid: u64,
        peers: &[u64],
    ) -> Result<Vec<bool>, AccountError> {
        self.memory_backend
            .hyperlan_peers_are_mutuals(implicated_cid, peers)
            .await
    }

    async fn get_hyperlan_peers(
        &self,
        implicated_cid: u64,
        peers: &[u64],
    ) -> Result<Vec<MutualPeer>, AccountError> {
        self.memory_backend
            .get_hyperlan_peers(implicated_cid, peers)
            .await
    }

    async fn get_hyperlan_peer_list_as_server(
        &self,
        implicated_cid: u64,
    ) -> Result<Option<Vec<MutualPeer>>, AccountError> {
        self.memory_backend
            .get_hyperlan_peer_list_as_server(implicated_cid)
            .await
    }

    async fn synchronize_hyperlan_peer_list_as_client(
        &self,
        cnac: &ClientNetworkAccount<R, Fcm>,
        peers: Vec<MutualPeer>,
    ) -> Result<(), AccountError> {
        self.memory_backend
            .synchronize_hyperlan_peer_list_as_client(cnac, peers)
            .await?;
        self.save_cnac(cnac).await
    }

    async fn get_byte_map_value(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        key: &str,
        sub_key: &str,
    ) -> Result<Option<Vec<u8>>, AccountError> {
        self.memory_backend
            .get_byte_map_value(implicated_cid, peer_cid, key, sub_key)
            .await
    }

    async fn remove_byte_map_value(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        key: &str,
        sub_key: &str,
    ) -> Result<Option<Vec<u8>>, AccountError> {
        let res = self
            .memory_backend
            .remove_byte_map_value(implicated_cid, peer_cid, key, sub_key)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await.map(|_| res)
    }

    async fn store_byte_map_value(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        key: &str,
        sub_key: &str,
        value: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, AccountError> {
        let res = self
            .memory_backend
            .store_byte_map_value(implicated_cid, peer_cid, key, sub_key, value)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await.map(|_| res)
    }

    async fn get_byte_map_values_by_key(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        key: &str,
    ) -> Result<HashMap<String, Vec<u8>>, AccountError> {
        let res = self
            .memory_backend
            .get_byte_map_values_by_key(implicated_cid, peer_cid, key)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await.map(|_| res)
    }

    async fn remove_byte_map_values_by_key(
        &self,
        implicated_cid: u64,
        peer_cid: u64,
        key: &str,
    ) -> Result<HashMap<String, Vec<u8>>, AccountError> {
        let res = self
            .memory_backend
            .remove_byte_map_values_by_key(implicated_cid, peer_cid, key)
            .await?;
        self.save_cnac_by_cid(implicated_cid).await.map(|_| res)
    }

    async fn stream_object_to_backend(
        &self,
        source: UnboundedReceiver<Vec<u8>>,
        sink_metadata: Arc<dyn StreamableTargetInformation>,
        status_tx: UnboundedSender<ObjectTransferStatus>,
    ) -> Result<(), AccountError> {
        let directory_store = self.directory_store.as_ref().unwrap();
        let name = sink_metadata.get_target_name();
        let save_path = directory_store.hyxe_virtual_dir.as_str();
        let save_location = format!("{}{}", save_path, name);
        let save_location = PathBuf::from(save_location);
        log::info!(target: "citadel", "Will stream object to {:?}", save_location);
        let file = tokio::fs::File::create(&save_location)
            .await
            .map_err(|err| AccountError::IoError(err.to_string()))?;

        let mut writer = tokio::io::BufWriter::new(file);
        let mut reader = tokio_util::io::StreamReader::new(
            tokio_stream::wrappers::UnboundedReceiverStream::new(source).map(|r| {
                Ok(std::io::Cursor::new(r)) as Result<std::io::Cursor<Vec<u8>>, std::io::Error>
            }),
        );

        let _ = status_tx.send(ObjectTransferStatus::ReceptionBeginning(
            save_location,
            sink_metadata,
        ));

        if let Err(err) = tokio::io::copy(&mut reader, &mut writer).await {
            log::error!(target: "citadel", "Error while copying from reader to writer: {}", err);
        }

        writer
            .shutdown()
            .await
            .map_err(|err| AccountError::IoError(err.to_string()))
    }
}

impl<R: Ratchet, Fcm: Ratchet> FilesystemBackend<R, Fcm> {
    async fn save_cnac_by_cid(&self, cid: u64) -> Result<(), AccountError> {
        let cnac = self
            .memory_backend
            .clients
            .read()
            .get(&cid)
            .cloned()
            .ok_or(AccountError::ClientNonExists(cid))?;
        self.save_cnac(&cnac).await
    }

    fn generate_cnac_local_save_path(&self, cid: u64, is_personal: bool) -> PathBuf {
        let dirs = self.directory_store.as_ref().unwrap();
        if is_personal {
            PathBuf::from(format!(
                "{}{}.{}",
                dirs.hyxe_nac_dir_personal.as_str(),
                cid,
                CNAC_SERIALIZED_EXTENSION
            ))
        } else {
            PathBuf::from(format!(
                "{}{}.{}",
                dirs.hyxe_nac_dir_impersonal.as_str(),
                cid,
                CNAC_SERIALIZED_EXTENSION
            ))
        }
    }
}

impl<R: Ratchet, Fcm: Ratchet> From<String> for FilesystemBackend<R, Fcm> {
    fn from(home_dir: String) -> Self {
        Self {
            home_dir,
            memory_backend: MemoryBackend::default(),
            directory_store: None,
        }
    }
}
