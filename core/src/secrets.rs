//! Secrets and the machine key (S9, B3, N2, N8, A6).
//!
//! - [`Secret`]: a value that is never printed (its `Debug` is masked) and is wiped on drop.
//! - [`SecretStore`]: where the machine credential lives. Live: the data protection Keychain,
//!   whose access is bound to the code signature (team and access group), not to a path (N2).
//!   Demo and tests: memory only — **a demo service never touches the Keychain** (N8).
//! - [`MachineKeyStore`]: the machine's P-256 key, generated *before* the association and used
//!   to prove possession on every call to Nestor (B3). Live: generated in the Secure Enclave, so
//!   its private part cannot be exported.
//!
//! Both live implementations need a signed build with a `keychain-access-groups` entitlement
//! (§6.B.5). An unsigned build gets an error, which the service reports. **The software key
//! fallback of A6 is coded ([`SoftwareKeyStore`]) but off**: [`KeyStoreWithFallback`] uses it only
//! if `allow_software_fallback` is set, which the service binary never does until Brice accepts
//! that weaker protection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A secret value. Its `Debug` output is masked, and its bytes are overwritten when dropped.
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Secret(self.0.clone())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([masked])")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best effort: overwrite before the allocation is released.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for byte in bytes.iter_mut() {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// Names of the secrets the service keeps.
pub const MACHINE_CREDENTIAL: &str = "machine-credential";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBackend {
    /// In memory, gone when the service stops (demo, tests).
    Memory,
    /// The data protection Keychain, access bound to the code signature.
    Keychain,
}

pub trait SecretStore: Send + Sync {
    fn backend(&self) -> SecretBackend;
    fn put(&self, name: &str, secret: &Secret) -> Result<(), String>;
    fn get(&self, name: &str) -> Result<Option<Secret>, String>;
    fn delete(&self, name: &str) -> Result<(), String>;
}

#[derive(Default)]
pub struct MemorySecretStore {
    items: Mutex<BTreeMap<String, Secret>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        MemorySecretStore::default()
    }

    pub fn names(&self) -> Vec<String> {
        self.items.lock().unwrap().keys().cloned().collect()
    }
}

impl SecretStore for MemorySecretStore {
    fn backend(&self) -> SecretBackend {
        SecretBackend::Memory
    }
    fn put(&self, name: &str, secret: &Secret) -> Result<(), String> {
        self.items.lock().unwrap().insert(name.into(), secret.clone());
        Ok(())
    }
    fn get(&self, name: &str) -> Result<Option<Secret>, String> {
        Ok(self.items.lock().unwrap().get(name).cloned())
    }
    fn delete(&self, name: &str) -> Result<(), String> {
        self.items.lock().unwrap().remove(name);
        Ok(())
    }
}

/// The data protection Keychain (macOS). `service` names the items; `access_group` is the
/// Keychain access group of the signed app and service (`TEAMID.fr.kocoa.nestor.runner-manager`).
#[cfg(target_os = "macos")]
pub struct KeychainSecretStore {
    pub service: String,
    pub access_group: Option<String>,
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn backend(&self) -> SecretBackend {
        SecretBackend::Keychain
    }
    fn put(&self, name: &str, secret: &Secret) -> Result<(), String> {
        crate::sys::keychain_put(&self.service, name, secret.expose().as_bytes(), self.access_group.as_deref())
    }
    fn get(&self, name: &str) -> Result<Option<Secret>, String> {
        crate::sys::keychain_get(&self.service, name, self.access_group.as_deref())
            .map(|bytes| bytes.map(|b| Secret::new(String::from_utf8_lossy(&b).into_owned())))
    }
    fn delete(&self, name: &str) -> Result<(), String> {
        crate::sys::keychain_delete(&self.service, name, self.access_group.as_deref())
    }
}

// MARK: Machine key

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyProtection {
    /// Generated in the Secure Enclave: the private key cannot leave it.
    SecureEnclave,
    /// The A6 fallback: a software key kept in the Keychain. Weaker; must be accepted first.
    Software,
    /// In memory only (demo, tests).
    Memory,
}

pub trait MachineKey: Send + Sync {
    /// X9.63 uncompressed P-256 public key.
    fn public_key(&self) -> Result<Vec<u8>, String>;
    /// ECDSA P-256 / SHA-256 signature of `message`.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String>;
}

pub trait MachineKeyStore: Send + Sync {
    fn protection(&self) -> KeyProtection;
    /// Generates a new key, replacing any previous one.
    fn create(&self) -> Result<Arc<dyn MachineKey>, String>;
    fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String>;
    fn delete(&self) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
struct SecMachineKey(crate::sys::SecKey);

#[cfg(target_os = "macos")]
impl MachineKey for SecMachineKey {
    fn public_key(&self) -> Result<Vec<u8>, String> {
        self.0.public_key()
    }
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        self.0.sign(message)
    }
}

/// A key in memory (Security.framework, not stored anywhere). Demo and tests.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct MemoryKeyStore {
    key: Mutex<Option<Arc<dyn MachineKey>>>,
}

#[cfg(target_os = "macos")]
impl MemoryKeyStore {
    pub fn new() -> Self {
        MemoryKeyStore::default()
    }
}

#[cfg(target_os = "macos")]
impl MachineKeyStore for MemoryKeyStore {
    fn protection(&self) -> KeyProtection {
        KeyProtection::Memory
    }
    fn create(&self) -> Result<Arc<dyn MachineKey>, String> {
        let key: Arc<dyn MachineKey> = Arc::new(SecMachineKey(crate::sys::SecKey::ephemeral()?));
        *self.key.lock().unwrap() = Some(key.clone());
        Ok(key)
    }
    fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String> {
        Ok(self.key.lock().unwrap().clone())
    }
    fn delete(&self) -> Result<(), String> {
        *self.key.lock().unwrap() = None;
        Ok(())
    }
}

/// The machine key in the Secure Enclave, found again by its tag in the data protection
/// Keychain. Needs a signed build.
#[cfg(target_os = "macos")]
pub struct SecureEnclaveKeyStore {
    pub tag: String,
    pub access_group: Option<String>,
}

#[cfg(target_os = "macos")]
impl MachineKeyStore for SecureEnclaveKeyStore {
    fn protection(&self) -> KeyProtection {
        KeyProtection::SecureEnclave
    }
    fn create(&self) -> Result<Arc<dyn MachineKey>, String> {
        crate::sys::SecKey::delete(&self.tag)
            .and_then(|_| crate::sys::SecKey::secure_enclave(&self.tag, self.access_group.as_deref()))
            .map(|key| Arc::new(SecMachineKey(key)) as Arc<dyn MachineKey>)
            .map_err(|e| {
                format!(
                    "cannot create the machine key in the Secure Enclave ({e}); a signed build is required (§6.B.5)"
                )
            })
    }
    fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String> {
        Ok(crate::sys::SecKey::find(&self.tag)?.map(|k| Arc::new(SecMachineKey(k)) as Arc<dyn MachineKey>))
    }
    fn delete(&self) -> Result<(), String> {
        crate::sys::SecKey::delete(&self.tag)
    }
}

/// The A6 fallback: a software P-256 key whose private part is kept in a [`SecretStore`]. Weaker
/// than the Secure Enclave (the key can be copied by anything that can read that store), so it is
/// only used through [`KeyStoreWithFallback`] with an explicit permission.
#[cfg(target_os = "macos")]
pub struct SoftwareKeyStore {
    pub secrets: Arc<dyn SecretStore>,
}

#[cfg(target_os = "macos")]
const SOFTWARE_KEY: &str = "machine-key-software";

#[cfg(target_os = "macos")]
impl MachineKeyStore for SoftwareKeyStore {
    fn protection(&self) -> KeyProtection {
        KeyProtection::Software
    }
    fn create(&self) -> Result<Arc<dyn MachineKey>, String> {
        let key = crate::sys::SecKey::ephemeral()?;
        let private = Secret::new(crate::sys::hex(&key.export_private_software()?));
        self.secrets.put(SOFTWARE_KEY, &private)?;
        Ok(Arc::new(SecMachineKey(key)))
    }
    fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String> {
        let Some(private) = self.secrets.get(SOFTWARE_KEY)? else { return Ok(None) };
        let bytes = crate::sys::unhex(private.expose()).ok_or("unreadable software key")?;
        Ok(Some(Arc::new(SecMachineKey(crate::sys::SecKey::import_private(&bytes)?))))
    }
    fn delete(&self) -> Result<(), String> {
        self.secrets.delete(SOFTWARE_KEY)
    }
}

/// The primary key store, and the A6 fallback only if explicitly allowed.
pub struct KeyStoreWithFallback {
    pub primary: Arc<dyn MachineKeyStore>,
    pub fallback: Option<Arc<dyn MachineKeyStore>>,
    /// Off by default. Turning it on is Brice's decision (A6).
    pub allow_software_fallback: bool,
    used_fallback: Mutex<bool>,
}

impl KeyStoreWithFallback {
    pub fn new(primary: Arc<dyn MachineKeyStore>, fallback: Option<Arc<dyn MachineKeyStore>>, allow: bool) -> Self {
        KeyStoreWithFallback { primary, fallback, allow_software_fallback: allow, used_fallback: Mutex::new(false) }
    }

    fn fallback(&self) -> Option<&Arc<dyn MachineKeyStore>> {
        self.fallback.as_ref().filter(|_| self.allow_software_fallback)
    }
}

impl MachineKeyStore for KeyStoreWithFallback {
    fn protection(&self) -> KeyProtection {
        match (*self.used_fallback.lock().unwrap(), self.fallback()) {
            (true, Some(fallback)) => fallback.protection(),
            _ => self.primary.protection(),
        }
    }
    fn create(&self) -> Result<Arc<dyn MachineKey>, String> {
        match self.primary.create() {
            Ok(key) => {
                *self.used_fallback.lock().unwrap() = false;
                Ok(key)
            }
            Err(error) => match self.fallback() {
                Some(fallback) => {
                    *self.used_fallback.lock().unwrap() = true;
                    fallback.create()
                }
                None => Err(format!("{error}. The software key fallback is disabled (A6).")),
            },
        }
    }
    fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String> {
        if let Some(key) = self.primary.load()? {
            return Ok(Some(key));
        }
        match self.fallback() {
            Some(fallback) => {
                let key = fallback.load()?;
                *self.used_fallback.lock().unwrap() = key.is_some();
                Ok(key)
            }
            None => Ok(None),
        }
    }
    fn delete(&self) -> Result<(), String> {
        let primary = self.primary.delete();
        // Erase a fallback key even if the fallback is now disallowed.
        let fallback = self.fallback.as_ref().map(|f| f.delete()).unwrap_or(Ok(()));
        primary.and(fallback)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    struct Broken;
    impl MachineKeyStore for Broken {
        fn protection(&self) -> KeyProtection {
            KeyProtection::SecureEnclave
        }
        fn create(&self) -> Result<Arc<dyn MachineKey>, String> {
            Err("no Secure Enclave access in an unsigned build".into())
        }
        fn load(&self) -> Result<Option<Arc<dyn MachineKey>>, String> {
            Ok(None)
        }
        fn delete(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("nestor_machine_0123456789");
        assert_eq!(format!("{secret:?}"), "Secret([masked])");
    }

    #[test]
    fn the_software_fallback_is_off_unless_allowed() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let fallback: Arc<dyn MachineKeyStore> = Arc::new(SoftwareKeyStore { secrets: secrets.clone() });
        let store = KeyStoreWithFallback::new(Arc::new(Broken), Some(fallback.clone()), false);
        let error = store.create().err().unwrap();
        assert!(error.contains("fallback is disabled"), "{error}");
        assert!(secrets.get("machine-key-software").unwrap().is_none(), "nothing was stored");

        let allowed = KeyStoreWithFallback::new(Arc::new(Broken), Some(fallback), true);
        let key = allowed.create().unwrap();
        assert_eq!(allowed.protection(), KeyProtection::Software);
        let reloaded = allowed.load().unwrap().unwrap();
        assert_eq!(key.public_key().unwrap(), reloaded.public_key().unwrap());
        let signature = reloaded.sign(b"m").unwrap();
        assert!(crate::sys::verify_signature(&key.public_key().unwrap(), b"m", &signature));
        allowed.delete().unwrap();
        assert!(allowed.load().unwrap().is_none());
    }
}

/// The secret and key stores of a service mode. A demo service gets memory-only stores, so it
/// can never read or write the Keychain (N8); a live service gets the Keychain and the Secure
/// Enclave, without the software fallback.
#[cfg(target_os = "macos")]
pub fn stores_for(
    mode: crate::model::ServiceMode,
    access_group: Option<String>,
) -> (Arc<dyn SecretStore>, Arc<dyn MachineKeyStore>) {
    match mode {
        crate::model::ServiceMode::Demo => (Arc::new(MemorySecretStore::new()), Arc::new(MemoryKeyStore::new())),
        crate::model::ServiceMode::Live => {
            let secrets: Arc<dyn SecretStore> = Arc::new(KeychainSecretStore {
                service: "fr.kocoa.nestor.runner-manager".into(),
                access_group: access_group.clone(),
            });
            let enclave: Arc<dyn MachineKeyStore> = Arc::new(SecureEnclaveKeyStore {
                tag: "fr.kocoa.nestor.runner-manager.machine-key".into(),
                access_group,
            });
            let fallback: Arc<dyn MachineKeyStore> = Arc::new(SoftwareKeyStore { secrets: secrets.clone() });
            let keys = KeyStoreWithFallback::new(enclave, Some(fallback), false);
            (secrets, Arc::new(keys))
        }
    }
}
