use std::path::PathBuf;

use bettersearch_core::{Rating, Store};
use serde::{Deserialize, Serialize};

/// Kept as JSON inside the encrypted database, so it's only readable once unlocked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
	/// Goes in the User-Agent; e621 asks for your username there.
	pub contact: String,
	/// Optional e621 login: username and an API key from the account settings page.
	pub e621_user: String,
	pub e621_key: String,
	/// Danbooru login: account name and an API key from its profile page.
	/// Raises the 2-tag search limit for Gold accounts and up.
	pub danbooru_user: String,
	pub danbooru_key: String,
	/// gelbooru.com login: the user id and API key from its account options page.
	pub gelbooru_user: String,
	pub gelbooru_key: String,
	/// The site from before several could be picked; read once into `sites`.
	#[serde(skip_serializing)]
	pub site: String,
	/// Sites searched together.
	pub sites: Vec<String>,
	pub ratings: Vec<Rating>,
	pub last_search: String,
	/// Fill in links made before on posts as they load.
	pub auto_links: bool,
	/// The tags panel on the left.
	pub show_tags: bool,
	/// Show posts the site sent without any image (e621 does that for logged-out
	/// users on posts with tags from its default blacklist).
	pub show_imageless: bool,
}

impl Default for Settings {
	fn default() -> Self {
		Settings {
			contact: String::new(),
			e621_user: String::new(),
			e621_key: String::new(),
			danbooru_user: String::new(),
			danbooru_key: String::new(),
			gelbooru_user: String::new(),
			gelbooru_key: String::new(),
			site: String::new(),
			sites: vec!["e926".into()],
			ratings: vec![Rating::Safe],
			last_search: String::new(),
			auto_links: true,
			show_tags: true,
			show_imageless: false,
		}
	}
}

pub fn data_dir() -> PathBuf {
	let dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("bettersearch");
	let _ = std::fs::create_dir_all(&dir);
	dir
}

impl Settings {
	/// The plain-text settings file from before the vault, if it's still around.
	pub fn legacy_path() -> PathBuf {
		data_dir().join("settings.json")
	}

	pub fn load(store: &Store) -> Settings {
		store.value("settings").ok().flatten().map(|json| Self::from_json(&json)).unwrap_or_default()
	}

	pub fn save(&self, store: &Store) {
		if let Ok(json) = serde_json::to_string(self) {
			let _ = store.set_value("settings", &json);
		}
	}

	pub fn from_json(json: &str) -> Settings {
		let mut s: Settings = serde_json::from_str(json).unwrap_or_default();
		// Files from before several sites could be picked name just one.
		if !s.site.is_empty() && !s.sites.contains(&s.site) {
			s.sites = vec![std::mem::take(&mut s.site)];
		}
		s
	}
}

#[cfg(test)]
mod tests {
	use super::Settings;

	#[test]
	fn reads_old_single_site_files() {
		assert_eq!(Settings::from_json(r#"{"site":"e621"}"#).sites, vec!["e621"]);
		assert_eq!(Settings::from_json(r#"{"sites":["e926","danbooru"]}"#).sites, vec!["e926", "danbooru"]);
		assert_eq!(Settings::from_json("{}").sites, vec!["e926"]);
	}
}
