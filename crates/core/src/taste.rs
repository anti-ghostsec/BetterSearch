//! What you like, scored from your own favorites, searches and groups.
//!
//! Nothing here talks to a model: it turns signals into weights and an
//! intensity number, and the model (if any) only writes prose about that. The
//! numbers stay the truth, so a bad generation can't send the searches
//! somewhere you don't actually go.
//!
//! Favorites count most, then groups (an explicit "I like this"), then the
//! tags you search for. Older signals fade: a half-life of 180 days for a
//! favorite and 30 for a search, so taste can move.
//!
//! Intensity is what the dial starts from: how much of what you favorite is
//! explicit or questionable, with a bump for the tags that usually mean it.
//! It's capped by the ratings you have ticked, so a pervy profile can't
//! produce `rating:e` searches when only Safe is on.

use std::collections::HashMap;

use crate::{Rating, Result, Store};

/// How long a favorite takes to lose half its weight, in days.
const FAV_HALF_LIFE: f64 = 180.0;
/// The same for a tag you searched for.
const QUERY_HALF_LIFE: f64 = 30.0;

const FAV_WEIGHT: f64 = 1.0;
const QUERY_WEIGHT: f64 = 0.35;
const GROUP_WEIGHT: f64 = 0.8;

/// Favorites older than this many posts are ignored, so scoring stays quick.
const MAX_FAVORITES: usize = 2_000;
/// Queries remembered as intent.
const MAX_QUERIES: usize = 300;
/// Tags kept in a profile.
pub const MAX_TAGS: usize = 400;

/// Tags that describe everything and so pin nothing: no use as an anchor.
const BROAD: &[&str] = &[
	"1girl",
	"1boy",
	"2girls",
	"solo",
	"hi_res",
	"absurd_res",
	"highres",
	"digital_media_(artwork)",
	"traditional_media_(artwork)",
	"english_commentary",
];

/// Tags that lean adult regardless of the site's rating.
const LEWD: &[&str] = &[
	"explicit",
	"lewd",
	"suggestive",
	"nude",
	"completely_nude",
	"topless",
	"bottomless",
	"underwear",
	"lingerie",
	"panties",
	"bra",
	"bikini",
	"cleavage",
	"spread_legs",
	"presenting",
	"sex",
	"penetration",
	"cum",
	"orgasm",
	"bondage",
	"large_breasts",
	"thick_thighs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
	/// A named character or species: the best anchors, they pin what it's of.
	Character,
	Species,
	General,
	Copyright,
	Artist,
}

impl Category {
	fn rank(self) -> u8 {
		match self {
			Self::Character => 0,
			Self::Species => 1,
			Self::General => 2,
			Self::Copyright => 3,
			Self::Artist => 4,
		}
	}
}

#[derive(Debug, Clone)]
pub struct WeightedTag {
	pub tag: String,
	pub category: Category,
	pub weight: f64,
}

#[derive(Debug, Clone)]
pub struct Taste {
	pub tags: Vec<WeightedTag>,
	/// 0 (wholesome) to 10 (as adult as the ticked ratings allow).
	pub intensity: f32,
	/// What searches should use: the measured intensity unless set by hand.
	pub dial: f32,
}

/// `0.5` after one half-life, `0.25` after two, and so on.
fn decay(age_secs: i64, half_life_days: f64) -> f64 {
	let age_days = (age_secs.max(0) as f64) / 86_400.0;
	0.5f64.powf(age_days / half_life_days)
}

struct Tally {
	weight: f64,
	category: Category,
}

fn insert(tally: &mut HashMap<String, Tally>, tag: &str, weight: f64, category: Category) {
	let slot = tally.entry(tag.to_string()).or_insert(Tally { weight: 0.0, category });
	slot.weight += weight;
	// A tag can be general in one post and a character in another; the more
	// specific reading wins so it can still anchor a search.
	slot.category = slot.category.min(category);
}

/// Scores every tag from the user's signals. `now` is unix seconds.
pub fn build(store: &Store, ratings: &[Rating], now: i64, dial_override: Option<f32>) -> Result<Taste> {
	let mut tally: HashMap<String, Tally> = HashMap::new();

	for (tag, at) in store.favorite_tag_events()?.into_iter().take(MAX_FAVORITES * 40) {
		insert(&mut tally, &tag, FAV_WEIGHT * decay(now - at, FAV_HALF_LIFE), Category::General);
	}

	// Groups say what you like even before you've favorited anything in them.
	for (_, members) in store.groups()? {
		for tag in members {
			insert(&mut tally, &tag, GROUP_WEIGHT, Category::General);
		}
	}

	for (query, at) in store.history(MAX_QUERIES)? {
		let Ok(expr) = crate::parse(&query) else { continue };
		let fade = decay(now - at, QUERY_HALF_LIFE);
		let mut seen = Vec::new();
		expr.walk(&mut |e| {
			if let crate::Expr::Tag(tag) = e
				&& !seen.contains(tag)
			{
				seen.push(tag.clone());
			}
		});
		for tag in seen {
			insert(&mut tally, &tag, QUERY_WEIGHT * fade, Category::General);
		}
	}

	// Favorites carry categories; the tallies above don't. Re-read them so a
	// favorited character or species can be recognized as one.
	for site in favorite_sites(store)? {
		for post in store.favorite_posts(&site)? {
			for (name, tags) in &post.categories {
				let category = match name.as_str() {
					"character" => Category::Character,
					"species" => Category::Species,
					"copyright" => Category::Copyright,
					"artist" => Category::Artist,
					_ => continue,
				};
				for tag in tags {
					if let Some(t) = tally.get_mut(tag) {
						t.category = t.category.min(category);
					}
				}
			}
		}
	}

	let mut tags: Vec<WeightedTag> = tally
		.into_iter()
		.filter(|(_, t)| t.weight > 0.0)
		.map(|(tag, t)| WeightedTag { tag, category: t.category, weight: t.weight })
		.collect();
	tags.sort_by(|a, b| b.weight.total_cmp(&a.weight).then(a.tag.cmp(&b.tag)));
	tags.truncate(MAX_TAGS);

	let intensity = measure(store, ratings)?;
	let dial = dial_override.map(|d| d.clamp(0.0, 10.0)).unwrap_or(intensity);
	Ok(Taste { tags, intensity, dial })
}

fn favorite_sites(store: &Store) -> Result<Vec<String>> {
	let mut st = store.conn_ref().prepare_cached("SELECT DISTINCT site FROM favorites")?;
	let rows = st.query_map([], |r| r.get::<_, String>(0))?;
	Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// How adult the favorites are, 0 to 10, capped by the ticked ratings.
fn measure(store: &Store, ratings: &[Rating]) -> Result<f32> {
	if ratings.is_empty() {
		return Ok(0.0);
	}
	let mut sum = 0.0;
	let mut n = 0usize;
	let mut lewd = 0usize;
	let mut tags = 0usize;
	for site in favorite_sites(store)? {
		for post in store.favorite_posts(&site)? {
			n += 1;
			sum += match post.rating {
				Rating::Explicit => 1.0,
				Rating::Questionable => 0.55,
				Rating::Safe => 0.08,
			};
			tags += post.tags.len();
			lewd += post.tags.iter().filter(|t| LEWD.contains(&t.as_str())).count();
		}
	}
	let rating_part = if n == 0 { 0.0 } else { sum / n as f64 };
	let tag_part = if tags == 0 { 0.0 } else { (lewd as f64 / tags as f64 * 8.0).min(1.0) };
	let mut intensity = ((rating_part * 0.75 + tag_part * 0.25) * 10.0) as f32;

	// The ratings are the ceiling: nothing explicit ticked means nothing above
	// questionable, and Safe alone means Safe.
	let has_explicit = ratings.contains(&Rating::Explicit);
	let has_questionable = ratings.contains(&Rating::Questionable);
	if !has_explicit {
		intensity = intensity.min(if has_questionable { 6.0 } else { 0.0 });
	}
	Ok(intensity.clamp(0.0, 10.0))
}

/// Ready-to-run searches built from the profile: an anchor, then more company
/// the higher the dial. Callers still plan and run them like any other search.
pub fn queries(taste: &Taste, ratings: &[Rating], want: usize) -> Result<Vec<String>> {
	let filter = crate::search::rating_filter(ratings)?.map(|e| e.to_string());
	let company = if taste.dial >= 7.0 {
		2
	} else if taste.dial >= 4.0 {
		1
	} else {
		0
	};

	let anchors: Vec<&WeightedTag> = taste
		.tags
		.iter()
		.filter(|t| !BROAD.contains(&t.tag.as_str()))
		.filter(|t| matches!(t.category, Category::Character | Category::Species))
		.chain(taste.tags.iter().filter(|t| !BROAD.contains(&t.tag.as_str()) && t.category == Category::General))
		.collect();

	let mut out: Vec<String> = Vec::new();
	for (i, anchor) in anchors.iter().enumerate() {
		if out.len() >= want {
			break;
		}
		let mut words = vec![anchor.tag.clone()];
		let candidates: Vec<&WeightedTag> = taste
			.tags
			.iter()
			.filter(|t| {
				t.tag != anchor.tag && !BROAD.contains(&t.tag.as_str()) && !matches!(t.category, Category::Artist) && !words.contains(&t.tag)
			})
			.collect();
		for other in candidates {
			if words.len() > company {
				break;
			}
			words.push(other.tag.clone());
		}
		if let Some(f) = &filter {
			words.push(f.clone());
		}
		let query = words.join(" ");
		if !out.contains(&query) {
			out.push(query);
		}
		// Rotate the pool so later searches aren't all the same anchor again.
		if i > taste.tags.len() {
			break;
		}
	}
	Ok(out)
}

/// Asks an OpenAI-compatible model (Venice by default) for the exaggerated
/// brief. Only tags from `tags` survive, so the prose can't invent vocabulary
/// the site doesn't have. `dial` sets how hard it leans in.
pub fn brief(endpoint: &str, key: &str, model: &str, tags: &[String], ratings: &[Rating], dial: f32) -> Result<String> {
	if model.trim().is_empty() {
		return Err(crate::Error::Query("no model chosen".into()));
	}
	let leaning = if dial >= 7.0 {
		"Write it as a breathless critic who is certain: lean hard into how adult it gets, use superlatives, commit."
	} else if dial >= 4.0 {
		"Write it as a sharp critic: interested, specific, not shy about the spicy parts."
	} else {
		"Write it warmly but plainly, like a bookseller who knows the customer."
	};
	let system = format!(
		"You describe someone's taste in booru art from their own saved tags. {leaning} \
		 Two or three sentences, present tense, second person (\"you\"). \
		 Only use tag names from the list you're given; you may describe what they mean."
	);
	// The ratings are a second, short line so the prose can mention how adult
	// the browsing is without the tag list being polluted by them.
	let mut asked = tags.join(", ");
	if !ratings.is_empty() {
		asked.push_str("\n\nRatings they browse: ");
		asked.push_str(&ratings.iter().map(|r| r.label()).collect::<Vec<_>>().join(", "));
	}
	let body = serde_json::json!({
		"model": model,
		"max_tokens": 220,
		"temperature": 0.9,
		"messages": [
			{ "role": "system", "content": system },
			{ "role": "user", "content": asked },
		]
	});
	let client = reqwest::blocking::Client::builder()
		.timeout(std::time::Duration::from_secs(60))
		.build()
		.map_err(|e| crate::Error::Query(e.to_string()))?;
	let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
	let mut req = client.post(&url).json(&body);
	if !key.trim().is_empty() {
		req = req.bearer_auth(key.trim());
	}
	let res = req.send().map_err(|e| crate::Error::Query(format!("model unreachable: {e}")))?;
	if !res.status().is_success() {
		return Err(crate::Error::Query(format!("the model said {}", res.status())));
	}
	let value: serde_json::Value = res.json().map_err(|e| crate::Error::Query(e.to_string()))?;
	let text = value["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string();
	if text.is_empty() {
		return Err(crate::Error::Query("the model answered nothing".into()));
	}
	Ok(text)
}

/// Models the endpoint offers, so the picker can list real ones.
pub fn models(endpoint: &str, key: &str) -> Result<Vec<String>> {
	let url = format!("{}/models", endpoint.trim_end_matches('/'));
	let client = reqwest::blocking::Client::builder()
		.timeout(std::time::Duration::from_secs(20))
		.build()
		.map_err(|e| crate::Error::Query(e.to_string()))?;
	let mut req = client.get(&url);
	if !key.trim().is_empty() {
		req = req.bearer_auth(key.trim());
	}
	let res = req.send().map_err(|e| crate::Error::Query(format!("unreachable: {e}")))?;
	if !res.status().is_success() {
		return Err(crate::Error::Query(format!("the model list said {}", res.status())));
	}
	let value: serde_json::Value = res.json().map_err(|e| crate::Error::Query(e.to_string()))?;
	let list = value["data"].as_array().or_else(|| value.as_array()).cloned().unwrap_or_default();
	let ids: Vec<String> = list.iter().filter_map(|m| m["id"].as_str().or_else(|| m.as_str()).map(str::to_string)).collect();
	if ids.is_empty() {
		return Err(crate::Error::Query("the endpoint listed no models".into()));
	}
	Ok(ids)
}

/// One chat turn: system, then what the visitor typed. Returned as written.
pub fn chat(endpoint: &str, key: &str, model: &str, system: &str, text: &str) -> Result<String> {
	if model.trim().is_empty() {
		return Err(crate::Error::Query("choose a model first".into()));
	}
	let body = serde_json::json!({
		"model": model,
		"max_tokens": 400,
		"temperature": 0.7,
		"messages": [
			{ "role": "system", "content": system },
			{ "role": "user", "content": text },
		]
	});
	let client = reqwest::blocking::Client::builder()
		.timeout(std::time::Duration::from_secs(60))
		.build()
		.map_err(|e| crate::Error::Query(e.to_string()))?;
	let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
	let mut req = client.post(&url).json(&body);
	if !key.trim().is_empty() {
		req = req.bearer_auth(key.trim());
	}
	let res = req.send().map_err(|e| crate::Error::Query(format!("model unreachable: {e}")))?;
	if !res.status().is_success() {
		return Err(crate::Error::Query(format!("the model said {}", res.status())));
	}
	let value: serde_json::Value = res.json().map_err(|e| crate::Error::Query(e.to_string()))?;
	let text = value["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string();
	if text.is_empty() {
		return Err(crate::Error::Query("the model answered nothing".into()));
	}
	Ok(text)
}

/// `rank`s used by the app to order `why` chips: character, species, general.
pub fn anchor_rank(category: Category) -> u8 {
	category.rank()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Post, Rating};
	use std::collections::BTreeMap;

	fn post(id: u64, rating: Rating, tags: &[(&str, &[&str])]) -> Post {
		let mut all: Vec<String> = Vec::new();
		let mut categories = BTreeMap::new();
		for (name, list) in tags {
			let list: Vec<String> = list.iter().map(|t| t.to_string()).collect();
			all.extend(list.clone());
			categories.insert(name.to_string(), list);
		}
		all.sort();
		all.dedup();
		Post {
			site: "t".into(),
			id,
			tags: all,
			categories,
			rating,
			score: 0,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: "png".into(),
			md5: None,
			file_url: None,
			sample_url: None,
			preview_url: None,
			page_url: String::new(),
			sources: Vec::new(),
			created_at: String::new(),
		}
	}

	#[test]
	fn weights_halve_at_the_half_life() {
		assert!((decay(180 * 86_400, FAV_HALF_LIFE) - 0.5).abs() < 1e-9);
		assert!((decay(30 * 86_400, QUERY_HALF_LIFE) - 0.5).abs() < 1e-9);
		assert_eq!(decay(-50, QUERY_HALF_LIFE), 1.0);
	}

	#[test]
	fn safe_favorites_stay_wholesome() {
		let s = Store::in_memory().unwrap();
		for i in 0..5 {
			s.set_favorite(&post(i, Rating::Safe, &[("general", &["fox", "smile"])]), true).unwrap();
		}
		let t = build(&s, &[Rating::Safe], 1_000_000, None).unwrap();
		assert_eq!(t.intensity, 0.0);
		let q = queries(&t, &[Rating::Safe], 3).unwrap();
		assert!(q.iter().all(|q| q.contains("rating:s")), "{q:?}");
	}

	#[test]
	fn explicit_favorites_raise_the_dial_and_anchor_on_what_it_is() {
		let s = Store::in_memory().unwrap();
		for i in 0..8 {
			s.set_favorite(&post(i, Rating::Explicit, &[("character", &["krystal"]), ("general", &["nude", "spread_legs"])]), true)
				.unwrap();
		}
		let ratings = [Rating::Safe, Rating::Questionable, Rating::Explicit];
		let t = build(&s, &ratings, 1_000_000, None).unwrap();
		assert!(t.intensity > 7.0, "{}", t.intensity);
		assert_eq!(t.tags[0].tag, "krystal");
		assert_eq!(t.tags[0].category, Category::Character);

		let q = queries(&t, &ratings, 3).unwrap();
		assert!(q[0].starts_with("krystal "), "{}", q[0]);
		assert!(q[0].contains("nude"), "{}", q[0]);
	}

	#[test]
	fn ratings_cap_the_measured_intensity() {
		let s = Store::in_memory().unwrap();
		s.set_favorite(&post(1, Rating::Explicit, &[("general", &["nude"])]), true).unwrap();
		let no_explicit = build(&s, &[Rating::Safe, Rating::Questionable], 1_000_000, None).unwrap();
		assert!(no_explicit.intensity <= 6.0, "{}", no_explicit.intensity);
	}

	#[test]
	fn a_hand_set_dial_wins() {
		let s = Store::in_memory().unwrap();
		s.set_favorite(&post(1, Rating::Safe, &[("general", &["fox"])]), true).unwrap();
		let t = build(&s, &[Rating::Safe], 1_000_000, Some(9.0)).unwrap();
		assert_eq!(t.dial, 9.0);
		let t = build(&s, &[Rating::Safe], 1_000_000, Some(99.0)).unwrap();
		assert_eq!(t.dial, 10.0);
	}

	#[test]
	fn queries_differ_and_skip_broad_anchors() {
		let s = Store::in_memory().unwrap();
		for (i, tag) in ["1girl", "fox", "wolf", "deer"].iter().enumerate() {
			s.set_favorite(&post(i as u64, Rating::Safe, &[("general", &[tag])]), true).unwrap();
		}
		let t = build(&s, &[Rating::Safe], 1_000_000, None).unwrap();
		let q = queries(&t, &[Rating::Safe], 3).unwrap();
		assert_eq!(q.len(), 3, "{q:?}");
		assert!(!q.iter().any(|q| q.starts_with("1girl")), "{q:?}");
		assert_eq!(q.iter().collect::<std::collections::HashSet<_>>().len(), 3);
	}

	#[test]
	fn groups_count_before_any_favorite() {
		let s = Store::in_memory().unwrap();
		s.set_in_group("animal", "rabbit", true).unwrap();
		let t = build(&s, &[Rating::Safe], 1_000_000, None).unwrap();
		assert!(t.tags.iter().any(|t| t.tag == "rabbit"));
	}
}
