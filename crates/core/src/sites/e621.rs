// e621 and its safe-only twin e926: a Danbooru fork with its own JSON.
// API notes: https://e621.net/help/api — a descriptive User-Agent is
// required and requests should stay under about one or two a second.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;

use std::collections::HashSet;
use std::sync::OnceLock;

use super::{Capabilities, Cursor, NOT_THINGS, Page, Profile, Site, TagSuggestion, Throttle};
use crate::{Error, Post, Rating, Result};

pub struct E621 {
	id: String,
	name: String,
	base: String,
	client: Client,
	throttle: Throttle,
	/// Images come from a CDN, which gets its own, looser pacing.
	cdn_throttle: Throttle,
	login: Option<(String, String)>,
	total: OnceLock<Option<u64>>,
	/// Tag profiles fetched this run: learning several subjects reuses the verb's.
	profiles: std::sync::Mutex<HashMap<String, Profile>>,
	/// e926 has Safe posts only.
	only: Option<&'static [Rating]>,
}

impl E621 {
	#[allow(clippy::self_named_constructors)]
	pub fn e621(user_agent: &str) -> Result<Self> {
		Self::new("e621", "e621", "https://e621.net", user_agent)
	}

	pub fn e926(user_agent: &str) -> Result<Self> {
		let mut site = Self::new("e926", "e926 (safe only)", "https://e926.net", user_agent)?;
		site.only = Some(&[Rating::Safe]);
		Ok(site)
	}

	/// An e621 fork for AI-made art. Same API, own login.
	pub fn e6ai(user_agent: &str) -> Result<Self> {
		Self::new("e6ai", "e6ai (AI art)", "https://e6ai.net", user_agent)
	}

	pub fn new(id: &str, name: &str, base: &str, user_agent: &str) -> Result<Self> {
		let client = Client::builder().user_agent(user_agent).timeout(Duration::from_secs(30)).build()?;
		Ok(E621 {
			id: id.into(),
			name: name.into(),
			base: base.trim_end_matches('/').into(),
			client,
			throttle: Throttle::new(Duration::from_millis(1000)),
			cdn_throttle: Throttle::new(Duration::from_millis(100)),
			login: None,
			total: OnceLock::new(),
			profiles: std::sync::Mutex::new(HashMap::new()),
			only: None,
		})
	}

	/// Uses an account's API key (from the site's account settings) for every request.
	pub fn with_login(mut self, username: &str, api_key: &str) -> Self {
		// Pasted keys often bring a space or line break along, which the site refuses.
		let (username, api_key) = (username.trim(), api_key.trim());
		if !username.is_empty() && !api_key.is_empty() {
			self.login = Some((username.into(), api_key.into()));
		}
		self
	}

	fn get<T: for<'de> Deserialize<'de>>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
		// A busy site answers 503 ("temporarily unavailable"): wait and try again, a little longer each time.
		let url = format!("{}{path}", self.base);
		let mut tries = 0;
		// Set when a challenge page came back with a cookie: the cookie may have
		// been earned under the other browser's UA, so it gets one try with it.
		let mut other_ua: Option<String> = None;
		let mut swapped = false;
		let res = loop {
			self.throttle.wait();
			let mut req = super::with_cookie(self.client.get(&url), &url).query(query);
			if let Some(ua) = &other_ua {
				req = req.header(reqwest::header::COOKIE, super::cookie_for(&url).map(|c| c.0).unwrap_or_default());
				req = req.header(reqwest::header::USER_AGENT, ua.as_str());
			}
			if let Some((u, k)) = &self.login {
				req = req.basic_auth(u, Some(k));
			}
			let res = req.send()?;
			if res.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE && tries < 3 {
				tries += 1;
				std::thread::sleep(Duration::from_secs(2 * tries));
				continue;
			}
			// A challenge page, and a cookie to try the other way round.
			if res.status().is_success() && !swapped {
				let html = res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("html"));
				if html && let Some((_, ua)) = super::other_cookie_ua(&url) {
					swapped = true;
					other_ua = Some(ua);
					continue;
				}
			}
			break res;
		};
		let status = res.status();
		if !status.is_success() {
			let body = res.text().unwrap_or_default();
			let message = serde_json::from_str::<ApiError>(&body)
				.ok()
				.and_then(|e| e.message.or(e.reason))
				.unwrap_or_else(|| status.to_string());
			return Err(Error::Api { site: self.name.clone(), message });
		}
		// A challenge page is HTML, which otherwise fails as a decode error.
		let body = res.text()?;
		if body.trim_start().starts_with('<') {
			return Err(Error::Api {
				site: self.name.clone(),
				message: "blocked by the site's bot check; a cookie from a browser gets past it (Settings → Blocked sites)".into(),
			});
		}
		serde_json::from_str(&body).map_err(|e| Error::Api { site: self.name.clone(), message: e.to_string() })
	}

	fn iqdb_post(&self, p: RawIqdbPost) -> Post {
		let mut tags: Vec<String> = p.tag_string.split_whitespace().map(String::from).collect();
		tags.sort();
		tags.dedup();
		Post {
			site: self.id.clone(),
			id: p.id,
			categories: BTreeMap::from([("general".to_string(), tags.clone())]),
			tags,
			rating: Rating::from_name(&p.rating).unwrap_or(Rating::Explicit),
			score: p.score,
			fav_count: p.fav_count,
			width: p.image_width,
			height: p.image_height,
			ext: p.file_ext,
			md5: p.md5,
			file_url: p.file_url,
			sample_url: p.sample_url,
			preview_url: p.preview_file_url,
			page_url: format!("{}/posts/{}", self.base, p.id),
			sources: Some(p.source).filter(|s| !s.is_empty()).into_iter().collect(),
			created_at: p.created_at,
		}
	}

	fn to_post(&self, p: RawPost) -> Post {
		let mut categories = BTreeMap::new();
		let mut tags = Vec::new();
		for (cat, list) in p.tags {
			if list.is_empty() {
				continue;
			}
			tags.extend(list.iter().cloned());
			categories.insert(cat, list);
		}
		tags.sort();
		tags.dedup();
		Post {
			site: self.id.clone(),
			id: p.id,
			tags,
			categories,
			rating: Rating::from_name(&p.rating).unwrap_or(Rating::Explicit),
			score: p.score.total,
			fav_count: p.fav_count,
			width: p.file.width,
			height: p.file.height,
			ext: p.file.ext.unwrap_or_default(),
			md5: p.file.md5,
			file_url: p.file.url,
			sample_url: p.sample.and_then(|s| s.url),
			preview_url: p.preview.and_then(|s| s.url),
			page_url: format!("{}/posts/{}", self.base, p.id),
			sources: p.sources,
			created_at: p.created_at,
		}
	}
}

#[derive(Deserialize)]
struct ApiError {
	message: Option<String>,
	reason: Option<String>,
}

#[derive(Deserialize)]
struct RawMatch {
	#[serde(default)]
	score: f64,
	post: Option<RawMatchPost>,
}

#[derive(Deserialize)]
struct RawMatchPost {
	posts: RawIqdbPost,
}

/// Posts in picture-search answers come in Danbooru's shape, not e621's usual one.
#[derive(Deserialize)]
struct RawIqdbPost {
	id: u64,
	#[serde(default)]
	created_at: String,
	#[serde(default)]
	score: i64,
	#[serde(default)]
	fav_count: u64,
	#[serde(default)]
	rating: String,
	#[serde(default)]
	image_width: u32,
	#[serde(default)]
	image_height: u32,
	#[serde(default)]
	file_ext: String,
	md5: Option<String>,
	#[serde(default)]
	tag_string: String,
	file_url: Option<String>,
	sample_url: Option<String>,
	preview_file_url: Option<String>,
	#[serde(default)]
	source: String,
}

#[derive(Deserialize)]
struct Posts {
	posts: Vec<RawPost>,
}

#[derive(Deserialize)]
struct RawPost {
	id: u64,
	#[serde(default)]
	created_at: String,
	file: RawFile,
	preview: Option<RawUrl>,
	sample: Option<RawUrl>,
	score: RawScore,
	tags: BTreeMap<String, Vec<String>>,
	rating: String,
	#[serde(default)]
	fav_count: u64,
	#[serde(default)]
	sources: Vec<String>,
}

#[derive(Deserialize)]
struct RawFile {
	#[serde(default)]
	width: u32,
	#[serde(default)]
	height: u32,
	ext: Option<String>,
	md5: Option<String>,
	url: Option<String>,
}

#[derive(Deserialize)]
struct RawUrl {
	url: Option<String>,
}

#[derive(Deserialize)]
struct RawScore {
	total: i64,
}

#[derive(Deserialize)]
struct RawTag {
	name: String,
	post_count: u64,
	category: u8,
	antecedent_name: Option<String>,
}

#[derive(Deserialize)]
struct RawAlias {
	antecedent_name: String,
	consequent_name: String,
}

/// The tags endpoint answers `[]` when there are none, or `{"tags": []}` on some versions.
#[derive(Deserialize)]
#[serde(untagged)]
enum ListOr<T> {
	List(Vec<T>),
	Empty(#[allow(dead_code)] HashMap<String, serde_json::Value>),
}

impl<T> ListOr<T> {
	fn into_vec(self) -> Vec<T> {
		match self {
			ListOr::List(v) => v,
			ListOr::Empty(_) => Vec::new(),
		}
	}
}

fn category_name(c: u8) -> &'static str {
	match c {
		0 => "general",
		1 => "artist",
		3 => "copyright",
		4 => "character",
		5 => "species",
		6 => "invalid",
		7 => "meta",
		8 => "lore",
		_ => "other",
	}
}

impl Site for E621 {
	fn id(&self) -> &str {
		&self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn base(&self) -> &str {
		&self.base
	}

	fn caps(&self) -> Capabilities {
		Capabilities { max_tags: 40, tilde_or: true, max_wildcards: 1, metas: crate::query::META_KEYS, free_metas: &[], page_size: 320 }
	}

	fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
		let limit = limit.clamp(1, 320);
		let mut q = vec![("tags", query.to_string()), ("limit", limit.to_string())];
		match cursor {
			Some(Cursor::Before(id)) => q.push(("page", format!("b{id}"))),
			Some(Cursor::Page(n)) => q.push(("page", n.to_string())),
			None => {}
		}
		let raw: Posts = self.get("/posts.json", &q)?;
		let full = raw.posts.len() >= limit;
		let posts: Vec<Post> = raw.posts.into_iter().map(|p| self.to_post(p)).collect();
		let ordered = query.split_whitespace().any(|w| w.starts_with("order:"));
		let next = match (full, ordered, cursor) {
			(false, _, _) => None,
			(true, false, _) => posts.iter().map(|p| p.id).min().map(Cursor::Before),
			(true, true, Some(Cursor::Page(n))) => Some(Cursor::Page(n + 1)),
			(true, true, _) => Some(Cursor::Page(2)),
		};
		Ok(Page { posts, next })
	}

	fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
		let names: Vec<&String> = names.iter().filter(|n| !n.contains('*')).collect();
		if names.is_empty() {
			return Ok(HashMap::new());
		}
		let joined = names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
		let tags: ListOr<RawTag> = self.get("/tags.json", &[("search[name]", joined), ("limit", "320".into())])?;
		Ok(tags.into_vec().into_iter().map(|t| (t.name, t.post_count)).collect())
	}

	fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
		let names: Vec<&String> = names.iter().filter(|n| !n.contains('*')).collect();
		if names.is_empty() {
			return Ok(HashMap::new());
		}
		let joined = names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
		let list: ListOr<RawAlias> = self.get(
			"/tag_aliases.json",
			&[("search[antecedent_name]", joined), ("search[status]", "active".into()), ("limit", "320".into())],
		)?;
		Ok(list.into_vec().into_iter().map(|a| (a.antecedent_name, a.consequent_name)).collect())
	}

	fn autocomplete(&self, prefix: &str) -> Result<Vec<TagSuggestion>> {
		let prefix = prefix.trim().to_lowercase();
		if prefix.chars().count() < 3 {
			// The site's autocomplete wants three characters; fall back to a name search.
			if prefix.is_empty() {
				return Ok(Vec::new());
			}
			let tags: ListOr<RawTag> = self.get(
				"/tags.json",
				&[("search[name_matches]", format!("{prefix}*")), ("search[order]", "count".into()), ("limit", "10".into())],
			)?;
			return Ok(tags.into_vec().into_iter().map(suggestion).collect());
		}
		let tags: ListOr<RawTag> = self.get("/tags/autocomplete.json", &[("search[name_matches]", prefix), ("expiry", "7".into())])?;
		Ok(tags.into_vec().into_iter().map(suggestion).collect())
	}

	/// Two random pages of the tag's posts (random, so one comic's pages can't
	/// flood the sample), and site-wide counts for the tags seen at least 3 times.
	fn tag_profile(&self, tag: &str) -> Result<Option<Profile>> {
		if let Some(p) = self.profiles.lock().unwrap().get(tag) {
			return Ok(Some(p.clone()));
		}
		let mut seen = HashSet::new();
		let mut profile = Profile::default();
		for _ in 0..2 {
			let raw: Posts = self.get("/posts.json", &[("tags", format!("{tag} order:random")), ("limit", "320".into())])?;
			for p in raw.posts {
				if !seen.insert(p.id) {
					continue;
				}
				profile.sampled += 1;
				for (cat, list) in &p.tags {
					if NOT_THINGS.contains(&cat.as_str()) {
						continue;
					}
					for t in list {
						*profile.counts.entry(t.clone()).or_default() += 1;
					}
				}
			}
		}
		let names: Vec<String> = profile.counts.iter().filter(|(_, c)| **c >= 3).map(|(t, _)| t.clone()).collect();
		for chunk in names.chunks(100) {
			profile.sizes.extend(self.tag_counts(chunk)?);
		}
		self.profiles.lock().unwrap().insert(tag.to_string(), profile.clone());
		Ok(Some(profile))
	}

	/// Estimated from a random sample: the most common tag's share of it, against
	/// that tag's post count.
	/// e621's IQDB picture search, by post id (open without a login).
	fn other_versions(&self, post_id: u64) -> Result<Option<Vec<Post>>> {
		let found: Vec<RawMatch> = self.get("/iqdb_queries.json", &[("search[post_id]", post_id.to_string())])?;
		let mut matches: Vec<(f64, Post)> = found
			.into_iter()
			.filter_map(|m| m.post.map(|w| (m.score, self.iqdb_post(w.posts))))
			.filter(|(_, p)| p.id != post_id)
			.collect();
		matches.sort_by(|a, b| b.0.total_cmp(&a.0));
		Ok(Some(matches.into_iter().map(|(_, p)| p).collect()))
	}

	fn only_ratings(&self) -> Option<&'static [Rating]> {
		self.only
	}

	/// Favorites need a login, so they tell a working one from a refused one.
	fn check_login(&self) -> Option<Result<String>> {
		let (user, _) = self.login.as_ref()?;
		Some(self.get::<serde_json::Value>("/favorites.json", &[("limit", "1".into())]).map(|_| user.clone()))
	}

	fn total_posts(&self) -> Result<Option<u64>> {
		if let Some(t) = self.total.get() {
			return Ok(*t);
		}
		let total = self.tag_profile("")?.and_then(|p| {
			let (tag, count) = p.counts.iter().filter(|(t, _)| p.sizes.contains_key(*t)).max_by_key(|(_, c)| **c)?;
			Some((p.sizes[tag] as f64 * p.sampled as f64 / *count as f64) as u64)
		});
		let _ = self.total.set(total);
		Ok(total)
	}

	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
		self.cdn_throttle.wait();
		super::download(&self.client, url)
	}
}

fn suggestion(t: RawTag) -> TagSuggestion {
	TagSuggestion { category: category_name(t.category).into(), count: t.post_count, alias: t.antecedent_name, name: t.name }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_a_post() {
		let json = r#"{"posts":[{"id":42,"created_at":"2020-01-01T00:00:00.000-05:00",
			"file":{"width":800,"height":600,"ext":"png","size":1,"md5":"abc","url":"https://static1.e621.net/data/ab/c.png"},
			"preview":{"width":150,"height":112,"url":"https://static1.e621.net/data/preview/ab/c.jpg"},
			"sample":{"has":false,"url":null},
			"score":{"up":10,"down":-2,"total":8},
			"tags":{"general":["solo","smile"],"species":["fox"],"character":[],"artist":["someone"]},
			"rating":"s","fav_count":3,"sources":[]}]}"#;
		let site = E621::e926("test").unwrap();
		let raw: Posts = serde_json::from_str(json).unwrap();
		let post = site.to_post(raw.posts.into_iter().next().unwrap());
		assert_eq!(post.tags, vec!["fox", "smile", "solo", "someone"]);
		assert!(!post.categories.contains_key("character"));
		assert_eq!(post.rating, Rating::Safe);
		assert_eq!(post.score, 8);
		assert_eq!(post.key(), "e926:post/42");
		assert_eq!(post.page_url, "https://e926.net/posts/42");
		assert_eq!(post.sample_url, None);
	}

	#[test]
	fn reads_empty_tag_answers() {
		let a: ListOr<RawTag> = serde_json::from_str(r#"{"tags":[]}"#).unwrap();
		assert!(a.into_vec().is_empty());
		let b: ListOr<RawTag> = serde_json::from_str(r#"[{"name":"fox","post_count":5,"category":5}]"#).unwrap();
		assert_eq!(b.into_vec()[0].name, "fox");
	}
}
