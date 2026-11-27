//! Encryption and explicit platform-backed storage for credential vault keys.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
#[cfg(not(test))]
use std::sync::Arc;
use zeroize::Zeroize;

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const VAULT_FORMAT: &str = "krypper-credential-vault";
const VAULT_VERSION: u8 = 1;
#[cfg(not(test))]
const KEYRING_SERVICE: &str = "com.krypper.credential-vault";

#[derive(Debug)]
pub(super) enum VaultError {
    Unavailable,
    MissingKey,
    InvalidKey,
    InvalidEnvelope,
    UnsupportedVersion,
    AuthenticationFailed,
    RandomSourceUnavailable,
}

/// Owns secret bytes and clears the allocation when dropped.
pub(super) struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialsEnvelope {
    format: String,
    version: u8,
    key_reference: String,
    nonce: String,
    ciphertext: String,
}

impl CredentialsEnvelope {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, VaultError> {
        let envelope: Self =
            serde_json::from_slice(bytes).map_err(|_| VaultError::InvalidEnvelope)?;
        if envelope.format != VAULT_FORMAT {
            return Err(VaultError::InvalidEnvelope);
        }
        if envelope.version != VAULT_VERSION {
            return Err(VaultError::UnsupportedVersion);
        }
        if !uuid::Uuid::parse_str(&envelope.key_reference)
            .is_ok_and(|reference| reference.to_string() == envelope.key_reference)
        {
            return Err(VaultError::InvalidEnvelope);
        }
        Ok(envelope)
    }

    pub(super) fn key_reference(&self) -> &str {
        &self.key_reference
    }

    pub(super) fn nonce_and_ciphertext(&self) -> Result<(Vec<u8>, Vec<u8>), VaultError> {
        let nonce = BASE64
            .decode(&self.nonce)
            .map_err(|_| VaultError::InvalidEnvelope)?;
        let ciphertext = BASE64
            .decode(&self.ciphertext)
            .map_err(|_| VaultError::InvalidEnvelope)?;
        if nonce.len() != NONCE_BYTES || ciphertext.len() < aead::AES_256_GCM.tag_len() {
            return Err(VaultError::InvalidEnvelope);
        }
        Ok((nonce, ciphertext))
    }
}

pub(super) fn create_key() -> Result<SecretBytes, VaultError> {
    let mut key = SecretBytes::new(vec![0_u8; KEY_BYTES]);
    SystemRandom::new()
        .fill(&mut key.0)
        .map_err(|_| VaultError::RandomSourceUnavailable)?;
    Ok(key)
}

pub(super) fn create_nonce() -> Result<SecretBytes, VaultError> {
    let mut nonce = SecretBytes::new(vec![0_u8; NONCE_BYTES]);
    SystemRandom::new()
        .fill(&mut nonce.0)
        .map_err(|_| VaultError::RandomSourceUnavailable)?;
    Ok(nonce)
}

pub(super) fn encrypt(
    key: &[u8],
    key_reference: &str,
    nonce: &[u8],
    mut plaintext: SecretBytes,
) -> Result<CredentialsEnvelope, VaultError> {
    if key.len() != KEY_BYTES || nonce.len() != NONCE_BYTES {
        return Err(VaultError::InvalidKey);
    }
    let unbound = UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| VaultError::InvalidKey)?;
    let key = LessSafeKey::new(unbound);
    let nonce_encoded = BASE64.encode(nonce);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| VaultError::InvalidEnvelope)?;
    let associated_data = associated_data(key_reference);
    key.seal_in_place_append_tag(
        nonce,
        Aad::from(associated_data.as_slice()),
        &mut plaintext.0,
    )
    .map_err(|_| VaultError::InvalidEnvelope)?;
    Ok(CredentialsEnvelope {
        format: VAULT_FORMAT.to_owned(),
        version: VAULT_VERSION,
        key_reference: key_reference.to_owned(),
        nonce: nonce_encoded,
        ciphertext: BASE64.encode(&plaintext.0),
    })
}

pub(super) fn decrypt(
    envelope: &CredentialsEnvelope,
    key: &[u8],
) -> Result<SecretBytes, VaultError> {
    if key.len() != KEY_BYTES {
        return Err(VaultError::InvalidKey);
    }
    let (nonce_bytes, ciphertext) = envelope.nonce_and_ciphertext()?;
    let nonce =
        Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| VaultError::InvalidEnvelope)?;
    let unbound = UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| VaultError::InvalidKey)?;
    let key = LessSafeKey::new(unbound);
    let associated_data = associated_data(envelope.key_reference());
    let mut plaintext = ciphertext;
    let plaintext_len = {
        let opened = key
            .open_in_place(nonce, Aad::from(associated_data.as_slice()), &mut plaintext)
            .map_err(|_| VaultError::AuthenticationFailed)?;
        opened.len()
    };
    plaintext.truncate(plaintext_len);
    Ok(SecretBytes::new(plaintext))
}

pub(super) fn serialize_envelope(envelope: &CredentialsEnvelope) -> Result<Vec<u8>, VaultError> {
    serde_json::to_vec(envelope).map_err(|_| VaultError::InvalidEnvelope)
}

fn associated_data(key_reference: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(VAULT_FORMAT.len() + key_reference.len() + 8);
    data.extend_from_slice(VAULT_FORMAT.as_bytes());
    data.extend_from_slice(b":v1:");
    data.extend_from_slice(key_reference.as_bytes());
    data
}

pub(super) trait MasterKeyStore: Send + Sync {
    fn store(&self, key_reference: &str, key: &[u8]) -> Result<(), VaultError>;
    fn retrieve(&self, key_reference: &str) -> Result<SecretBytes, VaultError>;
    fn delete(&self, key_reference: &str) -> Result<(), VaultError>;
}

#[cfg(not(test))]
pub(super) struct NativeMasterKeyStore;

#[cfg(not(test))]
impl MasterKeyStore for NativeMasterKeyStore {
    fn store(&self, key_reference: &str, key: &[u8]) -> Result<(), VaultError> {
        let entry = platform_entry(key_reference)?;
        entry.set_secret(key).map_err(|_| VaultError::Unavailable)
    }

    fn retrieve(&self, key_reference: &str) -> Result<SecretBytes, VaultError> {
        let entry = platform_entry(key_reference)?;
        match entry.get_secret() {
            Ok(key) => Ok(SecretBytes::new(key)),
            Err(keyring_core::Error::NoEntry) => Err(VaultError::MissingKey),
            Err(_) => Err(VaultError::Unavailable),
        }
    }

    fn delete(&self, key_reference: &str) -> Result<(), VaultError> {
        let entry = platform_entry(key_reference)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(_) => Err(VaultError::Unavailable),
        }
    }
}

#[cfg(target_os = "linux")]
#[cfg(all(not(test), target_os = "linux"))]
fn platform_entry(key_reference: &str) -> Result<keyring_core::Entry, VaultError> {
    let store: Arc<keyring_core::CredentialStore> =
        zbus_secret_service_keyring_store::Store::new().map_err(|_| VaultError::Unavailable)?;
    keyring_core::Entry::new_in_store(&store, KEYRING_SERVICE, key_reference)
        .map_err(|_| VaultError::Unavailable)
}

#[cfg(target_os = "macos")]
#[cfg(all(not(test), target_os = "macos"))]
fn platform_entry(key_reference: &str) -> Result<keyring_core::Entry, VaultError> {
    let store: Arc<keyring_core::CredentialStore> =
        apple_native_keyring_store::keychain::Store::new().map_err(|_| VaultError::Unavailable)?;
    keyring_core::Entry::new_in_store(&store, KEYRING_SERVICE, key_reference)
        .map_err(|_| VaultError::Unavailable)
}

#[cfg(target_os = "windows")]
#[cfg(all(not(test), target_os = "windows"))]
fn platform_entry(key_reference: &str) -> Result<keyring_core::Entry, VaultError> {
    let store: Arc<keyring_core::CredentialStore> =
        windows_native_keyring_store::Store::new().map_err(|_| VaultError::Unavailable)?;
    let modifiers = std::collections::HashMap::from([("persistence", "Local")]);
    keyring_core::Entry::new_in_store_with_modifiers(
        &store,
        KEYRING_SERVICE,
        key_reference,
        &modifiers,
    )
    .map_err(|_| VaultError::Unavailable)
}

#[cfg(all(
    not(test),
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
fn platform_entry(_key_reference: &str) -> Result<keyring_core::Entry, VaultError> {
    Err(VaultError::Unavailable)
}

#[cfg(not(test))]
pub(super) fn default_master_key_store() -> Box<dyn MasterKeyStore> {
    Box::new(NativeMasterKeyStore)
}

#[cfg(test)]
pub(super) fn default_master_key_store() -> Box<dyn MasterKeyStore> {
    Box::new(TestMasterKeyStore)
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestMasterKeyStore;

#[cfg(test)]
static TEST_MASTER_KEYS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
impl MasterKeyStore for TestMasterKeyStore {
    fn store(&self, key_reference: &str, key: &[u8]) -> Result<(), VaultError> {
        let keys = TEST_MASTER_KEYS.get_or_init(Default::default);
        keys.lock()
            .map_err(|_| VaultError::Unavailable)?
            .insert(key_reference.to_owned(), key.to_vec());
        Ok(())
    }

    fn retrieve(&self, key_reference: &str) -> Result<SecretBytes, VaultError> {
        let keys = TEST_MASTER_KEYS.get_or_init(Default::default);
        keys.lock()
            .map_err(|_| VaultError::Unavailable)?
            .get(key_reference)
            .cloned()
            .map(SecretBytes::new)
            .ok_or(VaultError::MissingKey)
    }

    fn delete(&self, key_reference: &str) -> Result<(), VaultError> {
        let keys = TEST_MASTER_KEYS.get_or_init(Default::default);
        if let Some(mut key) = keys
            .lock()
            .map_err(|_| VaultError::Unavailable)?
            .remove(key_reference)
        {
            key.zeroize();
        }
        Ok(())
    }
}
