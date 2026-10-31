// The one file the app writes: everything (settings, history, links, posts
// seen) as an encrypted SQLite image. It's only ever decrypted into memory.
//
//   "BSV1" | m_cost u32 | t_cost u32 | lanes u32 | salt 16 | nonce 24 | ciphertext
//
// The key comes from your PIN with Argon2id, made deliberately slow and
// memory-hungry; the data is sealed with XChaCha20-Poly1305, with the header
// as associated data so it can't be swapped. A wrong PIN fails the tag check.

use std::io::Write;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{AeadCore, AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"BSV1";
const SALT: usize = 16;
const NONCE: usize = 24;
const TAG: usize = 16;
/// Bytes of header the seal covers: magic, cost settings and salt.
const SEALED_HEADER: usize = 4 + 12 + SALT;
/// Argon2id: 192 MiB, 3 passes. About a second on a desktop, per guess.
#[cfg(not(test))]
const M_COST: u32 = 192 * 1024;
#[cfg(not(test))]
const T_COST: u32 = 3;
/// Tests make real keys with tiny costs, so a run doesn't take minutes.
#[cfg(test)]
const M_COST: u32 = 64;
#[cfg(test)]
const T_COST: u32 = 1;
const LANES: u32 = 1;
/// Shortest PIN that can be chosen. At a second a guess, someone with a copy
/// of the vault gets through every 4-digit PIN in about 3 hours on one core,
/// every 6-digit one in about 12 days (less with more cores). Vaults from when
/// 4 was the least still open with their PIN, then ask for a new one.
pub const MIN_PIN: usize = 6;

#[derive(Debug)]
pub enum VaultError {
	WrongPin,
	Damaged(String),
	Io(std::io::Error),
}

impl std::fmt::Display for VaultError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			VaultError::WrongPin => write!(f, "Wrong PIN."),
			VaultError::Damaged(why) => write!(f, "The saved data can't be read: {why}"),
			VaultError::Io(e) => write!(f, "Couldn't read or write the saved data: {e}"),
		}
	}
}

impl From<std::io::Error> for VaultError {
	fn from(e: std::io::Error) -> Self {
		VaultError::Io(e)
	}
}

/// A key from a PIN, with the salt and costs it was made with. Wiped from
/// memory when dropped.
pub struct Key {
	key: Zeroizing<[u8; 32]>,
	header: [u8; SEALED_HEADER],
}

impl Key {
	/// A key for a new PIN, with a fresh salt.
	pub fn new(pin: &str) -> Result<Key, VaultError> {
		let mut salt = [0u8; SALT];
		OsRng.fill_bytes(&mut salt);
		Self::derive(pin, M_COST, T_COST, LANES, salt)
	}

	/// A key made cheaply, for tests that don't care how slow a guess is.
	#[cfg(test)]
	pub fn for_tests() -> Key {
		Self::derive("test", 64, 1, 1, [7; SALT]).unwrap()
	}

	fn derive(pin: &str, m: u32, t: u32, p: u32, salt: [u8; SALT]) -> Result<Key, VaultError> {
		let params = Params::new(m, t, p, Some(32)).map_err(|e| VaultError::Damaged(e.to_string()))?;
		let mut key = Zeroizing::new([0u8; 32]);
		Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
			.hash_password_into(pin.as_bytes(), &salt, key.as_mut())
			.map_err(|e| VaultError::Damaged(e.to_string()))?;
		let mut header = [0u8; SEALED_HEADER];
		header[..4].copy_from_slice(MAGIC);
		header[4..8].copy_from_slice(&m.to_le_bytes());
		header[8..12].copy_from_slice(&t.to_le_bytes());
		header[12..16].copy_from_slice(&p.to_le_bytes());
		header[16..].copy_from_slice(&salt);
		Ok(Key { key, header })
	}

	fn cipher(&self) -> XChaCha20Poly1305 {
		XChaCha20Poly1305::new(self.key.as_ref().into())
	}
}

#[derive(Clone)]
pub struct Vault {
	path: PathBuf,
}

impl Vault {
	pub fn in_dir(dir: &Path) -> Vault {
		Vault { path: dir.join("vault.bin") }
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	pub fn exists(&self) -> bool {
		self.path.exists()
	}

	/// Opens the vault with a PIN: the key (to save with later) and the data.
	pub fn unlock(&self, pin: &str) -> Result<(Key, Zeroizing<Vec<u8>>), VaultError> {
		let file = std::fs::read(&self.path)?;
		if file.len() < SEALED_HEADER + NONCE + TAG || &file[..4] != MAGIC {
			return Err(VaultError::Damaged("not a BetterSearch vault".into()));
		}
		let num = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
		let (m, t, p) = (num(4), num(8), num(12));
		// Costs come from the file, so refuse absurd ones rather than run out of memory.
		if m > 4 * 1024 * 1024 || t > 64 || p > 64 {
			return Err(VaultError::Damaged("its settings are out of range".into()));
		}
		let salt: [u8; SALT] = file[16..SEALED_HEADER].try_into().unwrap();
		let key = Key::derive(pin, m, t, p, salt)?;
		let nonce = XNonce::from_slice(&file[SEALED_HEADER..SEALED_HEADER + NONCE]);
		// Opened in place, in a buffer wiped when dropped: the data is never
		// in memory unsealed anywhere else.
		let mut plain = Zeroizing::new(file[SEALED_HEADER + NONCE..].to_vec());
		key.cipher().decrypt_in_place(nonce, &key.header, &mut *plain).map_err(|_| VaultError::WrongPin)?;
		Ok((key, plain))
	}

	/// Seals `plain` with `key` and replaces the file, so a crash mid-write
	/// leaves the old one whole.
	pub fn write(&self, key: &Key, plain: &[u8]) -> Result<(), VaultError> {
		let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
		// Sealed in place, with room for the tag so the buffer never moves,
		// and wiped when dropped in case sealing fails halfway.
		let mut sealed = Zeroizing::new(Vec::with_capacity(plain.len() + TAG));
		sealed.extend_from_slice(plain);
		key.cipher().encrypt_in_place(&nonce, &key.header, &mut *sealed).map_err(|e| VaultError::Damaged(e.to_string()))?;
		let tmp = self.path.with_extension("tmp");
		{
			let mut f = open_private(&tmp)?;
			f.write_all(&key.header)?;
			f.write_all(&nonce)?;
			f.write_all(&sealed)?;
			f.sync_all()?;
		}
		std::fs::rename(&tmp, &self.path)?;
		Ok(())
	}

	/// Deletes the vault, for starting over after a forgotten PIN.
	pub fn erase(&self) -> Result<(), VaultError> {
		std::fs::remove_file(&self.path)?;
		Ok(())
	}
}

/// A new file only this user can read.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
	let mut opts = std::fs::OpenOptions::new();
	opts.write(true).create(true).truncate(true);
	#[cfg(unix)]
	std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
	opts.open(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The real costs take a second; these keep the test quick.
	fn quick_key(pin: &str) -> Key {
		Key::derive(pin, 64, 1, 1, [7; SALT]).unwrap()
	}

	#[test]
	fn seals_and_opens() {
		let dir = std::env::temp_dir().join(format!("bs-vault-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let v = Vault::in_dir(&dir);
		v.write(&quick_key("1234"), b"secret settings").unwrap();
		let raw = std::fs::read(v.path()).unwrap();
		assert!(!raw.windows(6).any(|w| w == b"secret"));
		let (_, plain) = v.unlock("1234").unwrap();
		assert_eq!(plain.as_slice(), b"secret settings");
		assert!(matches!(v.unlock("1235"), Err(VaultError::WrongPin)));
		// A changed byte in the header (here the salt) fails as well.
		let mut bad = raw.clone();
		bad[20] ^= 1;
		std::fs::write(v.path(), &bad).unwrap();
		assert!(matches!(v.unlock("1234"), Err(VaultError::WrongPin)));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
