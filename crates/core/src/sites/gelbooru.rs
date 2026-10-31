// Gelbooru 0.2 and the sites that run it: Safebooru (safebooru.org, no account
// needed) and gelbooru.com (an API key and user id from the account options
// page are required since its API went behind a login).
//
// The `dapi` answers differ between the two: Safebooru gives a bare JSON list
// (or an empty body), gelbooru.com wraps it in `{"post": [...]}`. Tag lookups
// only come as XML, one name per request. There's no alias endpoint.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;

use super::{Capabilities, Cursor, Page, Site, TagSuggestion, Throttle};
use crate::{Error, Post, Rating, Result};

/// Gelbooru-style sites spell `order:` as `sort:`.
const METAS: &[&str] = &["rating", "score", "id", "width", "height", "md5", "order", "user", "source"];
/// Tag counts cost one request each, so only this many are looked up.
const MAX_COUNTS: usize = 25;

/// Which autocomplete the site has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Suggest {
	/// `autocomplete.php?q=`, answers `{label: "name (count)", value}`.
	Labels,
	/// `index.php?page=autocomplete2`, answers `{value, post_count, category}`.
	Autocomplete2,
}

pub struct Gelbooru {
	id: String,
	name: String,
	base: String,
	client: Client,
	throttle: Throttle,
	image_throttle: Throttle,
	suggest: Suggest,
	/// OR is `{a ~ b}` on gelbooru.com but `( a ~ b )` on Safebooru, which
	/// finds nothing for the braces.
	braces: bool,
	/// The site's rating names and what each counts as here.
	ratings: &'static [(&'static str, Rating)],
	login: Option<(String, String)>,
}

impl Gelbooru {
	pub fn safebooru(user_agent: &str) -> Result<Self> {
		const RATINGS: &[(&str, Rating)] =
			&[("general", Rating::Safe), ("safe", Rating::Safe), ("questionable", Rating::Questionable)];
		Self::new("safebooru", "Safebooru", "https://safebooru.org", user_agent, Suggest::Labels, false, RATINGS)
	}

	/// Needs `with_login`; gelbooru.com answers 401 without it.
	#[allow(clippy::self_named_constructors)]
	pub fn gelbooru(user_agent: &str) -> Result<Self> {
		// "sensitive" sits between general and questionable; it counts as Questionable
		// so that Safe only ever means general.
		const RATINGS: &[(&str, Rating)] = &[
			("general", Rating::Safe),
			("sensitive", Rating::Questionable),
			("questionable", Rating::Questionable),
			("explicit", Rating::Explicit),
		];
		Self::new("gelbooru", "Gelbooru", "https://gelbooru.com", user_agent, Suggest::Autocomplete2, true, RATINGS)
	}

	/// Another Gelbooru 0.2 site: same API, its own name and address.
	pub fn custom(id: &str, name: &str, base: &str, user_agent: &str) -> Result<Self> {
		const RATINGS: &[(&str, Rating)] = &[
			("general", Rating::Safe),
			("sensitive", Rating::Questionable),
			("questionable", Rating::Questionable),
			("explicit", Rating::Explicit),
		];
		Self::new(id, name, base, user_agent, Suggest::Autocomplete2, true, RATINGS)
	}

	pub fn xbooru(user_agent: &str) -> Result<Self> {
		Self::custom("xbooru", "xbooru", "https://xbooru.com", user_agent)
	}

	pub fn tbib(user_agent: &str) -> Result<Self> {
		Self::custom("tbib", "The Big ImageBoard", "https://tbib.org", user_agent)
	}

	pub fn hypnohub(user_agent: &str) -> Result<Self> {
		Self::custom("hypnohub", "Hypnohub", "https://hypnohub.net", user_agent)
	}

	pub fn new(
		id: &str,
		name: &str,
		base: &str,
		user_agent: &str,
		suggest: Suggest,
		braces: bool,
		ratings: &'static [(&'static str, Rating)],
	) -> Result<Self> {
		let client = Client::builder().user_agent(user_agent).timeout(Duration::from_secs(30)).build()?;
		Ok(Gelbooru {
			id: id.into(),
			name: name.into(),
			base: base.trim_end_matches('/').into(),
			client,
			throttle: Throttle::new(Duration::from_millis(1000)),
			image_throttle: Throttle::new(Duration::from_millis(100)),
			suggest,
			braces,
			ratings,
			login: None,
		})
	}

	/// The user id and API key from the site's account options page.
	pub fn with_login(mut self, user_id: &str, api_key: &str) -> Self {
		let (user_id, api_key) = (user_id.trim(), api_key.trim());
		if !user_id.is_empty() && !api_key.is_empty() {
			self.login = Some((user_id.into(), api_key.into()));
		}
		self
	}

	fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
		self.throttle.wait();
		let url = format!("{}{path}", self.base);
		let mut req = super::with_cookie(self.client.get(url), &format!("{}{path}", self.base)).query(query);
		if let Some((user, key)) = &self.login {
			req = req.query(&[("user_id", user), ("api_key", key)]);
		}
		let res = req.send()?;
		let status = res.status();
		if status == reqwest::StatusCode::UNAUTHORIZED {
			let message = if self.login.is_some() {
				"the user id or API key was refused".into()
			} else {
				"this site needs your user id and API key; add them in Settings".into()
			};
			return Err(Error::Api { site: self.name.clone(), message });
		}
		if !status.is_success() {
			return Err(Error::Api { site: self.name.clone(), message: status.to_string() });
		}
		Ok(res.text()?)
	}

	fn dapi(&self, s: &str, extra: Vec<(&str, String)>) -> Result<String> {
		let mut q = vec![("page", "dapi".to_string()), ("s", s.into()), ("q", "index".into())];
		q.extend(extra);
		self.get("/index.php", &q)
	}

	fn rating(&self, name: &str) -> Rating {
		self.ratings
			.iter()
			.find(|(n, _)| *n == name)
			.map(|(_, r)| *r)
			.or_else(|| Rating::from_name(name))
			// Unknown ratings are kept out of Safe searches.
			.unwrap_or(Rating::Explicit)
	}

	/// Rewrites a planned query into this site's words: `~a ~b` → `( a ~ b )`,
	/// `order:` → `sort:`, and rating letters into the site's rating names.
	fn site_query(&self, query: &str) -> String {
		let mut out: Vec<String> = Vec::new();
		let mut tildes: Vec<&str> = Vec::new();
		let mut tilde_at = None;
		for word in query.split_whitespace() {
			if let Some(t) = word.strip_prefix('~') {
				tilde_at.get_or_insert(out.len());
				tildes.push(t);
			} else if let Some(v) = word.strip_prefix("order:") {
				let v = v.replace("_asc", ":asc").replace("_desc", ":desc");
				out.push(format!("sort:{}", if v == "favcount" { "score".into() } else { v }));
			} else if let Some((neg, v)) =
				word.strip_prefix("rating:").map(|v| (false, v)).or_else(|| word.strip_prefix("-rating:").map(|v| (true, v)))
			{
				out.extend(self.rating_terms(neg, v).unwrap_or_else(|| vec![word.to_string()]));
			} else {
				out.push(word.to_string());
			}
		}
		if let Some(at) = tilde_at {
			let group = match (tildes.len(), self.braces) {
				(1, _) => tildes[0].to_string(),
				(_, true) => format!("{{{}}}", tildes.join(" ~ ")),
				(_, false) => format!("( {} )", tildes.join(" ~ ")),
			};
			out.insert(at, group);
		}
		out.join(" ")
	}

	/// `rating:s` and `-rating:q` as the site's own names; `None` if `v` is the
	/// site's own name already (or not a rating at all).
	fn rating_terms(&self, neg: bool, v: &str) -> Option<Vec<String>> {
		if self.ratings.iter().any(|(n, _)| *n == v) {
			return None;
		}
		let want = Rating::from_name(v)?;
		let names = |keep: bool| self.ratings.iter().filter(move |(_, r)| (*r == want) == keep).map(|(n, _)| *n);
		Some(match (neg, names(true).count()) {
			(true, _) => names(true).map(|n| format!("-rating:{n}")).collect(),
			(false, 1) => names(true).map(|n| format!("rating:{n}")).collect(),
			// Several names (or none) for one rating: leave out all the others.
			(false, _) => names(false).map(|n| format!("-rating:{n}")).collect(),
		})
	}

	fn to_post(&self, p: RawPost) -> Post {
		let mut tags: Vec<String> = p.tags.split_whitespace().map(unescape).collect();
		tags.sort();
		tags.dedup();
		let mut categories = BTreeMap::new();
		if !tags.is_empty() {
			categories.insert("general".to_string(), tags.clone());
		}
		let file_url = non_empty(p.file_url);
		let ext = file_url
			.as_deref()
			.or(p.image.as_deref())
			.and_then(|u| u.rsplit_once('.'))
			.map(|(_, e)| e.to_lowercase())
			.unwrap_or_default();
		let has_sample = match &p.sample {
			Some(serde_json::Value::Bool(b)) => *b,
			Some(serde_json::Value::Number(n)) => n.as_i64() != Some(0),
			_ => true,
		};
		Post {
			site: self.id.clone(),
			id: p.id,
			tags,
			categories,
			rating: self.rating(&p.rating),
			score: p.score.unwrap_or(0),
			fav_count: 0,
			width: p.width,
			height: p.height,
			ext,
			md5: non_empty(p.md5).or(non_empty(p.hash)),
			file_url,
			sample_url: non_empty(p.sample_url).filter(|_| has_sample),
			preview_url: non_empty(p.preview_url),
			page_url: format!("{}/index.php?page=post&s=view&id={}", self.base, p.id),
			sources: non_empty(p.source).into_iter().collect(),
			created_at: p.created_at.unwrap_or_default(),
		}
	}
}

fn non_empty(s: Option<String>) -> Option<String> {
	s.filter(|s| !s.trim().is_empty())
}

/// The few HTML entities these sites put in tag names.
fn unescape(s: &str) -> String {
	if !s.contains('&') {
		return s.to_string();
	}
	s.replace("&#039;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Posts {
	List(Vec<RawPost>),
	Wrapped {
		#[serde(default)]
		post: Vec<RawPost>,
	},
}

#[derive(Deserialize)]
struct RawPost {
	id: u64,
	#[serde(default)]
	tags: String,
	#[serde(default)]
	rating: String,
	score: Option<i64>,
	#[serde(default)]
	width: u32,
	#[serde(default)]
	height: u32,
	md5: Option<String>,
	/// Safebooru's name for md5.
	hash: Option<String>,
	image: Option<String>,
	file_url: Option<String>,
	sample_url: Option<String>,
	/// `true`/`false` on Safebooru, `1`/`0` on gelbooru.com.
	sample: Option<serde_json::Value>,
	preview_url: Option<String>,
	source: Option<String>,
	created_at: Option<String>,
}

fn parse_posts(body: &str) -> Result<Vec<RawPost>> {
	let body = body.trim();
	if body.is_empty() {
		return Ok(Vec::new());
	}
	Ok(match serde_json::from_str::<Posts>(body).map_err(|e| Error::Api { site: "gelbooru".into(), message: e.to_string() })? {
		Posts::List(v) | Posts::Wrapped { post: v } => v,
	})
}

/// `name="..."` from one XML element.
fn attr<'a>(element: &'a str, name: &str) -> Option<&'a str> {
	let start = element.find(&format!(" {name}=\""))? + name.len() + 3;
	let len = element[start..].find('"')?;
	Some(&element[start..start + len])
}

/// The `<tag .../>` elements of a tag answer, as (name, count, type).
fn parse_tags(xml: &str) -> Vec<(String, u64, u8)> {
	xml.split("<tag ")
		.skip(1)
		.filter_map(|el| {
			let el = format!(" {}", &el[..el.find('>').unwrap_or(el.len())]);
			let name = unescape(attr(&el, "name")?);
			let count = attr(&el, "count")?.parse().ok()?;
			let kind = attr(&el, "type").and_then(|t| t.parse().ok()).unwrap_or(0);
			Some((name, count, kind))
		})
		.collect()
}

fn category_name(kind: &str) -> &'static str {
	match kind {
		"0" | "tag" | "general" => "general",
		"1" | "artist" => "artist",
		"3" | "copyright" => "copyright",
		"4" | "character" => "character",
		"5" | "metadata" | "meta" => "meta",
		_ => "other",
	}
}

#[derive(Deserialize)]
struct Labelled {
	label: String,
	value: String,
}

#[derive(Deserialize)]
struct Suggested {
	value: String,
	#[serde(default)]
	post_count: serde_json::Value,
	#[serde(default)]
	category: String,
}

impl Site for Gelbooru {
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
		// No tag limit showed up in testing; 20 keeps URLs and the server's work sane.
		Capabilities { max_tags: 20, tilde_or: true, max_wildcards: 2, metas: METAS, free_metas: &[], page_size: 100 }
	}

	fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
		let limit = limit.clamp(1, 100);
		let mut tags = self.site_query(query);
		let mut q = vec![("json", "1".to_string()), ("limit", limit.to_string())];
		match cursor {
			Some(Cursor::Before(id)) => tags = format!("{tags} id:<{id}").trim().to_string(),
			// Pages here count from 1; the site's `pid` counts from 0.
			Some(Cursor::Page(n)) => q.push(("pid", n.saturating_sub(1).to_string())),
			None => {}
		}
		q.push(("tags", tags));
		let raw = parse_posts(&self.dapi("post", q)?)?;
		let full = raw.len() >= limit;
		let posts: Vec<Post> = raw.into_iter().map(|p| self.to_post(p)).collect();
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
		let mut out = HashMap::new();
		for name in names.iter().filter(|n| !n.contains('*')).take(MAX_COUNTS) {
			let xml = self.dapi("tag", vec![("name", name.clone())])?;
			for (n, count, _) in parse_tags(&xml) {
				if n == *name {
					out.insert(n, count);
				}
			}
		}
		Ok(out)
	}

	fn aliases(&self, _names: &[String]) -> Result<HashMap<String, String>> {
		// Gelbooru 0.2 has no alias API.
		Ok(HashMap::new())
	}

	fn autocomplete(&self, prefix: &str) -> Result<Vec<TagSuggestion>> {
		let prefix = prefix.trim().to_lowercase();
		if prefix.is_empty() {
			return Ok(Vec::new());
		}
		let bad = |e: serde_json::Error| Error::Api { site: self.name.clone(), message: e.to_string() };
		match self.suggest {
			Suggest::Labels => {
				let list: Vec<Labelled> = serde_json::from_str(&self.get("/autocomplete.php", &[("q", prefix)])?).map_err(bad)?;
				Ok(list
					.into_iter()
					.map(|s| {
						let count = s.label.rsplit_once('(').and_then(|(_, c)| c.trim_end_matches(')').parse().ok()).unwrap_or(0);
						TagSuggestion { name: unescape(&s.value), category: "general".into(), count, alias: None }
					})
					.collect())
			}
			Suggest::Autocomplete2 => {
				let q = [
					("page", "autocomplete2".to_string()),
					("term", prefix),
					("type", "tag_query".into()),
					("limit", "10".into()),
				];
				let list: Vec<Suggested> = serde_json::from_str(&self.get("/index.php", &q)?).map_err(bad)?;
				Ok(list
					.into_iter()
					.map(|s| {
						let count = s.post_count.as_u64().or_else(|| s.post_count.as_str().and_then(|c| c.parse().ok())).unwrap_or(0);
						TagSuggestion { name: unescape(&s.value), category: category_name(&s.category).into(), count, alias: None }
					})
					.collect())
			}
		}
	}

	/// gelbooru.com refuses its API without a working login, so one small search tells.
	fn check_login(&self) -> Option<Result<String>> {
		let (user, _) = self.login.as_ref()?;
		Some(self.dapi("post", vec![("json", "1".into()), ("limit", "1".into())]).map(|_| format!("user {user}")))
	}

	/// gelbooru.com's image servers only serve their own pages; a `Referer` of
	/// the site fixes it, so images show like anywhere else.
	fn shows_images(&self) -> bool {
		true
	}

	/// Safebooru has no explicit posts.
	fn only_ratings(&self) -> Option<&'static [Rating]> {
		(self.id == "safebooru").then_some(&[Rating::Safe, Rating::Questionable][..])
	}

	/// Its image servers check the referer, so send it — but only to the site's
	/// own hosts, so nothing else learns where the file came from.
	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
		self.image_throttle.wait();
		let referer = format!("{}/", self.base);
		let own = url
			.split_once("//")
			.and_then(|(_, rest)| rest.split('/').next())
			.is_some_and(|host| host == "gelbooru.com" || host.ends_with(".gelbooru.com"));
		super::download_with(&self.client, url, own.then_some(referer.as_str()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_both_post_shapes() {
		let safebooru = r#"[{"preview_url":"https://safebooru.org/thumbnails/87/thumbnail_a.jpg",
			"sample_url":"https://safebooru.org/samples/87/sample_a.jpg","file_url":"https://safebooru.org/images/87/a.png",
			"directory":87,"hash":"4a47","width":2048,"height":1812,"id":7277481,"image":"a.png","rating":"general",
			"sample":false,"score":null,"tags":"1girl fox_ears  solo","source":""}]"#;
		let site = Gelbooru::safebooru("test").unwrap();
		let post = site.to_post(parse_posts(safebooru).unwrap().into_iter().next().unwrap());
		assert_eq!(post.tags, vec!["1girl", "fox_ears", "solo"]);
		assert_eq!(post.rating, Rating::Safe);
		assert_eq!((post.score, post.ext.as_str(), post.md5.as_deref()), (0, "png", Some("4a47")));
		assert_eq!(post.sample_url, None);
		assert!(post.sources.is_empty());
		assert_eq!(post.key(), "safebooru:post/7277481");

		let gelbooru = r#"{"@attributes":{"limit":1,"offset":0,"count":5},"post":[{"id":9,"tags":"a&#039;s b",
			"rating":"sensitive","score":4,"md5":"ff","file_url":"https://img.gelbooru.com/x.jpg","sample_url":"",
			"sample":0,"preview_url":"https://img.gelbooru.com/t.jpg","created_at":"Fri Oct 30 01:00:00 -0500 2026"}]}"#;
		let site = Gelbooru::gelbooru("test").unwrap();
		let post = site.to_post(parse_posts(gelbooru).unwrap().into_iter().next().unwrap());
		assert_eq!(post.tags, vec!["a's", "b"]);
		assert_eq!(post.rating, Rating::Questionable);
		assert_eq!(post.sample_url, None);

		assert!(parse_posts("").unwrap().is_empty());
		assert!(parse_posts(r#"{"@attributes":{"count":0}}"#).unwrap().is_empty());
	}

	#[test]
	fn rewrites_queries_for_the_site() {
		let sb = Gelbooru::safebooru("test").unwrap();
		assert_eq!(sb.site_query("rating:s fox ~smile ~grin -sketch"), "-rating:questionable fox ( smile ~ grin ) -sketch");
		assert_eq!(sb.site_query("-rating:q order:score_asc ~a"), "-rating:questionable sort:score:asc a");
		assert_eq!(sb.site_query("rating:general"), "rating:general");

		let gb = Gelbooru::gelbooru("test").unwrap();
		assert_eq!(gb.site_query("rating:s fox ~a ~b"), "rating:general fox {a ~ b}");
		assert_eq!(gb.site_query("rating:q"), "-rating:general -rating:explicit");
		assert_eq!(gb.site_query("-rating:q"), "-rating:sensitive -rating:questionable");
	}

	#[test]
	fn reads_tag_xml() {
		let xml = r#"<?xml version="1.0" encoding="UTF-8"?><tags type="array"><tag type="0" count="101123" name="fox_ears" ambiguous="false" id="269"/>
<tag type="4" count="2" name="setsuna(needless)" ambiguous="false" id="735"/></tags>"#;
		assert_eq!(parse_tags(xml), vec![("fox_ears".into(), 101123, 0), ("setsuna(needless)".into(), 2, 4)]);
	}
}
