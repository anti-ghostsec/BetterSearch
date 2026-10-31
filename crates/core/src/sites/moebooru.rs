// Moebooru-style boorus: yande.re, konachan.com, sakugabooru.com, and any
// other site running the same software. One `post.json` endpoint takes tags,
// a page and a limit; there is no alias or count mess to speak of.
//
// Ratings are the site's own `s`/`q`/`e`. OR is not supported by the software,
// so the planner is told to split them into separate searches.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;

use super::{Capabilities, Cursor, Page, Site, TagSuggestion, Throttle};
use crate::{Error, Post, Rating, Result};

const METAS: &[&str] = &["rating", "score", "id", "width", "height", "md5", "order", "date"];

pub struct Moebooru {
	id: String,
	name: String,
	base: String,
	client: Client,
	throttle: Throttle,
	image_throttle: Throttle,
}

impl Moebooru {
	pub fn yande_re(user_agent: &str) -> Result<Self> {
		Self::new("yandere", "yande.re", "https://yande.re", user_agent)
	}

	pub fn konachan(user_agent: &str) -> Result<Self> {
		Self::new("konachan", "Konachan", "https://konachan.com", user_agent)
	}

	pub fn konachan_net(user_agent: &str) -> Result<Self> {
		Self::new("konachan_net", "Konachan.net (safe)", "https://konachan.net", user_agent)
	}

	pub fn sakugabooru(user_agent: &str) -> Result<Self> {
		Self::new("sakugabooru", "Sakugabooru", "https://www.sakugabooru.com", user_agent)
	}

	pub fn new(id: &str, name: &str, base: &str, user_agent: &str) -> Result<Self> {
		let client = Client::builder().user_agent(user_agent).timeout(Duration::from_secs(30)).build()?;
		Ok(Self {
			id: id.into(),
			name: name.into(),
			base: base.trim_end_matches('/').into(),
			client,
			throttle: Throttle::new(Duration::from_millis(1000)),
			image_throttle: Throttle::new(Duration::from_millis(100)),
		})
	}

	fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
		self.throttle.wait();
		let res = self.client.get(format!("{}{path}", self.base)).query(query).send()?;
		if !res.status().is_success() {
			return Err(Error::Api { site: self.name.clone(), message: res.status().to_string() });
		}
		Ok(res.text()?)
	}

	/// `order:x` stays as the site spells it; everything else is plain.
	fn site_query(&self, query: &str) -> String {
		query.split_whitespace().collect::<Vec<_>>().join(" ")
	}

	fn rating(&self, name: &str) -> Rating {
		Rating::from_name(name).unwrap_or(Rating::Explicit)
	}

	fn to_post(&self, p: RawPost) -> Post {
		let mut tags: Vec<String> = p.tags.split_whitespace().map(str::to_string).collect();
		tags.sort();
		tags.dedup();
		let mut categories = std::collections::BTreeMap::new();
		if !tags.is_empty() {
			categories.insert("general".to_string(), tags.clone());
		}
		let file_url = non_empty(p.file_url);
		let ext = file_url
			.as_deref()
			.or(p.sample_url.as_deref())
			.and_then(|u| u.rsplit_once('.'))
			.map(|(_, e)| e.to_lowercase())
			.unwrap_or_default();
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
			md5: non_empty(p.md5),
			file_url,
			sample_url: non_empty(p.sample_url),
			preview_url: non_empty(p.preview_url),
			page_url: format!("{}/post/show/{}", self.base, p.id),
			sources: non_empty(p.source).into_iter().collect(),
			created_at: p.created_at.unwrap_or_default(),
		}
	}
}

fn non_empty(s: Option<String>) -> Option<String> {
	s.filter(|s| !s.trim().is_empty())
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
	file_url: Option<String>,
	sample_url: Option<String>,
	preview_url: Option<String>,
	source: Option<String>,
	created_at: Option<String>,
}

#[derive(Deserialize)]
struct RawTag {
	name: String,
	#[serde(default)]
	count: u64,
	#[serde(default)]
	#[serde(rename = "type")]
	kind: u8,
}

#[derive(Deserialize)]
struct RawAlias {
	antecedent_name: String,
	consequent_name: String,
}

fn category(kind: u8) -> &'static str {
	match kind {
		1 => "artist",
		3 => "copyright",
		4 => "character",
		5 | 6 => "meta",
		_ => "general",
	}
}

impl Site for Moebooru {
	fn id(&self) -> &str {
		&self.id
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn caps(&self) -> Capabilities {
		Capabilities { max_tags: 20, tilde_or: false, max_wildcards: 1, metas: METAS, free_metas: &[], page_size: 100 }
	}

	fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
		let limit = limit.clamp(1, 100);
		let tags = self.site_query(query);
		let mut q = vec![("limit", limit.to_string())];
		let mut tags = tags;
		match cursor {
			Some(Cursor::Before(id)) => tags = format!("{tags} id:<{id}").trim().to_string(),
			Some(Cursor::Page(n)) => q.push(("page", n.max(1).to_string())),
			None => {}
		}
		if !tags.is_empty() {
			q.push(("tags", tags));
		}
		let raw: Vec<RawPost> = serde_json::from_str(&self.get("/post.json", &q)?)
			.map_err(|e| Error::Api { site: self.name.clone(), message: e.to_string() })?;
		let full = raw.len() >= limit;
		let posts: Vec<Post> = raw.into_iter().map(|p| self.to_post(p)).collect();
		let next = match (full, cursor) {
			(false, _) => None,
			(true, Some(Cursor::Page(n))) => Some(Cursor::Page(n + 1)),
			(true, _) => Some(Cursor::Page(2)),
		};
		Ok(Page { posts, next })
	}

	fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
		let mut out = HashMap::new();
		for name in names.iter().filter(|n| !n.contains('*')).take(25) {
			let list: Vec<RawTag> = serde_json::from_str(&self.get("/tag.json", &[("name", name.clone())])?).unwrap_or_default();
			for t in list {
				if t.name == *name {
					out.insert(t.name, t.count);
				}
			}
		}
		Ok(out)
	}

	fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
		let mut out = HashMap::new();
		for name in names.iter().take(25) {
			let list: Vec<RawAlias> =
				serde_json::from_str(&self.get("/tag_alias.json", &[("name", name.clone())])?).unwrap_or_default();
			for a in list {
				if a.antecedent_name == *name && a.consequent_name != *name {
					out.insert(a.antecedent_name, a.consequent_name);
				}
			}
		}
		Ok(out)
	}

	fn autocomplete(&self, prefix: &str) -> Result<Vec<TagSuggestion>> {
		let prefix = prefix.trim().to_lowercase();
		if prefix.is_empty() {
			return Ok(Vec::new());
		}
		let list: Vec<RawTag> =
			serde_json::from_str(&self.get("/tag.json", &[("name", format!("{prefix}*")), ("limit", "10".into())])?)
				.unwrap_or_default();
		Ok(list
			.into_iter()
			.map(|t| TagSuggestion { name: t.name, category: category(t.kind).into(), count: t.count, alias: None })
			.collect())
	}

	fn only_ratings(&self) -> Option<&'static [Rating]> {
		(self.id == "konachan_net").then_some(&[Rating::Safe, Rating::Questionable][..])
	}

	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
		self.image_throttle.wait();
		super::download(&self.client, url)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_posts_and_keeps_the_rating_letters() {
		let site = Moebooru::konachan("test").unwrap();
		let json = r#"[{"id":410288,"tags":"tagme animal_ears","rating":"s","score":3,
			"width":1920,"height":1080,"md5":"abc","file_url":"https://k-cdn.yande.re/a.png",
			"sample_url":"https://k-cdn.yande.re/s.jpg","preview_url":"https://k-cdn.yande.re/t.jpg",
			"source":"","created_at":"1793308007"}]"#;
		let raw: Vec<RawPost> = serde_json::from_str(json).unwrap();
		let post = site.to_post(raw.into_iter().next().unwrap());
		assert_eq!(post.tags, vec!["animal_ears", "tagme"]);
		assert_eq!(post.rating, Rating::Safe);
		assert_eq!(post.ext, "png");
		assert_eq!(post.page_url, "https://konachan.com/post/show/410288");
		assert!(post.sources.is_empty());
	}

	#[test]
	fn keeps_order_metas_and_the_rest() {
		let site = Moebooru::yande_re("test").unwrap();
		assert_eq!(site.site_query("fox  -sketch order:score"), "fox -sketch order:score");
	}

	#[test]
	fn maps_tag_types() {
		assert_eq!(category(4), "character");
		assert_eq!(category(0), "general");
	}
}
