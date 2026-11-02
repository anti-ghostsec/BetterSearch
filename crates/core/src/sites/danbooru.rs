// Danbooru 2 (danbooru.donmai.us). API notes: https://danbooru.donmai.us/wiki_pages/help:api
//
// Searches without an account take only 2 tags (Gold 6, Platinum 12), but some
// metatags are free: rating, score, size, date and a few more. The limit comes
// from `/profile.json`, so a login raises it automatically.
//
// Ratings here are g(eneral), s(ensitive), q(uestionable), e(xplicit). Safe is
// general only; sensitive counts as Questionable.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;

use super::{Capabilities, Cursor, Page, Profile as TagProfile, Site, TagSuggestion, Throttle};
use crate::{Error, Post, Rating, Result};

const METAS: &[&str] = &[
	"rating", "score", "id", "favcount", "width", "height", "md5", "order", "status", "user", "fav", "pool", "source", "date",
];
/// Metatags that don't count toward the tag limit (checked against the site).
const FREE: &[&str] = &["rating", "score", "id", "favcount", "width", "height", "md5", "status", "date"];
/// The limit for searches without an account.
const ANONYMOUS_LIMIT: usize = 2;

pub struct Danbooru {
	id: String,
	name: String,
	base: String,
	client: Client,
	throttle: Throttle,
	cdn_throttle: Throttle,
	login: Option<(String, String)>,
	tag_limit: OnceLock<usize>,
}

impl Danbooru {
	#[allow(clippy::self_named_constructors)]
	pub fn danbooru(user_agent: &str) -> Result<Self> {
		Self::new("danbooru", "Danbooru", "https://danbooru.donmai.us", user_agent)
	}

	pub fn new(id: &str, name: &str, base: &str, user_agent: &str) -> Result<Self> {
		let client = Client::builder().user_agent(user_agent).timeout(Duration::from_secs(30)).build()?;
		Ok(Danbooru {
			id: id.into(),
			name: name.into(),
			base: base.trim_end_matches('/').into(),
			client,
			throttle: Throttle::new(Duration::from_millis(500)),
			cdn_throttle: Throttle::new(Duration::from_millis(100)),
			login: None,
			tag_limit: OnceLock::new(),
		})
	}

	/// The account name and an API key from the site's profile page.
	pub fn with_login(mut self, login: &str, api_key: &str) -> Self {
		let (login, api_key) = (login.trim(), api_key.trim());
		if !login.is_empty() && !api_key.is_empty() {
			self.login = Some((login.into(), api_key.into()));
		}
		self
	}

	fn get<T: for<'de> Deserialize<'de>>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
		let url = format!("{}{path}", self.base);
		let mut attempt = 0u32;
		loop {
			self.throttle.wait();
			let mut req = super::with_cookie(self.client.get(&url), &url).query(query);
			if let Some((u, k)) = &self.login {
				req = req.basic_auth(u, Some(k));
			}
			let res = req.send()?;
			let status = res.status();
			let asked = res
				.headers()
				.get(reqwest::header::RETRY_AFTER)
				.and_then(|v| v.to_str().ok())
				.and_then(|v| v.parse::<u64>().ok());
			let body = res.text().unwrap_or_default();
			if status.is_success() {
				return serde_json::from_str(&body).map_err(|e| Error::Api { site: self.name.clone(), message: e.to_string() });
			}
			let challenge = body.contains("Just a moment");
			// Cloudflare challenges and throttles come and go in seconds; three
			// tries with the site's own delay is worth it before failing.
			let retriable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
				|| status == reqwest::StatusCode::SERVICE_UNAVAILABLE
				|| (status == reqwest::StatusCode::FORBIDDEN && challenge);
			if retriable && attempt < 2 {
				attempt += 1;
				std::thread::sleep(Duration::from_secs(asked.unwrap_or(u64::from(challenge) * 2 + 2).min(15)));
				continue;
			}
			let message = serde_json::from_str::<ApiError>(&body).ok().and_then(|e| e.message).unwrap_or_else(|| {
				if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
					"rate-limited; give it a moment".into()
				} else if challenge {
					"blocked by the site's bot check".into()
				} else {
					status.to_string()
				}
			});
			return Err(Error::Api { site: self.name.clone(), message });
		}
	}

	/// The account's tag limit; asked once, 2 if the site doesn't say.
	fn tag_limit(&self) -> usize {
		*self.tag_limit.get_or_init(|| {
			if self.login.is_none() {
				return ANONYMOUS_LIMIT;
			}
			self.get::<Profile>("/profile.json", &[]).ok().and_then(|p| p.tag_query_limit).unwrap_or(ANONYMOUS_LIMIT)
		})
	}

	fn to_post(&self, p: RawPost) -> Post {
		let mut categories = BTreeMap::new();
		let mut tags = Vec::new();
		for (cat, list) in [
			("artist", &p.tag_string_artist),
			("copyright", &p.tag_string_copyright),
			("character", &p.tag_string_character),
			("general", &p.tag_string_general),
			("meta", &p.tag_string_meta),
		] {
			let list: Vec<String> = list.split_whitespace().map(String::from).collect();
			if list.is_empty() {
				continue;
			}
			tags.extend(list.iter().cloned());
			categories.insert(cat.to_string(), list);
		}
		if tags.is_empty() {
			tags = p.tag_string.split_whitespace().map(String::from).collect();
		}
		tags.sort();
		tags.dedup();
		Post {
			site: self.id.clone(),
			id: p.id,
			tags,
			categories,
			rating: post_rating(&p.rating),
			score: p.score,
			fav_count: p.fav_count,
			width: p.image_width,
			height: p.image_height,
			ext: p.file_ext,
			md5: p.md5,
			file_url: p.file_url,
			sample_url: p.large_file_url,
			preview_url: p.preview_file_url,
			page_url: format!("{}/posts/{}", self.base, p.id),
			sources: Some(p.source).filter(|s| !s.is_empty()).into_iter().collect(),
			created_at: p.created_at,
		}
	}
}

fn post_rating(letter: &str) -> Rating {
	match letter {
		"g" => Rating::Safe,
		"s" | "q" => Rating::Questionable,
		// "e", and anything unknown, stays out of Safe searches.
		_ => Rating::Explicit,
	}
}

/// The site's letters for one of our ratings.
fn site_letters(r: Rating) -> &'static str {
	match r {
		Rating::Safe => "g",
		Rating::Questionable => "s,q",
		Rating::Explicit => "e",
	}
}

/// Our rating letters into Danbooru's: `rating:s` → `rating:g`,
/// `-rating:q` → `-rating:s,q`. The site's own names (`rating:general`,
/// `rating:sensitive`) pass through.
fn site_query(query: &str) -> String {
	query
		.split_whitespace()
		.map(|w| {
			let (neg, rest) = w.strip_prefix('-').map_or((false, w), |r| (true, r));
			let Some(v) = rest.strip_prefix("rating:") else { return w.to_string() };
			if v.len() > 1 {
				return w.to_string();
			}
			match Rating::from_name(v) {
				Some(r) => format!("{}rating:{}", if neg { "-" } else { "" }, site_letters(r)),
				None => w.to_string(),
			}
		})
		.collect::<Vec<_>>()
		.join(" ")
}

fn category_name(c: u8) -> &'static str {
	match c {
		0 => "general",
		1 => "artist",
		3 => "copyright",
		4 => "character",
		5 => "meta",
		_ => "other",
	}
}

#[derive(Deserialize)]
struct ApiError {
	message: Option<String>,
}

#[derive(Deserialize)]
struct Profile {
	tag_query_limit: Option<usize>,
	/// `None` when the site doesn't know who's asking.
	id: Option<u64>,
	#[serde(default)]
	name: String,
}

#[derive(Deserialize)]
struct RawPost {
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
	/// Missing on posts only accounts can see.
	md5: Option<String>,
	file_url: Option<String>,
	large_file_url: Option<String>,
	preview_file_url: Option<String>,
	#[serde(default)]
	source: String,
	#[serde(default)]
	tag_string: String,
	#[serde(default)]
	tag_string_general: String,
	#[serde(default)]
	tag_string_artist: String,
	#[serde(default)]
	tag_string_character: String,
	#[serde(default)]
	tag_string_copyright: String,
	#[serde(default)]
	tag_string_meta: String,
}

#[derive(Deserialize)]
struct RawTag {
	name: String,
	post_count: u64,
}

#[derive(Deserialize)]
struct RawAlias {
	antecedent_name: String,
	consequent_name: String,
}

#[derive(Deserialize)]
struct RawRelated {
	#[serde(default)]
	post_count: u64,
	#[serde(default)]
	related_tags: Vec<RawRelatedTag>,
}

#[derive(Deserialize)]
struct RawRelatedTag {
	tag: RawTag,
	frequency: f64,
}

#[derive(Deserialize)]
struct RawCounts {
	counts: RawPostCount,
}

#[derive(Deserialize)]
struct RawPostCount {
	posts: u64,
}

#[derive(Deserialize)]
struct RawSuggestion {
	value: String,
	#[serde(default)]
	category: u8,
	#[serde(default)]
	post_count: u64,
	antecedent: Option<String>,
}

impl Site for Danbooru {
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
		Capabilities { max_tags: self.tag_limit(), tilde_or: true, max_wildcards: 1, metas: METAS, free_metas: FREE, page_size: 200 }
	}

	fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
		let limit = limit.clamp(1, 200);
		let mut q = vec![("tags", site_query(query)), ("limit", limit.to_string())];
		match cursor {
			Some(Cursor::Before(id)) => q.push(("page", format!("b{id}"))),
			Some(Cursor::Page(n)) => q.push(("page", n.to_string())),
			None => {}
		}
		let mut raw: Vec<RawPost> = self.get("/posts.json", &q)?;
		// A page of posts with no addresses at all means the response was cut
		// back (challenge or throttle). One retry usually gets the real thing.
		let bare = !raw.is_empty()
			&& raw.iter().all(|p| p.file_url.is_none() && p.large_file_url.is_none() && p.preview_file_url.is_none());
		if bare {
			std::thread::sleep(Duration::from_secs(3));
			if let Ok(again) = self.get::<Vec<RawPost>>("/posts.json", &q)
				&& again.iter().any(|p| p.file_url.is_some() || p.large_file_url.is_some() || p.preview_file_url.is_some())
			{
				raw = again;
			}
		}
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
		let names: Vec<&str> = names.iter().filter(|n| !n.contains('*')).map(|s| s.as_str()).collect();
		if names.is_empty() {
			return Ok(HashMap::new());
		}
		let tags: Vec<RawTag> = self.get(
			"/tags.json",
			&[("search[name_comma]", names.join(",")), ("only", "name,post_count".into()), ("limit", "200".into())],
		)?;
		Ok(tags.into_iter().map(|t| (t.name, t.post_count)).collect())
	}

	fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
		let names: Vec<&str> = names.iter().filter(|n| !n.contains('*')).map(|s| s.as_str()).collect();
		if names.is_empty() {
			return Ok(HashMap::new());
		}
		let list: Vec<RawAlias> = self.get(
			"/tag_aliases.json",
			&[
				("search[antecedent_name_comma]", names.join(",")),
				("search[status]", "active".into()),
				("only", "antecedent_name,consequent_name".into()),
				("limit", "200".into()),
			],
		)?;
		Ok(list.into_iter().map(|a| (a.antecedent_name, a.consequent_name)).collect())
	}

	fn autocomplete(&self, prefix: &str) -> Result<Vec<TagSuggestion>> {
		let prefix = prefix.trim().to_lowercase();
		if prefix.is_empty() {
			return Ok(Vec::new());
		}
		let list: Vec<RawSuggestion> = self.get(
			"/autocomplete.json",
			&[("search[query]", prefix), ("search[type]", "tag_query".into()), ("limit", "10".into())],
		)?;
		Ok(list
			.into_iter()
			.map(|s| TagSuggestion { name: s.value, category: category_name(s.category).into(), count: s.post_count, alias: s.antecedent })
			.collect())
	}

	/// Danbooru works this out over the whole site (from up to 1000 posts):
	/// one request.
	fn tag_profile(&self, tag: &str) -> Result<Option<TagProfile>> {
		let r: RawRelated = self.get(
			"/related_tag.json",
			&[("search[query]", tag.to_string()), ("search[category]", "general".into()), ("limit", "300".into())],
		)?;
		let sampled = r.post_count.clamp(1, 1000) as usize;
		let mut profile = TagProfile { sampled, ..TagProfile::default() };
		for t in r.related_tags {
			profile.counts.insert(t.tag.name.clone(), (t.frequency * sampled as f64).round() as u32);
			profile.sizes.insert(t.tag.name, t.tag.post_count);
		}
		Ok(Some(profile))
	}

	fn check_login(&self) -> Option<Result<String>> {
		self.login.as_ref()?;
		Some(self.get::<Profile>("/profile.json", &[]).and_then(|p| match p.id {
			Some(_) => Ok(p.name),
			None => Err(Error::Api { site: self.name.clone(), message: "the site doesn't know this login".into() }),
		}))
	}

	fn total_posts(&self) -> Result<Option<u64>> {
		let c: RawCounts = self.get("/counts/posts.json", &[])?;
		Ok(Some(c.counts.posts))
	}

	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
		self.cdn_throttle.wait();
		super::download(&self.client, url)
	}

	fn fetch_stream(&self, url: &str) -> Result<Box<dyn std::io::Read + Send>> {
		self.cdn_throttle.wait();
		super::stream_with(&self.client, url, None)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_a_post() {
		let json = r#"[{"id":12480318,"created_at":"2026-10-30T14:49:20.653-04:00","score":3,"source":"https://x.com/a",
			"md5":"aa89","rating":"s","image_width":2250,"image_height":3000,"tag_string":"fox_ears okukin_07 arknights",
			"fav_count":2,"file_ext":"jpg","tag_string_general":"fox_ears","tag_string_artist":"okukin_07",
			"tag_string_character":"","tag_string_copyright":"arknights","tag_string_meta":"",
			"file_url":"https://cdn.donmai.us/original/aa/89/aa89.jpg","large_file_url":"https://cdn.donmai.us/sample/aa/89/sample-aa89.jpg",
			"preview_file_url":"https://cdn.donmai.us/180x180/aa/89/aa89.jpg"},
			{"id":5,"rating":"g","tag_string":"b a"}]"#;
		let site = Danbooru::danbooru("test").unwrap();
		let mut posts = serde_json::from_str::<Vec<RawPost>>(json).unwrap().into_iter().map(|p| site.to_post(p));
		let p = posts.next().unwrap();
		assert_eq!(p.tags, vec!["arknights", "fox_ears", "okukin_07"]);
		assert_eq!(p.categories["artist"], vec!["okukin_07"]);
		assert!(!p.categories.contains_key("character"));
		assert_eq!(p.rating, Rating::Questionable);
		assert_eq!(p.sources, vec!["https://x.com/a"]);
		assert_eq!(p.page_url, "https://danbooru.donmai.us/posts/12480318");
		// A post without file fields (account-only) still reads, from tag_string.
		let p = posts.next().unwrap();
		assert_eq!((p.tags.clone(), p.rating, p.file_url), (vec!["a".to_string(), "b".into()], Rating::Safe, None));
	}

	#[test]
	fn rewrites_ratings() {
		assert_eq!(site_query("rating:s fox ~a ~b"), "rating:g fox ~a ~b");
		assert_eq!(site_query("-rating:q -rating:e"), "-rating:s,q -rating:e");
		assert_eq!(site_query("rating:general rating:sensitive"), "rating:general rating:sensitive");
	}
}
