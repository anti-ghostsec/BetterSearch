use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a site rated a post. Sites with more levels map onto these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Rating {
	Safe,
	Questionable,
	Explicit,
}

impl Rating {
	pub const ALL: [Rating; 3] = [Rating::Safe, Rating::Questionable, Rating::Explicit];

	/// The letter boorus use in `rating:` searches.
	pub fn letter(self) -> char {
		match self {
			Rating::Safe => 's',
			Rating::Questionable => 'q',
			Rating::Explicit => 'e',
		}
	}

	/// Reads `s`, `safe`, `q`, `questionable`, `e`, `explicit`.
	pub fn from_name(s: &str) -> Option<Rating> {
		// Danbooru-style names: general is Safe, sensitive leans Questionable.
		match s.to_ascii_lowercase().as_str() {
			"g" | "general" => return Some(Rating::Safe),
			"sensitive" => return Some(Rating::Questionable),
			_ => {}
		}
		match s.chars().next()?.to_ascii_lowercase() {
			's' => Some(Rating::Safe),
			'q' => Some(Rating::Questionable),
			'e' => Some(Rating::Explicit),
			_ => None,
		}
	}

	pub fn label(self) -> &'static str {
		match self {
			Rating::Safe => "Safe",
			Rating::Questionable => "Questionable",
			Rating::Explicit => "Explicit",
		}
	}
}

/// A post from any site, in one shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Post {
	/// The site id it came from, e.g. "e621".
	pub site: String,
	pub id: u64,
	/// Every tag, sorted, in booru form (`long_hair`).
	pub tags: Vec<String>,
	/// Tags by the site's category (artist, character, species, …), when it has them.
	pub categories: BTreeMap<String, Vec<String>>,
	pub rating: Rating,
	pub score: i64,
	pub fav_count: u64,
	pub width: u32,
	pub height: u32,
	/// File extension: jpg, png, gif, webm, …
	pub ext: String,
	pub md5: Option<String>,
	/// Missing when the site hides the file (e.g. it needs a login).
	pub file_url: Option<String>,
	pub sample_url: Option<String>,
	pub preview_url: Option<String>,
	/// The post's page on the site, for "open in browser".
	pub page_url: String,
	pub sources: Vec<String>,
	pub created_at: String,
}

impl Post {
	/// The item key the tag graph and cache use: "e621:post/123".
	pub fn key(&self) -> String {
		item_key(&self.site, self.id)
	}

	pub fn has_tag(&self, tag: &str) -> bool {
		self.tags.binary_search_by(|t| t.as_str().cmp(tag)).is_ok()
	}
}

pub fn item_key(site: &str, id: u64) -> String {
	format!("{site}:post/{id}")
}
