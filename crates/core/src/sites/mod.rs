// Sites, one adapter per API family. Each turns a site's JSON into `Post`s and
// says what its search box can take, so the planner knows what to send.

mod danbooru;
mod e621;
mod gelbooru;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use danbooru::Danbooru;
pub use e621::E621;
pub use gelbooru::Gelbooru;

use crate::{Post, Result};

/// What a site's own search can do.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
	/// Most tags and metatags in one query.
	pub max_tags: usize,
	/// Takes `~a ~b` as "a or b" (one such group per query).
	pub tilde_or: bool,
	/// Most wildcard tags in one query.
	pub max_wildcards: usize,
	/// Metatags it understands.
	pub metas: &'static [&'static str],
	/// Metatags that don't count toward `max_tags`.
	pub free_metas: &'static [&'static str],
	/// Most posts per page.
	pub page_size: usize,
}

/// Where the next page starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
	/// Newest first: posts with an id below this one.
	Before(u64),
	/// Numbered pages, for searches with their own `order:`.
	Page(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagSuggestion {
	pub name: String,
	pub category: String,
	pub count: u64,
	/// The name typed, when it's an alias of `name`.
	pub alias: Option<String>,
}

/// The tags on a sample of a tag's posts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
	/// Posts sampled.
	pub sampled: usize,
	/// Sampled posts with each tag (only tags that could be a "what").
	pub counts: HashMap<String, u32>,
	/// Each tag's post count on the whole site, for the tags counted.
	pub sizes: HashMap<String, u64>,
}

/// Tag categories that never answer "what": who drew it, which series, who's in it.
pub const NOT_THINGS: &[&str] = &["artist", "copyright", "character", "meta", "invalid", "lore", "contributor"];

pub struct Page {
	pub posts: Vec<Post>,
	pub next: Option<Cursor>,
}

pub trait Site: Send + Sync {
	/// Stable id used in item keys: "e621".
	fn id(&self) -> &str;
	fn name(&self) -> &str;
	fn caps(&self) -> Capabilities;
	/// One page of a query in the site's own syntax. `cursor: None` is the first page.
	fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page>;
	/// How many posts each tag is on, for picking the most selective ones.
	fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>>;
	/// Tags the site treats as another name: old → current.
	fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>>;
	fn autocomplete(&self, prefix: &str) -> Result<Vec<TagSuggestion>>;
	/// Fetches an image or thumbnail with the site's client.
	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>>;

	/// What tends to come with a tag, for learning relations. `None` where the
	/// site can't say cheaply.
	fn tag_profile(&self, _tag: &str) -> Result<Option<Profile>> {
		Ok(None)
	}

	/// About how many posts the site has, to tell common tags from telling ones.
	fn total_posts(&self) -> Result<Option<u64>> {
		Ok(None)
	}

	/// Other versions of a post's picture (edits, crops, reposts), from the
	/// site's picture search, closest first. `None` where the site has none
	/// open to this app.
	fn other_versions(&self, _post_id: u64) -> Result<Option<Vec<Post>>> {
		Ok(None)
	}

	/// Whether the site lets this app load its images. gelbooru.com doesn't:
	/// its image servers send anything not from its own pages to a hotlink page.
	fn shows_images(&self) -> bool {
		true
	}

	/// The only ratings the site has, when it doesn't have them all (e926 is
	/// Safe only), so a search without them can skip it.
	fn only_ratings(&self) -> Option<&'static [crate::Rating]> {
		None
	}

	/// Asks the site whether it accepts the login: `None` with no login set,
	/// the account name if it does, the site's answer if not.
	fn check_login(&self) -> Option<Result<String>> {
		None
	}
}

/// Most bytes one image or video download may bring in.
pub const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;

/// Downloads an image or video named in a site's answer: https only (no local
/// files, no plain-text links), and never more than `MAX_DOWNLOAD`, so a
/// broken or hostile server can't fill the memory.
pub fn download(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
	if !url.starts_with("https://") {
		return Err(crate::Error::Query("only https links are downloaded".into()));
	}
	let res = client.get(url).send()?.error_for_status()?;
	// A redirect mustn't land on plain http.
	if res.url().scheme() != "https" {
		return Err(crate::Error::Query("the download was redirected off https".into()));
	}
	// Hotlink protection answers with a web page (gelbooru.com/hotlink.php) instead of the image.
	let html = res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/html"));
	if html || res.url().path().contains("hotlink") {
		return Err(crate::Error::Query("the site sent a web page instead of the image".into()));
	}
	if res.content_length().is_some_and(|n| n > MAX_DOWNLOAD) {
		return Err(crate::Error::Query("the file is too big to load here".into()));
	}
	let mut out = Vec::new();
	use std::io::Read;
	res.take(MAX_DOWNLOAD + 1).read_to_end(&mut out).map_err(|e| crate::Error::Query(format!("download stopped: {e}")))?;
	if out.len() as u64 > MAX_DOWNLOAD {
		return Err(crate::Error::Query("the file is too big to load here".into()));
	}
	Ok(out)
}

/// Keeps requests to one site at least `gap` apart, across threads.
pub struct Throttle {
	gap: Duration,
	last: Mutex<Option<Instant>>,
}

impl Throttle {
	pub fn new(gap: Duration) -> Self {
		Throttle { gap, last: Mutex::new(None) }
	}

	pub fn wait(&self) {
		let mut last = self.last.lock().unwrap();
		if let Some(t) = *last {
			let since = t.elapsed();
			if since < self.gap {
				std::thread::sleep(self.gap - since);
			}
		}
		*last = Some(Instant::now());
	}
}

/// The User-Agent sites see. e621 asks for the project name and a contact.
pub fn user_agent(contact: &str) -> String {
	let contact = contact.trim();
	if contact.is_empty() {
		format!("BetterSearch/{} (desktop booru search)", env!("CARGO_PKG_VERSION"))
	} else {
		format!("BetterSearch/{} (by {contact})", env!("CARGO_PKG_VERSION"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn downloads_only_https() {
		let client = reqwest::blocking::Client::new();
		for url in ["http://example.invalid/a.png", "file:///etc/passwd", "-x"] {
			assert!(download(&client, url).is_err(), "{url}");
		}
	}

	/// A failed request's error doesn't carry the URL, where Gelbooru keeps the API key.
	#[test]
	fn errors_hide_the_url() {
		let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();
		let e: crate::Error = client.get("https://nowhere.invalid/index.php?api_key=SECRET123&user_id=42").send().unwrap_err().into();
		let shown = e.to_string();
		assert!(!shown.contains("SECRET123") && !shown.contains("user_id"), "{shown}");
	}

	/// Live: Gelbooru's image servers answer apps with their hotlink page;
	/// that's a clear error, not a picture that never loads.
	/// `cargo test -p bettersearch-core -- --ignored` (needs the network).
	#[test]
	#[ignore]
	fn gelbooru_hotlink_is_an_error() {
		let client = reqwest::blocking::Client::builder().user_agent(user_agent("")).build().unwrap();
		let e = download(&client, "https://img4.gelbooru.com/thumbnails/d5/09/thumbnail_d509cffc5c68c66faffc09617d575749.jpg").unwrap_err();
		assert!(e.to_string().contains("web page instead of the image"), "{e}");
	}

}
