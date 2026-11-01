// Sites, one adapter per API family. Each turns a site's JSON into `Post`s and
// says what its search box can take, so the planner knows what to send.

mod danbooru;
mod e621;
mod gelbooru;
mod moebooru;
mod realbooru;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use danbooru::Danbooru;
pub use moebooru::Moebooru;
pub use realbooru::Realbooru;
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
	/// The site's address, for cookies and for sorting out where a file came from.
	fn base(&self) -> &str;
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

/// Cookies harvested from the user's own browser, by host.
///
/// Some boorus answer a plain client with an interactive challenge. The app has
/// no browser, so it borrows the one that's already there: the visitor passes
/// it once in LibreWolf or Firefox and the clearance is copied out of that
/// profile's cookie database into the vault. Kept in memory here.
type Jar = std::sync::RwLock<std::collections::HashMap<String, (String, String)>>;
static JAR: std::sync::OnceLock<Jar> = std::sync::OnceLock::new();

fn jar() -> &'static Jar {
	JAR.get_or_init(Default::default)
}

pub fn set_cookie(host: &str, header: &str) {
	set_cookie_as(host, header, firefox_ua());
}

/// The UA a cookie was earned under has to go with it, or the site refuses it.
pub fn set_cookie_as(host: &str, header: &str, ua: &str) {
	let host = host.trim().trim_start_matches('.').to_lowercase();
	if host.is_empty() || header.trim().is_empty() {
		return;
	}
	jar().write().unwrap_or_else(|e| e.into_inner()).insert(host, (header.trim().to_string(), ua.trim().to_string()));
}

pub fn clear_cookies() {
	jar().write().unwrap_or_else(|e| e.into_inner()).clear();
}

pub fn cookie_hosts() -> Vec<String> {
	jar().read().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
}

/// The host of an https URL, without port, userinfo or path.
pub fn host_of(url: &str) -> Option<&str> {
	let rest = url.split_once("://")?.1;
	let host = rest.split(['/', '?', '#']).next()?;
	let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
	let host = host.split(':').next()?;
	(!host.is_empty()).then_some(host)
}

/// The longest stored cookie host that `url` belongs to (`bo.ru` or `a.bo.ru`).
pub fn cookie_for(url: &str) -> Option<(String, String)> {
	let host = host_of(url)?.to_lowercase();
	let jar = jar().read().unwrap_or_else(|e| e.into_inner());
	jar.iter()
		.filter(|(k, _)| host == **k || host.ends_with(&format!(".{k}")))
		.max_by_key(|(k, _)| k.len())
		.map(|(_, v)| v.clone())
}

/// The UA a clearance cookie was earned under. Cloudflare ties `cf_clearance`
/// to it, so a cookie sent with the app's own UA is refused.
/// The Firefox UA used when nothing better is known.
pub fn firefox_ua() -> &'static str {
	BROWSER_UA
}

pub const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Attaches the borrowed cookie, if there is one for this host, and the
/// matching UA. Sites without a cookie keep the app's own UA.
pub fn with_cookie(req: reqwest::blocking::RequestBuilder, url: &str) -> reqwest::blocking::RequestBuilder {
	match cookie_for(url) {
		Some((cookie, ua)) => req.header(reqwest::header::COOKIE, cookie).header(reqwest::header::USER_AGENT, ua),
		None => req,
	}
}

/// Most bytes one image or video download may bring in.
pub const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;

/// Downloads an image or video named in a site's answer: https only (no local
/// files, no plain-text links), and never more than `MAX_DOWNLOAD`, so a
/// broken or hostile server can't fill the memory.
pub fn download(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
	download_with(client, url, None)
}

/// `download`, with a `Referer` for sites whose image servers only serve their
/// own pages. Gelbooru answers anything without one with a redirect to its post
/// list, so the referer is required there, not a nicety.
pub fn download_with(client: &reqwest::blocking::Client, url: &str, referer: Option<&str>) -> Result<Vec<u8>> {
	if !url.starts_with("https://") {
		return Err(crate::Error::Query("only https links are downloaded".into()));
	}
	let mut req = with_cookie(client.get(url), url);
	if let Some(referer) = referer {
		req = req.header(reqwest::header::REFERER, referer);
	}
	let res = req.send()?.error_for_status()?;
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

	/// Live: Gelbooru's image servers are in place again: without the referer
	/// they answer with their post list, with it they send the file.
	/// `cargo test -p bettersearch-core -- --ignored` (needs the network).
	#[test]
	#[ignore]
	fn gelbooru_image_needs_the_referer() {
		let client = reqwest::blocking::Client::builder().user_agent(user_agent("")).build().unwrap();
		let url = "https://img4.gelbooru.com/thumbnails/d5/09/thumbnail_d509cffc5c68c66faffc09617d575749.jpg";
		let e = download(&client, url).unwrap_err();
		assert!(e.to_string().contains("web page instead of the image"), "{e}");

		let site = crate::sites::gelbooru::Gelbooru::gelbooru("test").unwrap();
		let bytes = site.fetch_bytes(url).unwrap();
		assert_eq!(&bytes[..3], b"\xff\xd8\xff", "not a JPEG");
	}
}
