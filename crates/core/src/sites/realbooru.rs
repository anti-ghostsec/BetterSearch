// Realbooru has no API and hasn't for years, but its list pages are ordinary
// HTML and give everything needed: the post id, the tags (in the thumbnail's
// `title`) and the md5 (in the thumbnail's file name). The full file lives at
// the same directory with the md5 as its name, so it can be reconstructed
// without opening each post.
//
// The extension isn't in the list, so `fetch_bytes` tries the plausible ones in
// turn, with the site as referer (its files 302 to a hotlink page otherwise).
// List pages also carry no rating, so one request is made per ticked rating and
// each result is labelled from the request that found it.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::blocking::Client;

use super::{Capabilities, Cursor, Page, Site, TagSuggestion, Throttle};
use crate::{Error, Post, Rating, Result};

const METAS: &[&str] = &["rating", "score", "id", "width", "height", "md5", "order", "source"];
/// Posts per list page, the site's own number.
const PER_PAGE: u32 = 42;
/// Extensions tried for a file, most common first.
const EXTS: &[&str] = &["jpeg", "jpg", "png", "gif", "webp", "webm", "mp4"];
/// Site rating names and what each counts as here.
const RATINGS: &[(&str, Rating)] = &[("safe", Rating::Safe), ("questionable", Rating::Questionable), ("explicit", Rating::Explicit)];

pub struct Realbooru {
	base: String,
	client: Client,
	throttle: Throttle,
	image_throttle: Throttle,
}

impl Realbooru {
	pub fn new(user_agent: &str) -> Result<Self> {
		let client = Client::builder().user_agent(user_agent).timeout(Duration::from_secs(30)).build()?;
		Ok(Self {
			base: "https://realbooru.com".into(),
			client,
			// Three requests per page, spaced a little.
			throttle: Throttle::new(Duration::from_millis(300)),
			image_throttle: Throttle::new(Duration::from_millis(150)),
		})
	}

	fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
		self.throttle.wait();
		let url = format!("{}{path}", self.base);
		let req = super::with_cookie(self.client.get(&url), &url).query(query);
		let res = req.send()?;
		if !res.status().is_success() {
			return Err(Error::Api { site: "realbooru".into(), message: res.status().to_string() });
		}
		Ok(res.text()?)
	}

	/// Our rating terms become the site's names; everything else is left alone.
	fn split_ratings(&self, query: &str) -> (String, Vec<(&'static str, Rating)>) {
		let mut keep = Vec::new();
		let mut wanted: Vec<(&'static str, Rating)> = Vec::new();
		for word in query.split_whitespace() {
			let Some(v) = word.strip_prefix("rating:") else {
				keep.push(word.to_string());
				continue;
			};
			let want = Rating::from_name(v);
			for (name, rating) in RATINGS {
				if want.is_some_and(|w| w == *rating) && !wanted.iter().any(|(n, _)| n == name) {
					wanted.push((name, *rating));
				}
			}
		}
		(keep.join(" "), wanted)
	}

	/// `<div class="col thumb" id="s123">` … `<img src="…/thumbnail_<md5>.jpg" title="tags" alt="Image: 123">`
	fn parse(&self, html: &str, rating: Rating) -> Vec<Post> {
		let mut out = Vec::new();
		for chunk in html.split("class=\"col thumb\"").skip(1) {
			let Some(id) = chunk.split("id=\"s").nth(1).and_then(|s| s.split('"').next()).and_then(|s| s.parse::<u64>().ok()) else {
				continue;
			};
			let Some(img) = chunk.split("<img ").nth(1) else { continue };
			let Some(src) = tag(img, "src") else { continue };
			let Some(md5) = md5_of(&src) else { continue };
			let tags: Vec<String> = tag(img, "title")
				.map(|t| t.split(", ").map(|w| w.trim().replace(' ', "_")).filter(|w| !w.is_empty()).collect())
				.unwrap_or_default();
			let mut tags: Vec<String> = tags.into_iter().map(unescape).collect();
			tags.sort();
			tags.dedup();
			let mut categories = std::collections::BTreeMap::new();
			if !tags.is_empty() {
				categories.insert("general".to_string(), tags.clone());
			}
			let dir = dir_of(&src).unwrap_or_default();
			// The site marks moving pictures with a tag; without it the viewer
			// would treat a video as a still.
			// `animated` is nearly always a GIF (plays on its own); `video`
			// alone is usually MP4. Guessing wrong only costs 404s, since the
			// fetch tries the others.
			let ext = if tags.iter().any(|t| t == "animated") {
				"gif"
			} else if tags.iter().any(|t| t == "video" || t == "webm" || t == "mp4") {
				"mp4"
			} else {
				// Most originals are .jpeg, not .jpg; the fetch tries this first
				// and the rest after it.
				"jpeg"
			};
			out.push(Post {
				site: "realbooru".into(),
				id,
				tags,
				categories,
				rating,
				score: 0,
				fav_count: 0,
				width: 0,
				height: 0,
				ext: ext.into(),
				md5: Some(md5.clone()),
				// Guessed extension; `fetch_bytes` corrects it by trying in turn.
				file_url: Some(format!("{0}//images/{dir}/{md5}.{ext}", self.base)),
				sample_url: None,
				preview_url: Some(src.clone()),
				page_url: format!("{}/index.php?page=post&s=view&id={id}", self.base),
				sources: Vec::new(),
				created_at: String::new(),
			});
		}
		out
	}
}

/// `name="…"` from inside one tag.
fn tag(img: &str, name: &str) -> Option<String> {
	let at = img.find(&format!("{name}=\""))? + name.len() + 2;
	let end = img[at..].find('"')?;
	Some(img[at..at + end].to_string())
}

/// `…/thumbnails/8c/3d/thumbnail_<md5>.jpg` → the md5.
fn md5_of(src: &str) -> Option<String> {
	let file = src.rsplit('/').next()?;
	let hash = file.strip_prefix("thumbnail_")?.split('.').next()?;
	(hash.len() >= 16 && hash.chars().all(|c| c.is_ascii_hexdigit())).then(|| hash.to_lowercase())
}

/// `…/thumbnails/8c/3d/…` → `8c/3d`.
fn dir_of(src: &str) -> Option<String> {
	let parts: Vec<&str> = src.rsplit('/').take(3).collect();
	(parts.len() == 3).then(|| format!("{}/{}", parts[2], parts[1]))
}

fn unescape(s: String) -> String {
	s.replace("&#039;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&amp;", "&")
}

impl Site for Realbooru {
	fn id(&self) -> &str {
		"realbooru"
	}

	fn name(&self) -> &str {
		"Realbooru"
	}

	fn base(&self) -> &str {
		&self.base
	}

	fn caps(&self) -> Capabilities {
		Capabilities { max_tags: 16, tilde_or: true, max_wildcards: 1, metas: METAS, free_metas: &[], page_size: PER_PAGE as usize }
	}

	/// Pages are always the site's 42 a rating, whatever the limit: the next
	/// page starts after all of them, so any cut here would never be seen.
	fn search(&self, query: &str, cursor: Option<Cursor>, _limit: usize) -> Result<Page> {
		let (tags, wanted) = self.split_ratings(query);
		let page = match cursor {
			Some(Cursor::Page(n)) => n.max(1),
			_ => 1,
		};
		let pid = (page - 1) * PER_PAGE;
		// No rating asked for: the site's own order is fine, and everything it
		// returns gets one label.
		let asked: Vec<(&str, Rating)> = if wanted.is_empty() { vec![("", Rating::Explicit)] } else { wanted };
		// One request per rating, together rather than in turn: the site is the
		// slow part and each answer is independent.
		let pages: Vec<Result<Vec<Post>>> = std::thread::scope(|scope| {
			let jobs: Vec<_> = asked
				.iter()
				.map(|(name, rating)| {
					let full = if name.is_empty() { tags.clone() } else { format!("{tags} rating:{name}").trim().to_string() };
					let rating = *rating;
					scope.spawn(move || {
						let mut q = vec![("page", "post".to_string()), ("s", "list".into()), ("pid", pid.to_string())];
						if !full.is_empty() {
							q.push(("tags", full));
						}
						self.get("/index.php", &q).map(|html| self.parse(&html, rating))
					})
				})
				.collect();
			jobs.into_iter().map(|j| j.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
		});
		let (mut posts, mut full, mut error) = (Vec::new(), false, None);
		for answer in pages {
			match answer {
				Ok(list) => {
					full |= list.len() >= PER_PAGE as usize;
					posts.extend(list);
				}
				Err(e) => error = Some(e),
			}
		}
		// A failure with nothing else to show is an error, not "no posts".
		if posts.is_empty()
			&& let Some(e) = error
		{
			return Err(e);
		}
		posts.sort_by_key(|p| std::cmp::Reverse(p.id));
		posts.dedup_by(|a, b| a.id == b.id);
		let next = full.then_some(Cursor::Page(page + 1));
		Ok(Page { posts, next })
	}

	/// No API for counts; the planner then leaves the tags as written.
	fn tag_counts(&self, _names: &[String]) -> Result<HashMap<String, u64>> {
		Ok(HashMap::new())
	}

	fn aliases(&self, _names: &[String]) -> Result<HashMap<String, String>> {
		Ok(HashMap::new())
	}

	fn autocomplete(&self, _prefix: &str) -> Result<Vec<TagSuggestion>> {
		Ok(Vec::new())
	}

	/// The extension isn't known, so try them until one answers with a file.
	fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
		self.image_throttle.wait();
		if !url.contains("/images/") {
			return super::download_with(&self.client, url, Some(&format!("{}/", self.base)));
		}
		let stem = url.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_else(|| url.to_string());
		// Whatever the post claimed is tried first; a video should not pay for
		// four 404s before its own extension.
		let claimed = url.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
		let mut order: Vec<&str> = EXTS.to_vec();
		order.sort_by_key(|e| *e != claimed);
		let mut last = None;
		for ext in order {
			match super::download_with(&self.client, &format!("{stem}.{ext}"), Some(&format!("{}/", self.base))) {
				Ok(bytes) => return Ok(bytes),
				Err(e) => last = Some(e),
			}
		}
		Err(last.unwrap_or_else(|| Error::Query("no file at that address".into())))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const PAGE: &str = r#"<div class="col thumb" id="s645546">
<a id="p645546" href="https://realbooru.com/index.php?page=post&amp;s=view&amp;id=645546">
<img src="https://realbooru.com/thumbnails/8c/3d/thumbnail_8c3da0a36bd70ba3c321ee374ce7e515.jpg" title="1girl, canine, female fox, krystal, star fox" alt="Image: 645546"  style=""/>
</a></div><div class="col thumb" id="s1"><img src="https://realbooru.com/thumbnails/aa/bb/thumbnail_aa11bb22cc33dd44ee55ff6677889900.png" title="beach, solo" alt="Image: 1"/></div>"#;

	#[test]
	fn reads_posts_out_of_a_list_page() {
		let site = Realbooru::new("test").unwrap();
		let posts = site.parse(PAGE, Rating::Safe);
		assert_eq!(posts.len(), 2);
		assert_eq!(posts[0].id, 645546);
		// Spaces in tags become underscores, as boorus write them.
		assert!(posts[0].tags.contains(&"female_fox".to_string()));
		assert_eq!(posts[0].rating, Rating::Safe);
		assert_eq!(posts[0].file_url.as_deref(), Some("https://realbooru.com//images/8c/3d/8c3da0a36bd70ba3c321ee374ce7e515.jpeg"));
		assert_eq!(posts[0].sample_url, None, "the preview is the thumbnail; the panel must not stand in for the picture");
		assert_eq!(posts[0].page_url, "https://realbooru.com/index.php?page=post&s=view&id=645546");
		assert_eq!(posts[1].file_url.as_deref(), Some("https://realbooru.com//images/aa/bb/aa11bb22cc33dd44ee55ff6677889900.jpeg"));
	}

	#[test]
	fn rating_terms_become_site_queries() {
		let site = Realbooru::new("test").unwrap();
		let (tags, wanted) = site.split_ratings("fox rating:s rating:q");
		assert_eq!(tags, "fox");
		assert_eq!(wanted, vec![("safe", Rating::Safe), ("questionable", Rating::Questionable)]);
		let (tags, wanted) = site.split_ratings("fox");
		assert_eq!(tags, "fox");
		assert!(wanted.is_empty());
	}

	/// Live, since most of this is parsing a real page.
	/// `cargo test -p bettersearch-core -- --ignored realbooru`
	#[test]
	#[ignore]
	fn live_search_and_fetch() {
		let site = Realbooru::new("test").unwrap();
		let page = site.search("1girl rating:s", None, 5).unwrap();
		assert!(!page.posts.is_empty(), "no posts");
		let post = &page.posts[0];
		assert!(post.tags.iter().any(|t| t == "1girl"));
		assert_eq!(post.rating, Rating::Safe);
		assert!(post.preview_url.as_deref().is_some_and(|u| u.starts_with("https://realbooru.com/thumbnails/")));
		let bytes = site.fetch_bytes(post.file_url.as_deref().unwrap()).unwrap();
		assert!(bytes.len() > 1_000, "{} bytes", bytes.len());
	}

	#[test]
	fn digs_the_hash_out_of_a_thumbnail() {
		assert_eq!(
			md5_of("https://realbooru.com/thumbnails/8c/3d/thumbnail_8C3DA0A36BD70BA3C321EE374CE7E515.jpg").as_deref(),
			Some("8c3da0a36bd70ba3c321ee374ce7e515")
		);
		assert_eq!(dir_of("https://realbooru.com/thumbnails/8c/3d/thumbnail_x.jpg").as_deref(), Some("8c/3d"));
		assert!(md5_of("https://x/thumbnails/8c/3d/other.jpg").is_none());
	}
}
