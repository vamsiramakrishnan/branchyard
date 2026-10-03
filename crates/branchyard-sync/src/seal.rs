//! What every object looks like in the bucket, and the keys behind it.
//!
//! **Envelope.** Each object is framed as
//!
//! ```text
//! "BYS1" | flags (0 plain, 1 sealed) | ...
//! plain:  blake3(payload) (32) | payload
//! sealed: algorithm (1) | key version (4, BE) | wrapped data key (60) | nonce (12) | ciphertext + tag
//! ```
//!
//! A sealed object has a data key of its own (32 random bytes), sealed
//! with the object's algorithm (AES-256-GCM or ChaCha20-Poly1305, through
//! `ring`) with the object's name in the associated data, so an object
//! moved to another name does not open. The data key is wrapped with
//! AES-256-GCM under the tenant's key-encryption key of that version
//! (again bound to the name). A plain object carries a BLAKE3 checksum.
//!
//! **Names.** In a sealed remote, an object's name is a keyed hash
//! (BLAKE3 in keyed mode, with a names key only the tenant holds) of its
//! kind and content hash, and a task's directory is a keyed hash of its
//! ID, so the bucket learns neither content hashes nor task names. In a
//! plain remote they are the BLAKE3 hash and the ID. Either way, a reader
//! recomputes the name from what it decrypted and refuses a mismatch.
//!
//! **Keyring.** `keyring.json` at the remote's root says whether the
//! remote is sealed, and holds each key version's secret wrapped by the
//! tenant's wrapper (a passphrase or a KMS, [`crate::kms`]) and the names
//! key sealed under the current version. Keys are derived from those
//! secrets with HKDF-SHA256. Rotation adds a version, rewraps every
//! object's data key under it (the data and the names stay), then drops
//! the versions no object uses that were superseded longer ago than the
//! grace period: see [`rotate`]. A writer re-reads the keyring before it
//! publishes a manifest, and at least once a minute before other writes,
//! so none is still sealing under a version by the time it can go.

use std::collections::BTreeMap;

use ring::{aead, hkdf};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Kind, Result};
use crate::kms::{aead_open, aead_seal, Wrapper};
use crate::store::ObjectStore;
use crate::util::{b64, hex, random_bytes, unb64};

pub const KEYRING: &str = "keyring.json";
const MAGIC: &[u8; 4] = b"BYS1";
const PLAIN: u8 = 0;
const SEALED: u8 = 1;
const WRAPPED_LEN: usize = 12 + 32 + 16;

/// The data cipher of a sealed remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Algorithm {
    Aes256Gcm,
    Chacha20Poly1305,
}

impl Algorithm {
    fn byte(self) -> u8 {
        match self {
            Algorithm::Aes256Gcm => 1,
            Algorithm::Chacha20Poly1305 => 2,
        }
    }

    fn from_byte(b: u8) -> Result<Algorithm> {
        match b {
            1 => Ok(Algorithm::Aes256Gcm),
            2 => Ok(Algorithm::Chacha20Poly1305),
            _ => Err(Error::corrupt(format!("unknown cipher {b}"))),
        }
    }

    fn ring(self) -> &'static aead::Algorithm {
        match self {
            Algorithm::Aes256Gcm => &aead::AES_256_GCM,
            Algorithm::Chacha20Poly1305 => &aead::CHACHA20_POLY1305,
        }
    }

    pub fn parse(text: &str) -> Result<Algorithm> {
        match text {
            "aes-256-gcm" => Ok(Algorithm::Aes256Gcm),
            "chacha20-poly1305" => Ok(Algorithm::Chacha20Poly1305),
            _ => Err(Error::config(format!(
                "{text:?} is not aes-256-gcm or chacha20-poly1305"
            ))),
        }
    }
}

/// One key version in the keyring.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyVersion {
    pub version: u32,
    /// What wrapped it: `passphrase` or a `kms://` URL.
    pub wrapper: String,
    pub wrapped: String,
    pub created_ms: u64,
    /// When a newer version became current. A version is retired only a
    /// grace period after this, so a writer still sealing under it (one
    /// that opened the keyring before the rotation) has re-read the
    /// keyring, and its objects are there to be seen, before the version
    /// can go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_ms: Option<u64>,
}

/// `keyring.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringFile {
    pub format: u32,
    /// `none` or `envelope`.
    pub encryption: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<Algorithm>,
    #[serde(default)]
    pub current: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyVersion>,
    /// The names key, sealed under `names_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<String>,
    #[serde(default)]
    pub names_version: u32,
}

/// What a remote is asked to be.
pub enum Encryption {
    None,
    Envelope {
        wrapper: Box<dyn Wrapper>,
        algorithm: Algorithm,
    },
}

impl Encryption {
    pub fn describe(&self) -> String {
        match self {
            Encryption::None => "none".into(),
            Encryption::Envelope { wrapper, .. } => wrapper.describe(),
        }
    }
}

#[derive(Clone)]
enum Mode {
    Plain,
    Sealed {
        algorithm: Algorithm,
        current: u32,
        keks: BTreeMap<u32, [u8; 32]>,
        names: [u8; 32],
    },
}

/// Seals, opens and names objects for one remote.
#[derive(Clone)]
pub struct Sealer {
    mode: Mode,
}

struct Len32;

impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn derive(secret: &[u8], info: &[u8]) -> Result<[u8; 32]> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"branchyard-sync").extract(secret);
    let info = [info];
    let okm = prk
        .expand(&info, Len32)
        .map_err(|_| Error::local("HKDF failed"))?;
    let mut out = [0u8; 32];
    okm.fill(&mut out)
        .map_err(|_| Error::local("HKDF failed"))?;
    Ok(out)
}

fn kek(secret: &[u8]) -> Result<[u8; 32]> {
    derive(secret, b"kek v1")
}

impl Sealer {
    pub fn plain() -> Sealer {
        Sealer { mode: Mode::Plain }
    }

    pub fn sealed(&self) -> bool {
        matches!(self.mode, Mode::Sealed { .. })
    }

    /// The current key version, for a sealed remote.
    pub fn current_version(&self) -> Option<u32> {
        match &self.mode {
            Mode::Plain => None,
            Mode::Sealed { current, .. } => Some(*current),
        }
    }

    /// Whether this sealer seals under `file`'s current version and holds
    /// every version `file` lists (a plain sealer, a plain keyring).
    pub fn matches(&self, file: &KeyringFile) -> bool {
        match &self.mode {
            Mode::Plain => file.encryption == "none",
            Mode::Sealed { current, keks, .. } => {
                *current == file.current && file.keys.iter().all(|k| keks.contains_key(&k.version))
            }
        }
    }

    /// An object's name from its kind and content hash.
    pub fn name(&self, kind: &str, content: &blake3::Hash) -> String {
        match &self.mode {
            Mode::Plain => content.to_hex().to_string(),
            Mode::Sealed { names, .. } => {
                let mut input = kind.as_bytes().to_vec();
                input.push(0);
                input.extend_from_slice(content.as_bytes());
                blake3::keyed_hash(names, &input).to_hex().to_string()
            }
        }
    }

    /// A task's directory name.
    pub fn task_dir(&self, task: &str) -> String {
        match &self.mode {
            Mode::Plain => task.to_owned(),
            Mode::Sealed { names, .. } => {
                let mut input = b"task\0".to_vec();
                input.extend_from_slice(task.as_bytes());
                hex(&blake3::keyed_hash(names, &input).as_bytes()[..20])
            }
        }
    }

    /// A name derived from text (a lease's attempt, a device).
    pub fn keyed(&self, label: &str, text: &str) -> String {
        match &self.mode {
            Mode::Plain => text.to_owned(),
            Mode::Sealed { names, .. } => {
                let mut input = label.as_bytes().to_vec();
                input.push(0);
                input.extend_from_slice(text.as_bytes());
                hex(&blake3::keyed_hash(names, &input).as_bytes()[..20])
            }
        }
    }

    /// Frame (and in a sealed remote, encrypt) `plaintext` for `key`.
    pub fn seal(&self, key: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut out = MAGIC.to_vec();
        match &self.mode {
            Mode::Plain => {
                out.push(PLAIN);
                out.extend_from_slice(blake3::hash(plaintext).as_bytes());
                out.extend_from_slice(plaintext);
            }
            Mode::Sealed {
                algorithm,
                current,
                keks,
                ..
            } => {
                let kek = keks
                    .get(current)
                    .ok_or_else(|| Error::local("the current key is not loaded"))?;
                let dk = random_bytes(32)?;
                let wrapped = aead_seal(kek, &dk_aad(key), &dk)?;
                out.push(SEALED);
                out.push(algorithm.byte());
                out.extend_from_slice(&current.to_be_bytes());
                out.extend_from_slice(&wrapped);
                let nonce = random_bytes(12)?;
                let sealing = aead::LessSafeKey::new(
                    aead::UnboundKey::new(algorithm.ring(), &dk)
                        .map_err(|_| Error::local("bad data key"))?,
                );
                let mut buf = plaintext.to_vec();
                sealing
                    .seal_in_place_append_tag(
                        aead::Nonce::try_assume_unique_for_key(&nonce)
                            .map_err(|_| Error::local("bad nonce"))?,
                        aead::Aad::from(data_aad(*algorithm, key)),
                        &mut buf,
                    )
                    .map_err(|_| Error::local("sealing failed"))?;
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&buf);
            }
        }
        Ok(out)
    }

    /// Unframe (and decrypt) what was read at `key`, checking its
    /// checksum or tag. Never returns data that does not verify.
    pub fn open(&self, key: &str, framed: &[u8]) -> Result<Vec<u8>> {
        if framed.len() < 5 || &framed[..4] != MAGIC {
            return Err(Error::corrupt(format!(
                "{key}: not a Branchyard sync object"
            )));
        }
        match (framed[4], &self.mode) {
            (PLAIN, Mode::Plain) => {
                if framed.len() < 37 {
                    return Err(Error::corrupt(format!("{key}: truncated")));
                }
                let payload = &framed[37..];
                match blake3::hash(payload).as_bytes()[..] == framed[5..37] {
                    true => Ok(payload.to_vec()),
                    false => Err(Error::corrupt(format!("{key}: checksum mismatch"))),
                }
            }
            (SEALED, Mode::Sealed { keks, .. }) => {
                let header = 5 + 1 + 4 + WRAPPED_LEN + 12;
                if framed.len() < header + 16 {
                    return Err(Error::corrupt(format!("{key}: truncated")));
                }
                let algorithm = Algorithm::from_byte(framed[5])?;
                let version = u32::from_be_bytes(framed[6..10].try_into().unwrap_or_default());
                let kek = keks.get(&version).ok_or_else(|| {
                    Error::refused(format!(
                        "{key}: sealed with key version {version}, which this keyring lacks"
                    ))
                })?;
                let dk =
                    aead_open(kek, &dk_aad(key), &framed[10..10 + WRAPPED_LEN]).map_err(|_| {
                        Error::corrupt(format!(
                            "{key}: its data key does not open (tampered or moved)"
                        ))
                    })?;
                let nonce = &framed[10 + WRAPPED_LEN..header];
                let opening = aead::LessSafeKey::new(
                    aead::UnboundKey::new(algorithm.ring(), &dk)
                        .map_err(|_| Error::corrupt("bad data key"))?,
                );
                let mut buf = framed[header..].to_vec();
                let plain = opening
                    .open_in_place(
                        aead::Nonce::try_assume_unique_for_key(nonce)
                            .map_err(|_| Error::corrupt("bad nonce"))?,
                        aead::Aad::from(data_aad(algorithm, key)),
                        &mut buf,
                    )
                    .map_err(|_| Error::corrupt(format!("{key}: does not decrypt (tampered)")))?;
                Ok(plain.to_vec())
            }
            (PLAIN, Mode::Sealed { .. }) => Err(Error::corrupt(format!(
                "{key}: a plain object in a sealed remote"
            ))),
            (SEALED, Mode::Plain) => Err(Error::config(format!(
                "{key} is sealed; set [sync] encrypt to read this remote"
            ))),
            (other, _) => Err(Error::corrupt(format!("{key}: unknown framing {other}"))),
        }
    }

    /// The key version a sealed object's data key is wrapped under.
    pub fn version_of(framed: &[u8]) -> Option<u32> {
        (framed.len() > 10 && &framed[..4] == MAGIC && framed[4] == SEALED)
            .then(|| u32::from_be_bytes(framed[6..10].try_into().unwrap_or_default()))
    }

    /// The same object with its data key wrapped under the current
    /// version, or `None` when it already is (or the remote is plain).
    pub fn rewrap(&self, key: &str, framed: &[u8]) -> Result<Option<Vec<u8>>> {
        let Mode::Sealed { current, keks, .. } = &self.mode else {
            return Ok(None);
        };
        let Some(version) = Sealer::version_of(framed) else {
            return Ok(None);
        };
        if version == *current {
            return Ok(None);
        }
        let old = keks
            .get(&version)
            .ok_or_else(|| Error::refused(format!("{key}: key version {version} is not loaded")))?;
        let new = keks
            .get(current)
            .ok_or_else(|| Error::local("the current key is not loaded"))?;
        let dk = aead_open(old, &dk_aad(key), &framed[10..10 + WRAPPED_LEN])
            .map_err(|_| Error::corrupt(format!("{key}: its data key does not open")))?;
        let wrapped = aead_seal(new, &dk_aad(key), &dk)?;
        let mut out = framed.to_vec();
        out[6..10].copy_from_slice(&current.to_be_bytes());
        out[10..10 + WRAPPED_LEN].copy_from_slice(&wrapped);
        Ok(Some(out))
    }
}

fn dk_aad(key: &str) -> Vec<u8> {
    let mut aad = b"branchyard-sync dk\0".to_vec();
    aad.extend_from_slice(key.as_bytes());
    aad
}

fn data_aad(algorithm: Algorithm, key: &str) -> Vec<u8> {
    let mut aad = MAGIC.to_vec();
    aad.push(SEALED);
    aad.push(algorithm.byte());
    aad.extend_from_slice(key.as_bytes());
    aad
}

/// Read `keyring.json`, or make it when the remote has none, and the
/// sealer it describes. Refuses a remote whose encryption differs from
/// what was asked (a plain config against a sealed remote, or the
/// reverse), and a wrapper that cannot open its keys.
pub fn open_keyring(
    store: &dyn ObjectStore,
    encryption: &Encryption,
    now_ms: u64,
) -> Result<Sealer> {
    loop {
        match store.get(KEYRING) {
            Ok(object) => {
                let file: KeyringFile = serde_json::from_slice(&object.data)?;
                return sealer_from(&file, encryption);
            }
            Err(e) if e.is(Kind::NotFound) => {
                let (file, sealer) = new_keyring(encryption, now_ms)?;
                match store.put_if_absent(KEYRING, &serde_json::to_vec_pretty(&file)?) {
                    Ok(_) => return Ok(sealer),
                    // Another machine made it first: read theirs.
                    Err(e) if e.is(Kind::Precondition) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read `keyring.json`.
pub fn read_keyring(store: &dyn ObjectStore) -> Result<KeyringFile> {
    Ok(serde_json::from_slice(&store.get(KEYRING)?.data)?)
}

/// The sealer `file` describes, opened with `encryption`.
pub fn sealer_for(file: &KeyringFile, encryption: &Encryption) -> Result<Sealer> {
    sealer_from(file, encryption)
}

fn new_keyring(encryption: &Encryption, now_ms: u64) -> Result<(KeyringFile, Sealer)> {
    Ok(match encryption {
        Encryption::None => (
            KeyringFile {
                format: 1,
                encryption: "none".into(),
                algorithm: None,
                current: 0,
                keys: Vec::new(),
                names: None,
                names_version: 0,
            },
            Sealer::plain(),
        ),
        Encryption::Envelope { wrapper, algorithm } => {
            let secret = random_bytes(32)?;
            let names = random_bytes(32)?;
            let k = kek(&secret)?;
            let sealer = Sealer {
                mode: Mode::Sealed {
                    algorithm: *algorithm,
                    current: 1,
                    keks: BTreeMap::from([(1, k)]),
                    names: names
                        .clone()
                        .try_into()
                        .map_err(|_| Error::local("names key"))?,
                },
            };
            (
                KeyringFile {
                    format: 1,
                    encryption: "envelope".into(),
                    algorithm: Some(*algorithm),
                    current: 1,
                    keys: vec![KeyVersion {
                        version: 1,
                        wrapper: wrapper.describe(),
                        wrapped: wrapper.wrap(&secret)?,
                        created_ms: now_ms,
                        superseded_ms: None,
                    }],
                    names: Some(b64(&aead_seal(&k, b"names", &names)?)),
                    names_version: 1,
                },
                sealer,
            )
        }
    })
}

fn sealer_from(file: &KeyringFile, encryption: &Encryption) -> Result<Sealer> {
    if file.format != 1 {
        return Err(Error::config(format!(
            "this remote's keyring is format {}, which this build does not read",
            file.format
        )));
    }
    match (file.encryption.as_str(), encryption) {
        ("none", Encryption::None) => Ok(Sealer::plain()),
        ("none", Encryption::Envelope { .. }) => Err(Error::config(
            "this remote is not encrypted, but [sync] encrypt is set; use another remote, or \
             remove encrypt",
        )),
        ("envelope", Encryption::None) => Err(Error::config(
            "this remote is encrypted; set [sync] encrypt (a passphrase or kms:// URL) to use it",
        )),
        ("envelope", Encryption::Envelope { wrapper, .. }) => sealed_from(file, wrapper.as_ref()),
        (other, _) => Err(Error::config(format!(
            "this remote's encryption {other:?} is unknown to this build"
        ))),
    }
}

fn sealed_from(file: &KeyringFile, wrapper: &dyn Wrapper) -> Result<Sealer> {
    {
        {
            let mut keks = BTreeMap::new();
            for version in &file.keys {
                let secret = wrapper.unwrap(&version.wrapped).map_err(|e| {
                    Error::refused(format!(
                        "key version {} (wrapped by {}) does not open: {e}",
                        version.version, version.wrapper
                    ))
                })?;
                keks.insert(version.version, kek(&secret)?);
            }
            let names_kek = keks
                .get(&file.names_version)
                .ok_or_else(|| Error::corrupt("the keyring's names key has no key version"))?;
            let names = aead_open(
                names_kek,
                b"names",
                &unb64(file.names.as_deref().unwrap_or(""))?,
            )?;
            Ok(Sealer {
                mode: Mode::Sealed {
                    algorithm: file.algorithm.unwrap_or(Algorithm::Aes256Gcm),
                    current: file.current,
                    keks,
                    names: names
                        .try_into()
                        .map_err(|_| Error::corrupt("the names key is not 32 bytes"))?,
                },
            })
        }
    }
}

/// What a rotation did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    pub new_version: u32,
    pub rewrapped: usize,
    pub already_current: usize,
    pub retired: Vec<u32>,
}

/// Rotate the tenant key: add a version (its secret wrapped by `to`, or
/// by `wrapper` when `to` is `None`, which also changes the passphrase or
/// KMS key), rewrap every object's data key under it, then drop the
/// versions nothing uses that were superseded at least `grace_ms` ago.
/// Safe to run again after a stop: each step is a conditional write, and
/// objects already rewrapped are skipped.
///
/// The version this rotation supersedes is never retired by it. Another
/// process that opened the keyring before the rotation still seals under
/// that version, and could store an object after the scan for versions
/// in use and before the keyring is swapped; retiring it then would leave
/// that object unreadable. Writers re-read the keyring before publishing
/// a manifest and at least once a minute otherwise, so by the time a
/// version has been superseded for the grace period (longer than any
/// upload, at least 15 minutes) none still seals under it, and every
/// object sealed under it is there for the scan to see.
pub fn rotate(
    store: &dyn ObjectStore,
    wrapper: &dyn Wrapper,
    to: Option<&dyn Wrapper>,
    now_ms: u64,
    grace_ms: u64,
) -> Result<(Sealer, Rotation)> {
    let to = to.unwrap_or(wrapper);
    // 1. A new version, with the names key sealed under it.
    let (file, sealer) = loop {
        let object = store.get(KEYRING)?;
        let mut file: KeyringFile = serde_json::from_slice(&object.data)?;
        if file.encryption != "envelope" {
            return Err(Error::config("only an encrypted remote has keys to rotate"));
        }
        let old = sealed_from(&file, wrapper)?;
        let Mode::Sealed { names, keks, .. } = &old.mode else {
            unreachable!()
        };
        let version = file.keys.iter().map(|k| k.version).max().unwrap_or(0) + 1;
        let secret = random_bytes(32)?;
        let k = kek(&secret)?;
        // Every version is rewrapped by the new wrapper, so one wrapper
        // opens the whole keyring afterwards.
        for existing in file.keys.iter_mut() {
            let secret = wrapper.unwrap(&existing.wrapped)?;
            existing.wrapped = to.wrap(&secret)?;
            existing.wrapper = to.describe();
            existing.superseded_ms.get_or_insert(now_ms);
        }
        file.keys.push(KeyVersion {
            version,
            wrapper: to.describe(),
            wrapped: to.wrap(&secret)?,
            created_ms: now_ms,
            superseded_ms: None,
        });
        file.current = version;
        file.names = Some(b64(&aead_seal(&k, b"names", names)?));
        file.names_version = version;
        let mut keks = keks.clone();
        keks.insert(version, k);
        match store.put_if_match(
            KEYRING,
            &serde_json::to_vec_pretty(&file)?,
            &object.generation,
        ) {
            Ok(_) => {
                let sealer = Sealer {
                    mode: Mode::Sealed {
                        algorithm: file.algorithm.unwrap_or(Algorithm::Aes256Gcm),
                        current: version,
                        keks,
                        names: *names,
                    },
                };
                break (file, sealer);
            }
            Err(e) if e.is(Kind::Precondition) => continue,
            Err(e) => return Err(e),
        }
    };
    // 2. Rewrap every object's data key.
    let mut report = Rotation {
        new_version: file.current,
        ..Rotation::default()
    };
    for entry in store.list("")? {
        if entry.key == KEYRING {
            continue;
        }
        let object = match store.get(&entry.key) {
            Ok(o) => o,
            Err(e) if e.is(Kind::NotFound) => continue,
            Err(e) => return Err(e),
        };
        match sealer.rewrap(&entry.key, &object.data)? {
            None => report.already_current += 1,
            Some(rewrapped) => {
                match store.put_if_match(&entry.key, &rewrapped, &object.generation) {
                    Ok(_) => report.rewrapped += 1,
                    // Replaced meanwhile, by a writer that sealed it under
                    // the current version.
                    Err(e) if e.is(Kind::Precondition) || e.is(Kind::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    // 3. Retire versions no object uses any more, superseded longer ago
    //    than the grace period.
    loop {
        let mut in_use = std::collections::BTreeSet::new();
        for entry in store.list("")? {
            if entry.key == KEYRING {
                continue;
            }
            if let Ok(object) = store.get_range(&entry.key, 0, 10) {
                if let Some(v) = Sealer::version_of(&object) {
                    in_use.insert(v);
                }
            }
        }
        let object = store.get(KEYRING)?;
        let mut file: KeyringFile = serde_json::from_slice(&object.data)?;
        let current = file.current;
        let retired: Vec<u32> = file
            .keys
            .iter()
            .filter(|k| {
                k.version != current
                    && !in_use.contains(&k.version)
                    && k.superseded_ms
                        .is_some_and(|at| at.saturating_add(grace_ms) <= now_ms)
            })
            .map(|k| k.version)
            .collect();
        if retired.is_empty() {
            break;
        }
        file.keys.retain(|k| !retired.contains(&k.version));
        match store.put_if_match(
            KEYRING,
            &serde_json::to_vec_pretty(&file)?,
            &object.generation,
        ) {
            Ok(_) => {
                report.retired = retired;
                break;
            }
            Err(e) if e.is(Kind::Precondition) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((sealer, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::Passphrase;
    use crate::store::memory::MemoryStore;

    const GRACE: u64 = 15 * 60 * 1000;

    fn passphrase(p: &str) -> Encryption {
        Encryption::Envelope {
            wrapper: Box::new(Passphrase::new(p).unwrap().with_iterations(10)),
            algorithm: Algorithm::Chacha20Poly1305,
        }
    }

    #[test]
    fn plain_objects_carry_a_checksum() {
        let s = Sealer::plain();
        let framed = s.seal("k", b"data").unwrap();
        assert_eq!(s.open("k", &framed).unwrap(), b"data");
        let mut bad = framed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(s.open("k", &bad).unwrap_err().kind, Kind::Corrupt);
        let hash = blake3::hash(b"data");
        assert_eq!(s.name("chunk", &hash), hash.to_hex().to_string());
    }

    #[test]
    fn sealed_objects_hide_and_bind() {
        let store = MemoryStore::new();
        let s = open_keyring(&store, &passphrase("a good passphrase"), 0).unwrap();
        assert!(s.sealed());
        let framed = s.seal("chunks/ab/x", b"secret plaintext").unwrap();
        assert!(!framed.windows(6).any(|w| w == b"secret"));
        assert_eq!(s.open("chunks/ab/x", &framed).unwrap(), b"secret plaintext");
        // Moved to another name: refused.
        assert_eq!(
            s.open("chunks/ab/y", &framed).unwrap_err().kind,
            Kind::Corrupt
        );
        // Names are keyed: not the content hash.
        let hash = blake3::hash(b"secret plaintext");
        assert_ne!(s.name("chunk", &hash), hash.to_hex().to_string());
        assert_ne!(s.task_dir("board-update"), "board-update");
        // The same keyring opens it again; the wrong passphrase does not.
        let again = open_keyring(&store, &passphrase("a good passphrase"), 0).unwrap();
        assert_eq!(
            again.open("chunks/ab/x", &framed).unwrap(),
            b"secret plaintext"
        );
        assert_eq!(again.name("chunk", &hash), s.name("chunk", &hash));
        let wrong = open_keyring(&store, &passphrase("the wrong passphrase"), 0)
            .err()
            .unwrap();
        assert_eq!(wrong.kind, Kind::Refused, "{wrong}");
        // A plain config against a sealed remote, and the reverse.
        assert_eq!(
            open_keyring(&store, &Encryption::None, 0)
                .err()
                .unwrap()
                .kind,
            Kind::Config
        );
        let plain = MemoryStore::new();
        open_keyring(&plain, &Encryption::None, 0).unwrap();
        assert_eq!(
            open_keyring(&plain, &passphrase("a good passphrase"), 0)
                .err()
                .unwrap()
                .kind,
            Kind::Config
        );
    }

    #[test]
    fn rotation_rewraps_and_retires() {
        let store = MemoryStore::new();
        let old = Passphrase::new("first passphrase")
            .unwrap()
            .with_iterations(10);
        let new = Passphrase::new("second passphrase")
            .unwrap()
            .with_iterations(10);
        let s = open_keyring(&store, &passphrase("first passphrase"), 0).unwrap();
        for i in 0..3 {
            let key = format!("chunks/aa/{i}");
            store
                .put_if_absent(
                    &key,
                    &s.seal(&key, format!("object {i}").as_bytes()).unwrap(),
                )
                .unwrap();
        }
        let (rotated, report) = rotate(&store, &old, Some(&new), 5, GRACE).unwrap();
        assert_eq!(report.new_version, 2);
        assert_eq!(report.rewrapped, 3);
        // The version just superseded stays for the grace period.
        assert!(report.retired.is_empty(), "{report:?}");
        // The new passphrase opens everything; names did not change.
        let s2 = open_keyring(&store, &passphrase("second passphrase"), 0).unwrap();
        assert_eq!(s2.current_version(), Some(2));
        for i in 0..3 {
            let key = format!("chunks/aa/{i}");
            let framed = store.get(&key).unwrap().data;
            assert_eq!(Sealer::version_of(&framed), Some(2));
            assert_eq!(
                s2.open(&key, &framed).unwrap(),
                format!("object {i}").as_bytes()
            );
        }
        assert_eq!(s2.task_dir("t"), s.task_dir("t"));
        assert_eq!(rotated.task_dir("t"), s.task_dir("t"));
        assert_eq!(
            open_keyring(&store, &passphrase("first passphrase"), 0)
                .err()
                .unwrap()
                .kind,
            Kind::Refused
        );
        // Running it again is harmless; once the grace period has passed,
        // the version superseded then is retired (not the one superseded
        // now).
        let (_, again) = rotate(&store, &new, None, 6, GRACE).unwrap();
        assert_eq!((again.new_version, again.rewrapped), (3, 3));
        assert!(again.retired.is_empty(), "{again:?}");
        let (_, later) = rotate(&store, &new, None, 5 + GRACE, GRACE).unwrap();
        assert_eq!((later.new_version, later.rewrapped), (4, 3));
        assert_eq!(later.retired, vec![1]);
        let file = read_keyring(&store).unwrap();
        assert_eq!(
            file.keys.iter().map(|k| k.version).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn a_writer_that_opened_the_keyring_before_a_rotation_stays_readable() {
        let store = MemoryStore::new();
        let wrapper = Passphrase::new("passphrase").unwrap().with_iterations(10);
        // A writer opens the remote: it seals under version 1.
        let writer = open_keyring(&store, &passphrase("passphrase"), 0).unwrap();
        store
            .put_if_absent(
                "chunks/aa/early",
                &writer.seal("chunks/aa/early", b"early").unwrap(),
            )
            .unwrap();
        // A rotation runs to the end on another machine: version 2 is
        // current, every object it saw rewrapped, the keyring swapped.
        let (_, report) = rotate(&store, &wrapper, None, 10, GRACE).unwrap();
        assert_eq!((report.new_version, report.rewrapped), (2, 1));
        // The writer has not re-read the keyring yet: what it stores now
        // (after the rotation's scan for versions in use) is sealed under
        // version 1, and must stay readable.
        store
            .put_if_absent(
                "chunks/aa/late",
                &writer.seal("chunks/aa/late", b"late").unwrap(),
            )
            .unwrap();
        let reader = open_keyring(&store, &passphrase("passphrase"), 0).unwrap();
        for (key, plain) in [
            ("chunks/aa/early", b"early".as_slice()),
            ("chunks/aa/late", b"late"),
        ] {
            assert_eq!(
                reader.open(key, &store.get(key).unwrap().data).unwrap(),
                plain,
                "{key}"
            );
        }
        // The writer, re-reading the keyring, sees it is stale and takes
        // the current version.
        let file = read_keyring(&store).unwrap();
        assert!(!writer.matches(&file));
        assert!(reader.matches(&file));
        // A rotation after the grace period rewraps the late object, and
        // only then is version 1 retired.
        let (_, report) = rotate(&store, &wrapper, None, 10 + GRACE, GRACE).unwrap();
        assert_eq!(report.retired, vec![1]);
        let reader = open_keyring(&store, &passphrase("passphrase"), 0).unwrap();
        let late = store.get("chunks/aa/late").unwrap().data;
        assert_eq!(Sealer::version_of(&late), Some(3));
        assert_eq!(reader.open("chunks/aa/late", &late).unwrap(), b"late");
    }
}
