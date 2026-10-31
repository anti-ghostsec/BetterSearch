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
	/// Optional e6ai login; it's an e621 fork with its own accounts.
	pub e6ai_user: String,
	pub e6ai_key: String,
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
	/// The Taste tab and the model behind it. Off until switched on.
	pub taste: Taste,
	/// The local API other programs can search through. Off until switched on.
	pub api: Api,
	/// Boorus you added yourself.
	pub custom_sites: Vec<CustomSite>,
}

/// Which API a site speaks, so the right adapter is built for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
	/// Gelbooru 0.2 (`index.php?page=dapi`): most small and large boorus.
	#[default]
	Gelbooru,
	/// Danbooru 2 (`posts.json`).
	Danbooru,
	/// e621 and its forks.
	E621,
	/// Moebooru (`post.json`): yande.re, konachan and friends.
	Moebooru,
}

impl Family {
	pub fn label(self) -> &'static str {
		match self {
			Self::Gelbooru => "Gelbooru 0.2",
			Self::Danbooru => "Danbooru 2",
			Self::E621 => "e621",
			Self::Moebooru => "Moebooru",
		}
	}
}

/// A booru the user added. `id` is a slug used in saved searches and tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSite {
	pub id: String,
	pub name: String,
	pub base: String,
	pub family: Family,
	#[serde(default)]
	pub user: String,
	#[serde(default)]
	pub key: String,
}

/// Where the model calls go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
	/// No model: the tab still works, from tag weights alone.
	None,
	Venice,
	OpenAiCompatible,
	Anthropic,
	Local,
}

/// Taste settings. The tab is hidden unless `enabled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Taste {
	pub enabled: bool,
	pub provider: Provider,
	pub endpoint: String,
	pub api_key: String,
	pub model: String,
	/// A dial set by hand instead of measured from the favorites.
	pub dial_override: Option<f32>,
	/// Share of results that should be a stretch, 0 to 1.
	pub exploration: f32,
	/// Whether the README-able rating mix may go to the model.
	pub send_ratings: bool,
	pub max_calls_per_hour: u32,
}

/// The local API's settings. Off until `enabled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Api {
	pub enabled: bool,
	/// 0 picks a free port.
	pub port: u16,
	/// Reachable from the local network instead of only this computer.
	pub lan: bool,
	pub token: String,
	/// Read-only until switched off: no favorites, feedback or dial changes.
	pub read_only: bool,
	pub per_minute: u32,
	/// Origins allowed to call it from a browser page.
	pub cors: Vec<String>,
}

impl Default for Taste {
	fn default() -> Self {
		Self {
			enabled: false,
			provider: Provider::None,
			endpoint: String::new(),
			api_key: String::new(),
			model: String::new(),
			dial_override: None,
			exploration: 0.2,
			send_ratings: true,
			max_calls_per_hour: 60,
		}
	}
}

impl Default for Api {
	fn default() -> Self {
		Self { enabled: false, port: 0, lan: false, token: String::new(), read_only: true, per_minute: 30, cors: Vec::new() }
	}
}

impl Default for Settings {
	fn default() -> Self {
		Settings {
			contact: String::new(),
			e621_user: String::new(),
			e621_key: String::new(),
			e6ai_user: String::new(),
			e6ai_key: String::new(),
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
			taste: Taste::default(),
			api: Api::default(),
			custom_sites: Vec::new(),
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
		s.custom_sites = clean_custom(std::mem::take(&mut s.custom_sites));
		s
	}
}

/// Keeps only usable custom sites: https, a host, a unique slug.
pub fn clean_custom(list: Vec<CustomSite>) -> Vec<CustomSite> {
	let mut seen = std::collections::HashSet::new();
	list.into_iter()
		.filter_map(|mut c| {
			let base = c.base.trim().trim_end_matches('/').to_string();
			let host = base.strip_prefix("https://")?.split('/').next()?.to_lowercase();
			if host.is_empty() || host.contains(' ') {
				return None;
			}
			c.base = base;
			if c.id.trim().is_empty() {
				c.id = host.replace('.', "_");
			}
			c.id = c.id.trim().to_lowercase().replace(' ', "_");
			if c.name.trim().is_empty() {
				c.name = host;
			}
			seen.insert(c.id.clone()).then_some(c)
		})
		.collect()
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

	#[test]
	fn custom_sites_are_cleaned_and_deduped() {
		let j = r#"{"custom_sites":[
			{"id":"","name":"","base":"https://Booru.Example/","family":"gelbooru"},
			{"id":"","name":"","base":"https://booru.example","family":"danbooru"},
			{"id":"x","name":"X","base":"http://plain.example","family":"gelbooru"}]}"#;
		let s = Settings::from_json(j);
		// http is refused; the same host twice is folded into one.
		assert_eq!(s.custom_sites.len(), 1, "{:?}", s.custom_sites);
		assert_eq!(s.custom_sites[0].id, "booru_example");
		assert_eq!(s.custom_sites[0].base, "https://Booru.Example");
		assert_eq!(s.custom_sites[0].name, "booru.example");
	}

	#[test]
	fn taste_and_api_are_off_until_switched_on() {
		let s = Settings::from_json("{}");
		assert!(!s.taste.enabled);
		assert!(!s.api.enabled);
		assert_eq!(s.taste.provider, super::Provider::None);
		assert!(s.api.read_only);
		assert_eq!(s.taste.exploration, 0.2);
	}
}
