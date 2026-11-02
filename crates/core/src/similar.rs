// "Find similar" for a post. First the same artist drawing the same
// characters; when nothing else has those, the same artist with any one of
// them; then posts sharing its two most telling tags, then its most telling
// one. Each search asks the site only for posts that have everything it names,
// so what comes back is a short list of near matches, not a broad net with a
// few good ones in it. A tag is telling when it's on few posts (a specific
// object or species) and weak when it's everywhere (solo, smile).

use std::collections::HashMap;

use crate::search::rating_filter;
use crate::sites::{NOT_THINGS, Site};
use crate::{Post, Rating, Result};

/// Pages of results asked for at each step.
const PAGES: usize = 2;
/// A tag searched for alone or in a pair has to be on this many posts, so the
/// search can find more than the post itself.
const MIN_POSTS: u64 = 50;

/// Posts like one post, and how they were found.
pub struct Found {
	/// The query that found them.
	pub query: String,
	/// What they share with the post, for people: "the same artist and characters".
	pub by: String,
	/// The tags searched for.
	pub tags: Vec<String>,
	pub posts: Vec<Post>,
}

/// The post's tags that say what's in it: not the artist, series, characters or meta tags.
fn content_tags(post: &Post) -> Vec<String> {
	if post.categories.is_empty() {
		return post.tags.clone();
	}
	post.categories.iter().filter(|(c, _)| !NOT_THINGS.contains(&c.as_str())).flat_map(|(_, t)| t.iter().cloned()).collect()
}

/// Artist tags that name an artist: e621 files unknowns and warnings
/// (conditional_dnp, sound_warning) under artist too.
fn is_artist(tag: &str) -> bool {
	!(tag.contains("unknown")
		|| tag.contains("anonymous")
		|| tag.contains("dnp")
		|| tag.ends_with("_warning")
		|| matches!(tag, "avoid_posting" | "third-party_edit" | "artist_request" | "banned_artist"))
}

/// Character tags that name a character, not just "someone".
fn is_character(tag: &str) -> bool {
	!(tag.starts_with("unknown_") || tag.starts_with("ambiguous_") || tag == "fan_character")
}

fn named(post: &Post, category: &str, keep: fn(&str) -> bool) -> Vec<String> {
	post.categories.get(category).into_iter().flatten().filter(|t| keep(t)).cloned().collect()
}

/// How telling a tag is: rarer is more (natural log of how rare, out of ~10M posts).
fn weight(count: Option<u64>) -> f32 {
	match count {
		Some(c) => (1e7 / c.max(1) as f32).ln().max(0.0),
		// Not counted: somewhere in the middle.
		None => 6.0,
	}
}

/// Posts ranked by the weight of the tags they share with `post`, best first,
/// each once. Every one already has what was searched for; this only puts the
/// closest first.
pub fn rank(found: Vec<Post>, weights: &HashMap<String, f32>, limit: usize) -> Vec<Post> {
	let mut scored: Vec<(f32, Post)> = found.into_iter().map(|p| (weights.iter().filter(|(t, _)| p.has_tag(t)).map(|(_, w)| *w).sum::<f32>(), p)).collect();
	scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
	let mut seen = std::collections::HashSet::new();
	scored.into_iter().map(|(_, p)| p).filter(|p| seen.insert(p.id)).take(limit).collect()
}

/// Searches for posts with every one of `tags` (a `~` group counts as one
/// either-or), within the ratings ticked. The post itself and copies of it
/// are left out.
fn ask(site: &dyn Site, post: &Post, tags: &[String], ratings: &[Rating]) -> Result<(String, Vec<Post>)> {
	let caps = site.caps();
	let mut query: Vec<String> = tags.iter().take(caps.max_tags.max(1)).cloned().collect();
	// The rating filter goes in when there's room; what's out of range is dropped below either way.
	if let Some(r) = rating_filter(ratings)?
		&& (query.len() < caps.max_tags || caps.free_metas.contains(&"rating"))
	{
		query.push(r.to_string());
	}
	let query = query.join(" ");
	let mut found = Vec::new();
	let mut cursor = None;
	for _ in 0..PAGES {
		let page = site.search(&query, cursor, caps.page_size)?;
		found.extend(page.posts);
		cursor = page.next;
		if cursor.is_none() {
			break;
		}
	}
	let found = found
		.into_iter()
		.filter(|p| ratings.contains(&p.rating) && !(p.site == post.site && p.id == post.id) && (p.md5.is_none() || p.md5 != post.md5))
		.collect();
	Ok((query, found))
}

/// Finds posts like `post` on its site, within the ratings ticked.
pub fn find(site: &dyn Site, post: &Post, ratings: &[Rating], limit: usize) -> Result<Found> {
	let artists = named(post, "artist", is_artist);
	let characters = named(post, "character", is_character);
	let mut tries: Vec<(Vec<String>, String)> = Vec::new();
	let who = match (artists.is_empty(), characters.is_empty()) {
		(false, false) => "the same artist and characters",
		(false, true) => "the same artist",
		(true, false) => "the same characters",
		(true, true) => "",
	};
	if !who.is_empty() {
		tries.push((artists.iter().chain(&characters).cloned().collect(), who.to_string()));
	}
	if characters.len() > 1 && site.caps().tilde_or {
		let any = artists.iter().cloned().chain(characters.iter().map(|c| format!("~{c}"))).collect();
		let by = if artists.is_empty() { "any of the same characters" } else { "the same artist, with any of the same characters" };
		tries.push((any, by.to_string()));
	}
	// Ranked by what else they share; equal weights until the tags are counted.
	let content: Vec<String> = content_tags(post).into_iter().filter(|t| !characters.contains(t)).collect();
	let even: HashMap<String, f32> = content.iter().map(|t| (t.clone(), weight(None))).collect();
	for (tags, by) in &tries {
		let (query, posts) = ask(site, post, tags, ratings)?;
		if !posts.is_empty() {
			return Ok(Found { query, by: by.clone(), tags: tags.clone(), posts: rank(posts, &even, limit) });
		}
	}
	let counts = site.tag_counts(&content.iter().take(100).cloned().collect::<Vec<_>>()).unwrap_or_default();
	let weights: HashMap<String, f32> = content.iter().map(|t| (t.clone(), weight(counts.get(t).copied()))).collect();
	// The most telling tags on enough posts to find others.
	let mut telling: Vec<&String> = content.iter().filter(|t| counts.get(*t).is_none_or(|c| *c >= MIN_POSTS)).collect();
	// Catch-alls (ambiguous_species, unknown_species) say nothing about what it is.
	telling.retain(|t| !t.starts_with("ambiguous_") && !t.starts_with("unknown_"));
	telling.sort_by(|a, b| weights[*b].total_cmp(&weights[*a]).then(a.cmp(b)));
	// What it's of comes first where the site says (e621's species): a fox in
	// a hat finds foxes in hats, not anything in a hat.
	let species = post.categories.get("species").and_then(|s| telling.iter().copied().find(|t| s.contains(t)));
	let top: Vec<&String> = species.into_iter().chain(telling.iter().copied().filter(|t| Some(*t) != species)).take(2).collect();
	let before = if tries.is_empty() { String::new() } else { format!("nothing else has {who}, so ") };
	let mut last = None;
	for n in [2, 1] {
		if top.len() < n {
			continue;
		}
		let tags: Vec<String> = top.iter().take(n).map(|t| t.to_string()).collect();
		let by = format!("{before}{}", if n == 2 { "its two most telling tags" } else { "its most telling tag" });
		let (query, posts) = ask(site, post, &tags, ratings)?;
		if !posts.is_empty() {
			return Ok(Found { query, by, tags, posts: rank(posts, &weights, limit) });
		}
		last = Some(Found { query, by, tags, posts: Vec::new() });
	}
	Ok(last.unwrap_or(Found { query: String::new(), by: format!("{before}it has no tags to search by"), tags: Vec::new(), posts: Vec::new() }))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sites::{Capabilities, Cursor, Page, TagSuggestion};
	use std::collections::BTreeMap;
	use std::sync::Mutex;

	fn post(id: u64, artist: &str, character: &str, general: &[&str]) -> Post {
		let mut categories = BTreeMap::from([("general".to_string(), general.iter().map(|t| t.to_string()).collect::<Vec<_>>())]);
		if !artist.is_empty() {
			categories.insert("artist".into(), vec![artist.into()]);
		}
		if !character.is_empty() {
			categories.insert("character".into(), character.split(' ').map(String::from).collect());
		}
		// Sorted, as the site adapters keep them (has_tag relies on it).
		let mut tags: Vec<String> = categories.values().flatten().cloned().collect();
		tags.sort();
		Post {
			site: "t".into(),
			id,
			categories,
			tags,
			rating: Rating::Safe,
			score: 0,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: "png".into(),
			md5: Some(format!("md5-{id}")),
			file_url: None,
			sample_url: None,
			preview_url: None,
			page_url: String::new(),
			sources: Vec::new(),
			created_at: String::new(),
		}
	}

	/// A site that answers `a b ~c ~d` over its posts, and counts tags.
	struct Fake {
		posts: Vec<Post>,
		asked: Mutex<Vec<String>>,
	}

	impl Site for Fake {
		fn id(&self) -> &str {
			"t"
		}
		fn name(&self) -> &str {
			"Test"
		}
		fn base(&self) -> &str {
			"https://example.invalid"
		}
		fn caps(&self) -> Capabilities {
			Capabilities { max_tags: 40, tilde_or: true, max_wildcards: 1, metas: &["rating"], free_metas: &[], page_size: 50 }
		}
		fn search(&self, query: &str, _: Option<Cursor>, _: usize) -> Result<Page> {
			self.asked.lock().unwrap().push(query.to_string());
			let words: Vec<&str> = query.split_whitespace().filter(|w| !w.contains(':')).collect();
			let any: Vec<&str> = words.iter().filter_map(|w| w.strip_prefix('~')).collect();
			let all: Vec<&&str> = words.iter().filter(|w| !w.starts_with('~')).collect();
			let posts = self
				.posts
				.iter()
				.filter(|p| all.iter().all(|w| p.has_tag(w)) && (any.is_empty() || any.iter().any(|w| p.has_tag(w))))
				.cloned()
				.collect();
			Ok(Page { posts, next: None })
		}
		fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
			Ok(names.iter().map(|n| (n.clone(), 100 * self.posts.iter().filter(|p| p.has_tag(n)).count() as u64)).collect())
		}
		fn aliases(&self, _: &[String]) -> Result<HashMap<String, String>> {
			Ok(HashMap::new())
		}
		fn autocomplete(&self, _: &str) -> Result<Vec<TagSuggestion>> {
			Ok(vec![])
		}
		fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>> {
			Ok(vec![])
		}
	}

	fn ids(found: &Found) -> Vec<u64> {
		found.posts.iter().map(|p| p.id).collect()
	}

	#[test]
	fn same_artist_and_character_first() {
		let me = post(1, "kim", "rex", &["dinosaur", "hat", "solo"]);
		let site = Fake {
			posts: vec![
				me.clone(),
				post(2, "kim", "rex", &["dinosaur", "solo"]),
				post(3, "kim", "rex", &["dinosaur", "hat", "solo"]),
				post(4, "kim", "", &["dinosaur", "hat", "solo"]),
				post(5, "lee", "rex", &["dinosaur", "hat", "solo"]),
			],
			asked: Mutex::new(vec![]),
		};
		let found = find(&site, &me, &Rating::ALL, 10).unwrap();
		assert_eq!(found.by, "the same artist and characters");
		assert_eq!(found.query, "kim rex");
		// Only those two; the one sharing more of the rest first.
		assert_eq!(ids(&found), vec![3, 2]);
	}

	#[test]
	fn falls_back_to_the_most_telling_tags() {
		let me = post(1, "kim", "rex tia", &["dinosaur", "hat", "umbrella", "solo"]);
		let mut posts = vec![me.clone(), post(2, "lee", "", &["umbrella", "hat", "solo"]), post(3, "lee", "", &["dinosaur", "solo"])];
		// Common tags, so `solo` and `dinosaur` aren't telling.
		posts.extend((10..20).map(|id| post(id, "lee", "", &["dinosaur", "solo"])));
		let site = Fake { posts, asked: Mutex::new(vec![]) };
		let found = find(&site, &me, &[Rating::Safe], 10).unwrap();
		// Same artist and both characters, then either character, then the tags.
		let asked = site.asked.lock().unwrap().clone();
		assert_eq!(asked[..2], ["kim rex tia rating:s", "kim ~rex ~tia rating:s"]);
		assert_eq!(found.tags, vec!["hat", "umbrella"]);
		assert_eq!(found.by, "nothing else has the same artist and characters, so its two most telling tags");
		assert_eq!(ids(&found), vec![2]);
	}

	#[test]
	fn the_species_leads_the_tags() {
		let mut me = post(1, "", "", &["hat", "umbrella"]);
		me.categories.insert("species".into(), vec!["fox".into()]);
		me.tags.push("fox".into());
		me.tags.sort();
		let mut fox = post(2, "", "", &["hat"]);
		fox.categories.insert("species".into(), vec!["fox".into()]);
		fox.tags.push("fox".into());
		fox.tags.sort();
		// `fox` is on more posts than `umbrella`, and still goes first.
		let mut posts = vec![me.clone(), fox, post(3, "", "", &["hat", "umbrella"])];
		posts.extend((10..14).map(|id| post(id, "", "", &["fox"])));
		let site = Fake { posts, asked: Mutex::new(vec![]) };
		let found = find(&site, &me, &Rating::ALL, 10).unwrap();
		assert_eq!(site.asked.lock().unwrap()[0], "fox umbrella");
		// No other fox has an umbrella, so: foxes, the one in a hat first.
		assert_eq!((found.query.as_str(), found.by.as_str()), ("fox", "its most telling tag"));
		assert_eq!(ids(&found)[0], 2);
	}

	#[test]
	fn names_that_arent_artists_are_skipped() {
		for t in ["conditional_dnp", "sound_warning", "unknown_artist", "anonymous_artist", "avoid_posting"] {
			assert!(!is_artist(t), "{t}");
		}
		assert!(is_artist("kim_possible_artist") && is_artist("zackary911"));
		assert!(!is_character("unknown_character") && is_character("rex"));
	}
}
