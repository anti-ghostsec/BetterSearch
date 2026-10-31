//! Image bytes held for the local API, in memory only.
//!
//! The app never writes pictures anywhere: thumbnails were already memory-only
//! and a consumer polling `/file` shouldn't change that. So a repeat request
//! inside one run is served from here instead of from the site again, and the
//! cache dies with the process like everything else.
//!
//! Nothing is decrypted to get it out — these are the bytes as downloaded — so
//! there's no key to involve and nothing on disk to leave behind.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// How much may be held at once.
pub const CAP: usize = 48 * 1024 * 1024;

pub struct Cache {
	cap: usize,
	used: usize,
	clock: u64,
	order: VecDeque<(u64, String)>,
	entries: HashMap<String, (u64, Arc<Vec<u8>>)>,
}

impl Cache {
	pub fn new(cap: usize) -> Self {
		Self { cap, used: 0, clock: 0, order: VecDeque::new(), entries: HashMap::new() }
	}

	pub fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
		let bytes = self.entries.get(key)?.1.clone();
		// Re-queue so the newest touch is last to be evicted.
		self.clock += 1;
		self.order.push_back((self.clock, key.to_string()));
		self.entries.insert(key.to_string(), (self.clock, bytes.clone()));
		Some(bytes)
	}

	pub fn put(&mut self, key: &str, bytes: Vec<u8>) {
		if bytes.len() > self.cap {
			return;
		}
		if let Some((_, old)) = self.entries.remove(key) {
			self.used = self.used.saturating_sub(old.len());
		}
		self.clock += 1;
		let at = self.clock;
		self.used += bytes.len();
		self.entries.insert(key.to_string(), (at, Arc::new(bytes)));
		self.order.push_back((at, key.to_string()));
		while self.used > self.cap {
			let Some((at, key)) = self.order.pop_front() else { break };
			// A stale queue entry (the key was touched or replaced since) keeps
			// the current one.
			if self.entries.get(&key).is_some_and(|(cur, _)| *cur == at)
				&& let Some((_, old)) = self.entries.remove(&key)
			{
				self.used = self.used.saturating_sub(old.len());
			}
		}
	}

	pub fn clear(&mut self) {
		self.entries.clear();
		self.order.clear();
		self.used = 0;
	}

	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn bytes(&self) -> usize {
		self.used
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keeps_what_fits_and_drops_the_oldest() {
		let mut c = Cache::new(10);
		c.put("a", vec![1; 6]);
		c.get("a");
		c.put("b", vec![2; 6]);
		assert!(c.get("a").is_none(), "a was the oldest, touched or not");
		assert!(c.get("b").is_some());
		c.put("c", vec![3; 10]);
		assert!(c.get("b").is_none());
		assert_eq!(c.bytes(), 10);
	}

	#[test]
	fn an_oversized_file_is_not_kept() {
		let mut c = Cache::new(4);
		c.put("big", vec![1; 5]);
		assert_eq!(c.len(), 0);
		assert_eq!(c.bytes(), 0);
	}

	#[test]
	fn replacing_a_key_counts_once() {
		let mut c = Cache::new(10);
		c.put("a", vec![1; 8]);
		c.put("a", vec![2; 8]);
		assert_eq!(c.bytes(), 8);
		assert_eq!(c.get("a").unwrap().len(), 8);
	}

	#[test]
	fn clear_empties_it() {
		let mut c = Cache::new(10);
		c.put("a", vec![1; 4]);
		c.clear();
		assert_eq!((c.len(), c.bytes()), (0, 0));
	}
}
