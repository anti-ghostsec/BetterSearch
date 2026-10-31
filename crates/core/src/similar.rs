// "Find similar" for a post: posts sharing the most *telling* tags with it.
// A tag is telling when it's on few posts (a specific species, an object, a
// character) and weak when it's everywhere (solo, smile), so tags are weighted
// by how rare they are, the rarest few are searched for, and what comes back
// is ranked by the weight of the tags it shares.

use std::collections::HashMap;

use crate::search::rating_filter;
use crate::sites::{NOT_THINGS, Site};
use crate::{Post, Rating, Result};

/// Tags searched for (as `~a ~b …`); fewer where the site takes fewer.
const SEARCH_TAGS: usize = 6;
/// Pages of results ranked.
const PAGES: usize = 2;

/// The post's tags that say what's in it: not the artist, series, or meta tags.
fn content_tags(post: &Post) -> Vec<String> {
	if post.categories.is_empty() {
		return post.tags.clone();
	}
	post.categories.iter().filter(|(c, _)| !NOT_THINGS.contains(&c.as_str())).flat_map(|(_, t)| t.iter().cloned()).collect()
}

/// How telling a tag is: rarer is more (natural log of how rare, out of ~10M posts).
fn weight(count: Option<u64>) -> f32 {
	match count {
		Some(c) => (1e7 / c.max(1) as f32).ln().max(0.0),
		// Not counted: somewhere in the middle.
		None => 6.0,
	}
}

/// Posts ranked by the weight of the tags they share with `post`, best first;
/// the post itself, copies of it, and posts sharing too little are left out.
pub fn rank(post: &Post, found: Vec<Post>, weights: &HashMap<String, f32>, limit: usize) -> Vec<Post> {
	let mine: Vec<&String> = weights.keys().collect();
	let top: f32 = {
		let mut w: Vec<f32> = weights.values().copied().collect();
		w.sort_by(|a, b| b.total_cmp(a));
		w.iter().take(3).sum()
	};
	let mut scored: Vec<(f32, Post)> = found
		.into_iter()
		.filter(|p| !(p.site == post.site && p.id == post.id) && (p.md5.is_none() || p.md5 != post.md5))
		.map(|p| (mine.iter().filter(|t| p.has_tag(t)).map(|t| weights[*t]).sum::<f32>(), p))
		// At least half of what its three most telling tags would give.
		.filter(|(s, _)| *s >= top * 0.5)
		.collect();
	scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
	let mut seen = std::collections::HashSet::new();
	scored.into_iter().map(|(_, p)| p).filter(|p| seen.insert(p.id)).take(limit).collect()
}

/// Finds posts like `post` on its site, within the ratings ticked. Returns the
/// query used, the tags it was built from, and the ranked posts.
pub fn find(site: &dyn Site, post: &Post, ratings: &[Rating], limit: usize) -> Result<(String, Vec<String>, Vec<Post>)> {
	let tags = content_tags(post);
	let counts = site.tag_counts(&tags.iter().take(100).cloned().collect::<Vec<_>>()).unwrap_or_default();
	let weights: HashMap<String, f32> = tags.iter().map(|t| (t.clone(), weight(counts.get(t).copied()))).collect();
	// The rarest tags with enough posts to find anything.
	let mut pick: Vec<&String> = tags.iter().filter(|t| counts.get(*t).is_none_or(|c| *c >= 10)).collect();
	pick.sort_by(|a, b| weights[*b].total_cmp(&weights[*a]).then(a.cmp(b)));
	// What it's *of* comes first: the rarest species or character tag is required,
	// so a dog playing guitar finds dogs, not anyone in a plaid shirt.
	let of: Option<String> = ["character", "species"]
		.iter()
		.filter_map(|c| post.categories.get(*c))
		.flatten()
		.filter(|t| counts.get(*t).is_some_and(|c| *c >= 50))
		// Catch-alls (ambiguous_species, unknown_species) say nothing about what it is.
		.filter(|t| !t.starts_with("ambiguous_") && !t.starts_with("unknown_"))
		.max_by(|a, b| weights[*a].total_cmp(&weights[*b]).then(b.cmp(a)))
		.cloned();
	let caps = site.caps();
	let room = caps.max_tags.saturating_sub(2 + of.is_some() as usize);
	let n = if caps.tilde_or && room >= SEARCH_TAGS { SEARCH_TAGS } else { usize::from(of.is_none()) };
	let pick: Vec<String> = pick.into_iter().filter(|t| Some(*t) != of.as_ref()).take(n).cloned().collect();
	let mut query: Vec<String> = of.iter().cloned().collect();
	if pick.len() > 1 {
		query.extend(pick.iter().map(|t| format!("~{t}")));
	} else {
		query.extend(pick.iter().cloned());
	}
	let pick: Vec<String> = of.into_iter().chain(pick).collect();
	if let Some(r) = rating_filter(ratings)? {
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
	let found: Vec<Post> = found.into_iter().filter(|p| ratings.contains(&p.rating)).collect();
	Ok((query, pick, rank(post, found, &weights, limit)))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::BTreeMap;

	fn post(id: u64, tags: &[&str]) -> Post {
		// Sorted, as the site adapters keep them (has_tag relies on it).
		let mut tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
		tags.sort();
		Post {
			site: "t".into(),
			id,
			categories: BTreeMap::from([("general".to_string(), tags.clone())]),
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

	#[test]
	fn ranks_by_shared_telling_tags() {
		let me = post(1, &["red_panda", "umbrella", "rain", "solo", "smile"]);
		let counts = HashMap::from([
			("red_panda".to_string(), 5_000u64),
			("umbrella".to_string(), 20_000),
			("rain".to_string(), 30_000),
			("solo".to_string(), 3_000_000),
			("smile".to_string(), 2_000_000),
		]);
		let weights: HashMap<String, f32> = content_tags(&me).into_iter().map(|t| (t.clone(), weight(counts.get(&t).copied()))).collect();
		let found = vec![
			post(2, &["solo", "smile", "fox"]),              // only common tags: out
			post(3, &["red_panda", "umbrella", "solo"]),     // two telling ones
			post(4, &["red_panda", "umbrella", "rain"]),     // all three telling ones: first
			post(1, &["red_panda", "umbrella", "rain"]),     // itself: out
			post(5, &["umbrella", "rain", "smile", "solo"]), // umbrella and rain
		];
		let got: Vec<u64> = rank(&me, found, &weights, 10).iter().map(|p| p.id).collect();
		assert_eq!(got, vec![4, 3, 5]);
	}
}
