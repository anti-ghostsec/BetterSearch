// Runs a plan: pages through each query, merges them newest first, drops
// repeats and keeps the posts that pass the whole search.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::plan::{Plan, apply_aliases, apply_translations, plan, tags_in};
use crate::post::{Post, Rating};
use crate::query::{Expr, parse};
use crate::sites::{Cursor, Site};
use crate::learn::learn;
use crate::store::{Asked, RelationEntry, Store};
use crate::{Error, Result};

/// Most site requests one `next` call may make, so a search that matches
/// little doesn't run forever. Calling `next` again carries on.
const MAX_REQUESTS: usize = 8;

struct Stream {
	query: String,
	started: bool,
	next: Option<Cursor>,
	buf: VecDeque<Post>,
}

impl Stream {
	fn has_more(&self) -> bool {
		!self.started || self.next.is_some()
	}
}

/// A relation asked about with `relation`, as this site searched it.
#[derive(Debug, Clone, PartialEq)]
pub struct AskedHere {
	/// In your words: a tag, or an `@group`.
	pub subject: String,
	pub verb: String,
	/// The site's tag for the verb: eat → eating.
	pub verb_tag: String,
	/// Each subject (a group's members, or the one tag) and what the relation
	/// index says it does the verb with.
	pub pairs: Vec<(String, Vec<RelationEntry>)>,
}

/// Most subjects one `@group … relation` search looks up.
const MAX_SUBJECTS: usize = 6;
/// Most objects learned per subject and verb.
const LEARN: usize = 10;

pub struct Search {
	site: Arc<dyn Site>,
	pub plan: Plan,
	pub asked: Option<AskedHere>,
	/// `favorited:yes`: only your favorites, from the vault; the site isn't asked.
	favorites_only: bool,
	streams: Vec<Stream>,
	seen: HashSet<u64>,
	local_done: bool,
	/// Posts looked at so far, matching or not.
	pub checked: usize,
}

/// The search for the ratings ticked: one is `rating:x`, two is `-rating:y`, all three adds nothing.
pub(crate) fn rating_filter(ratings: &[Rating]) -> Result<Option<Expr>> {
	let meta = |r: Rating| Expr::Meta { key: "rating".into(), value: r.letter().to_string() };
	let missing: Vec<Rating> = Rating::ALL.into_iter().filter(|r| !ratings.contains(r)).collect();
	match (ratings.len(), missing.as_slice()) {
		(0, _) => Err(Error::Query("Tick at least one rating.".into())),
		(_, []) => Ok(None),
		(1, _) => Ok(Some(meta(ratings[0]))),
		(_, [m]) => Ok(Some(Expr::Not(Box::new(meta(*m))))),
		_ => unreachable!(),
	}
}

impl Search {
	/// Reads the search, applies this site's tag translations, looks up aliases
	/// (and tag counts when it has to pick tags) and plans it. Makes up to two
	/// site requests.
	pub fn start(site: Arc<dyn Site>, text: &str, ratings: &[Rating], store: &Mutex<Store>) -> Result<Search> {
		Self::start_with(site, text, ratings, store, false)
	}

	/// `check_tags` also looks up tag counts to warn about tags the site
	/// doesn't have, which matters most when one search runs on several sites.
	pub fn start_with(site: Arc<dyn Site>, text: &str, ratings: &[Rating], store: &Mutex<Store>, check_tags: bool) -> Result<Search> {
		let mut notes = Vec::new();
		let (question, mut expr) = {
			let reading = store.lock().unwrap().read_relations(&parse(text)?)?;
			(reading.asked.into_iter().next(), reading.expr)
		};
		let mut relation = None;
		if let Some(q) = question {
			let (e, here) = Self::use_relations(site.as_ref(), store, q, expr, &mut notes)?;
			expr = e;
			relation = Some(here);
		}
		expr = {
			let s = store.lock().unwrap();
			let grouped = s.expand_groups(&expr)?;
			let expanded = s.expand_relations(&grouped)?;
			if expanded != grouped {
				notes.push(format!("From your relations, that's: {expanded}"));
			}
			expanded
		};
		let translations = store.lock().unwrap().translations(site.id())?;
		for t in tags_in_all(&expr) {
			match translations.get(&t) {
				Some(Expr::And(es)) if es.is_empty() => notes.push(format!("“{t}” isn't on {}, so it's left out there.", site.name())),
				Some(to) => notes.push(format!("“{t}” is searched as “{to}” on {}.", site.name())),
				None => {}
			}
		}
		expr = apply_translations(&expr, &translations);
		if let Some(r) = rating_filter(ratings)? {
			expr = match expr {
				Expr::And(mut es) => {
					es.push(r);
					Expr::And(es)
				}
				e => Expr::And(vec![e, r]),
			};
		}

		let caps = site.caps();
		// Tags in your links and relations are looked up too, so they fill in
		// on posts tagged with the site's current names.
		let asked = tags_in(&expr);
		let mut names = asked.clone();
		for t in store.lock().unwrap().pattern_tags()?.into_iter().take(200) {
			if !names.contains(&t) {
				names.push(t);
			}
		}

		let mut aliases = HashMap::new();
		let unknown: Vec<String> = {
			let s = store.lock().unwrap();
			names
				.iter()
				.filter(|n| match s.cached_alias(site.id(), n) {
					Some(Some(to)) => {
						aliases.insert((*n).clone(), to);
						false
					}
					Some(None) => false,
					None => true,
				})
				.cloned()
				.collect()
		};
		if !unknown.is_empty() {
			match site.aliases(&unknown) {
				Ok(found) => {
					let mut s = store.lock().unwrap();
					for n in &unknown {
						s.remember_alias(site.id(), n, found.get(n).cloned());
					}
					aliases.extend(found);
				}
				Err(e) => notes.push(format!("Couldn't look up tag aliases ({e}); old tag names may miss posts.")),
			}
		}
		for (from, to) in aliases.iter().filter(|(from, _)| asked.contains(from)) {
			notes.push(format!("“{from}” is now “{to}” on {}.", site.name()));
		}
		expr = apply_aliases(&expr, &aliases);
		expr = store.lock().unwrap().resolve_links(&expr)?;

		// Counts only matter when there are more tags than the site takes, or to check them.
		let names = tags_in(&expr);
		let counts = if check_tags || names.len() + 2 > caps.max_tags {
			match site.tag_counts(&names) {
				Ok(counts) => {
					if check_tags {
						for n in names.iter().filter(|n| counts.get(*n).is_none_or(|c| *c == 0)) {
							notes.push(format!("“{n}” has no posts on {}; a tag translation can fix that.", site.name()));
						}
					}
					counts
				}
				Err(_) => HashMap::new(),
			}
		} else {
			HashMap::new()
		};

		let mut plan = plan(&expr, &caps, &counts);
		notes.append(&mut plan.notes);
		plan.notes = notes;
		let favorites_only = conjuncts_of(&plan.filter)
			.iter()
			.any(|e| matches!(e, Expr::Meta { key, value } if key == "favorited" && matches!(value.as_str(), "yes" | "true" | "1")));
		if favorites_only {
			plan.local = true;
			plan.notes.push("Your favorites, kept on this computer; the site isn't asked.".into());
		}
		// A search only of saved posts asks the site nothing.
		let streams = if plan.local {
			Vec::new()
		} else {
			plan.queries.iter().map(|q| Stream { query: q.clone(), started: false, next: None, buf: VecDeque::new() }).collect()
		};
		Ok(Search { site, plan, asked: relation, favorites_only, streams, seen: HashSet::new(), local_done: false, checked: 0 })
	}

	/// `rabbit eat relation`: the relation index's objects for each subject
	/// (learning them on this site first if nothing is known yet), searched as
	/// rabbit ∧ eating ∧ (carrot | vegetable | …), one side per subject.
	fn use_relations(site: &dyn Site, store: &Mutex<Store>, q: Asked, expr: Expr, notes: &mut Vec<String>) -> Result<(Expr, AskedHere)> {
		let Some(subject) = q.subject else {
			return Err(Error::Query(format!("Say who, like “rabbit {} relation”.", q.verb)));
		};
		let verb_tag = match site_verb(site, &q.verb) {
			Some(t) => {
				if t != q.verb {
					notes.push(format!("“{}” is “{t}” on {}.", q.verb, site.name()));
				}
				t
			}
			None => {
				notes.push(format!("{} has no tag for “{}”, so it's searched as written.", site.name(), q.verb));
				q.verb.clone()
			}
		};
		let subjects: Vec<String> = if subject.starts_with('@') {
			let members = store.lock().unwrap().group(&subject)?;
			if members.is_empty() {
				return Err(Error::Query(format!("There's no group “{subject}” yet. Make one in Groups & relations.")));
			}
			if members.len() > MAX_SUBJECTS {
				notes.push(format!("{subject} has {} tags; the first {MAX_SUBJECTS} are asked about.", members.len()));
			}
			members.into_iter().take(MAX_SUBJECTS).collect()
		} else {
			vec![subject.clone()]
		};
		let mut pairs = Vec::new();
		let mut sides = Vec::new();
		for s in subjects {
			let mut known = store.lock().unwrap().relations(&s, &q.verb, false)?;
			if known.is_empty() && !store.lock().unwrap().has_learned(&s, &q.verb)? {
				// Learned under the site's name for the subject (dog → domestic_dog), without holding the store.
				let s_tag = site.aliases(std::slice::from_ref(&s)).ok().and_then(|m| m.get(&s).cloned()).unwrap_or_else(|| s.clone());
				match learn(site, &s_tag, &verb_tag, LEARN)? {
					Some(found) => {
						let list = found.iter().map(|l| l.object.as_str()).collect::<Vec<_>>().join(", ");
						notes.push(format!(
							"Learned on {} what “{s}” does “{}” with: {}.",
							site.name(),
							q.verb,
							if list.is_empty() { "nothing clear" } else { &list }
						));
						let st = store.lock().unwrap();
						st.remember_learned(&s, &q.verb, site.id(), &found)?;
						known = st.relations(&s, &q.verb, false)?;
					}
					None => notes.push(format!("{} can't learn relations; ask with e621/e926 or Danbooru ticked first.", site.name())),
				}
			}
			if !known.is_empty() {
				let objects: Vec<Expr> = known.iter().map(|e| Expr::Tag(e.object.clone())).collect();
				let objects = if objects.len() == 1 { objects.into_iter().next().unwrap() } else { Expr::Or(objects) };
				sides.push(Expr::And(vec![Expr::Tag(s.clone()), objects]));
			}
			pairs.push((s, known));
		}
		if sides.is_empty() {
			return Err(Error::Query(format!(
				"Nothing is known yet that {subject} does “{}” with. Tick e926/e621 or Danbooru so it can be learned, or add one in Groups & relations.",
				q.verb
			)));
		}
		let rel = if sides.len() == 1 { sides.pop().unwrap() } else { Expr::Or(sides) };
		notes.push(format!("From the relation index: {rel} {verb_tag}"));
		let expr = replace_with(&replace_tag(&expr, &q.verb, &verb_tag), &subject, &rel);
		Ok((expr, AskedHere { subject, verb: q.verb, verb_tag, pairs }))
	}

	pub fn site(&self) -> &Arc<dyn Site> {
		&self.site
	}

	/// Searches with links also look at posts saved here: some links hold tags
	/// of your own, which the site can't find.
	fn wants_saved(&self) -> bool {
		self.favorites_only || self.plan.local || self.plan.filter.any(&|e| matches!(e, Expr::Link { .. }))
	}

	pub fn done(&self) -> bool {
		(self.local_done || !self.wants_saved()) && self.streams.iter().all(|s| s.buf.is_empty() && !s.has_more())
	}

	/// Up to `want` more matching posts. Fewer (even none) means the request
	/// budget ran out first; check `done`.
	pub fn next(&mut self, want: usize, store: &Mutex<Store>) -> Result<Vec<Post>> {
		let mut out = Vec::new();
		// Saved posts join as one more stream, so they merge newest first with
		// what the site sends instead of all coming ahead of it.
		if !self.local_done && self.wants_saved() {
			self.local_done = true;
			let saved = {
				let s = store.lock().unwrap();
				if self.favorites_only { s.favorite_posts(self.site.id())? } else { s.linked_posts(self.site.id())? }
			};
			self.streams.push(Stream { query: String::new(), started: true, next: None, buf: saved.into() });
		}

		let page_size = self.site.caps().page_size;
		let mut requests = 0;
		'outer: while out.len() < want {
			// Every stream with more to give needs a post buffered before the
			// newest can be picked.
			for s in &mut self.streams {
				if s.buf.is_empty() && s.has_more() {
					if requests >= MAX_REQUESTS {
						break 'outer;
					}
					requests += 1;
					let page = self.site.search(&s.query, s.next, page_size)?;
					s.started = true;
					s.next = page.next;
					{
						let s = store.lock().unwrap();
						s.save_posts(&page.posts)?;
						// Before the check, so searches for links see filled-in ones too.
						s.auto_link(&page.posts)?;
					}
					s.buf.extend(page.posts);
				}
			}
			let pick = if self.plan.ordered {
				self.streams.iter().position(|s| !s.buf.is_empty())
			} else {
				// Newest first; on the same post, the site's fresh copy beats an old saved
				// one (which may be from before you logged in, without images).
				self.streams
					.iter()
					.enumerate()
					.filter_map(|(i, s)| s.buf.front().map(|p| (p.id, std::cmp::Reverse(i))))
					.max()
					.map(|(_, std::cmp::Reverse(i))| i)
			};
			let Some(i) = pick else { break };
			let post = self.streams[i].buf.pop_front().unwrap();
			if !self.seen.insert(post.id) {
				continue;
			}
			self.checked += 1;
			let keep = {
				let s = store.lock().unwrap();
				self.plan.filter.matches(&s.subject(&post)?)
			};
			if keep {
				out.push(post);
			}
		}
		Ok(out)
	}
}

/// The ANDed parts of a search, flattened.
fn conjuncts_of(e: &Expr) -> Vec<&Expr> {
	match e {
		Expr::And(es) => es.iter().flat_map(conjuncts_of).collect(),
		e => vec![e],
	}
}

/// Ways a site may tag a verb: eat, eating; ride, riding; hug, hugging.
fn verb_forms(verb: &str) -> Vec<String> {
	let mut out = vec![verb.to_string()];
	if verb.ends_with("ing") || verb.len() < 2 {
		return out;
	}
	let chars: Vec<char> = verb.chars().collect();
	let vowel = |c: char| "aeiou".contains(c);
	let n = chars.len();
	if verb.ends_with('e') && !verb.ends_with("ee") {
		out.push(format!("{}ing", &verb[..verb.len() - 1]));
	}
	// Short verbs ending consonant-vowel-consonant double the last letter.
	if n >= 3 && !vowel(chars[n - 1]) && !"wxy".contains(chars[n - 1]) && vowel(chars[n - 2]) && !vowel(chars[n - 3]) {
		out.push(format!("{verb}{}ing", chars[n - 1]));
	}
	out.push(format!("{verb}ing"));
	out
}

/// The site's tag for a verb: whichever form (after the site's aliases) has
/// the most posts. Two requests; `None` if no form has any.
fn site_verb(site: &dyn Site, verb: &str) -> Option<String> {
	let forms = verb_forms(verb);
	let aliases = site.aliases(&forms).unwrap_or_default();
	let mut names: Vec<String> = forms.iter().map(|f| aliases.get(f).cloned().unwrap_or_else(|| f.clone())).collect();
	names.dedup();
	let counts = site.tag_counts(&names).ok()?;
	names.into_iter().filter_map(|n| counts.get(&n).map(|c| (*c, n))).filter(|(c, _)| *c > 0).max().map(|(_, n)| n)
}

/// Swaps a tag for a whole expression.
fn replace_with(e: &Expr, from: &str, to: &Expr) -> Expr {
	match e {
		Expr::Tag(t) if t == from => to.clone(),
		Expr::Not(inner) => Expr::Not(Box::new(replace_with(inner, from, to))),
		Expr::And(es) => Expr::And(es.iter().map(|e| replace_with(e, from, to)).collect()),
		Expr::Or(es) => Expr::Or(es.iter().map(|e| replace_with(e, from, to)).collect()),
		e => e.clone(),
	}
}

fn replace_tag(e: &Expr, from: &str, to: &str) -> Expr {
	match e {
		Expr::Tag(t) if t == from => Expr::Tag(to.to_string()),
		Expr::Not(inner) => Expr::Not(Box::new(replace_tag(inner, from, to))),
		Expr::And(es) => Expr::And(es.iter().map(|e| replace_tag(e, from, to)).collect()),
		Expr::Or(es) => Expr::Or(es.iter().map(|e| replace_tag(e, from, to)).collect()),
		e => e.clone(),
	}
}

/// Every plain tag in a search, negated ones too, for reporting translations.
fn tags_in_all(expr: &Expr) -> Vec<String> {
	let mut out = Vec::new();
	expr.walk(&mut |e| {
		if let Expr::Tag(t) = e
			&& !out.contains(t)
		{
			out.push(t.clone());
		}
	});
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sites::{Capabilities, Page, TagSuggestion};
	use std::collections::BTreeMap;

	/// A site that ignores negated tags, so the local check has to catch them.
	struct Fake {
		posts: Vec<Post>,
		calls: Mutex<Vec<String>>,
	}

	fn post(id: u64, tags: &[&str]) -> Post {
		let mut tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
		tags.sort();
		Post {
			site: "fake".into(),
			id,
			tags,
			categories: BTreeMap::new(),
			rating: Rating::Safe,
			score: id as i64,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: "png".into(),
			md5: None,
			file_url: None,
			sample_url: None,
			preview_url: None,
			page_url: String::new(),
			sources: vec![],
			created_at: String::new(),
		}
	}

	impl Site for Fake {
		fn id(&self) -> &str {
			"fake"
		}
		fn name(&self) -> &str {
			"Fake"
		}
		fn caps(&self) -> Capabilities {
			Capabilities { max_tags: 2, tilde_or: false, max_wildcards: 1, metas: &["rating"], free_metas: &[], page_size: 2 }
		}
		fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
			self.calls.lock().unwrap().push(format!("{query} {cursor:?}"));
			let want: Vec<&str> = query.split_whitespace().filter(|w| !w.contains(':') && !w.starts_with('-')).collect();
			let before = match cursor {
				Some(Cursor::Before(id)) => id,
				_ => u64::MAX,
			};
			let posts: Vec<Post> =
				self.posts.iter().rev().filter(|p| p.id < before && want.iter().all(|w| p.has_tag(w))).take(limit).cloned().collect();
			let next = (posts.len() == limit).then(|| Cursor::Before(posts.last().unwrap().id));
			Ok(Page { posts, next })
		}
		fn tag_counts(&self, _: &[String]) -> Result<HashMap<String, u64>> {
			Ok(HashMap::new())
		}
		fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
			Ok(names.iter().filter(|n| *n == "old").map(|n| (n.clone(), "a".to_string())).collect())
		}
		fn autocomplete(&self, _: &str) -> Result<Vec<TagSuggestion>> {
			Ok(vec![])
		}
		fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>> {
			Ok(vec![])
		}
	}

	fn fake() -> Arc<Fake> {
		Arc::new(Fake {
			posts: vec![post(1, &["a"]), post(2, &["b"]), post(3, &["a", "c"]), post(4, &["b", "c"]), post(5, &["a"]), post(6, &["b", "c"])],
			calls: Mutex::new(vec![]),
		})
	}

	fn ids(posts: &[Post]) -> Vec<u64> {
		posts.iter().map(|p| p.id).collect()
	}

	#[test]
	fn merges_or_sides_newest_first() {
		let site = fake();
		let store = Mutex::new(Store::in_memory().unwrap());
		let mut s = Search::start(site.clone(), "a | b", &Rating::ALL, &store).unwrap();
		assert_eq!(s.plan.queries, vec!["a", "b"]);
		assert_eq!(ids(&s.next(10, &store).unwrap()), vec![6, 5, 4, 3, 2, 1]);
		assert!(s.done());
	}

	#[test]
	fn checks_what_the_site_couldnt_take() {
		let site = fake();
		let store = Mutex::new(Store::in_memory().unwrap());
		// Max 2 tags: `c` and `-a` reach the site; the rest is checked here.
		let mut s = Search::start(site, "c -a -zzz -yyy", &Rating::ALL, &store).unwrap();
		assert_eq!(ids(&s.next(10, &store).unwrap()), vec![6, 4]);
	}

	#[test]
	fn follows_site_aliases() {
		let site = fake();
		let store = Mutex::new(Store::in_memory().unwrap());
		let mut s = Search::start(site, "old", &Rating::ALL, &store).unwrap();
		assert_eq!(s.plan.queries, vec!["a"]);
		assert_eq!(ids(&s.next(10, &store).unwrap()), vec![5, 3, 1]);
	}

	#[test]
	fn link_searches_use_saved_posts() {
		let site = fake();
		let store = Mutex::new(Store::in_memory().unwrap());
		// Seeing the posts saves them.
		let mut s = Search::start(site.clone(), "c", &Rating::ALL, &store).unwrap();
		s.next(10, &store).unwrap();
		{
			let st = store.lock().unwrap();
			let g = st.graph();
			g.vote_tag("fake:post/4", "eat", "local", 1).unwrap();
			g.add_link("fake:post/4", "relation", &["b".into(), "eat".into()], "local", &["b".into(), "c".into()]).unwrap();
		}
		// `eat` is a tag of your own, so the site finds nothing; the saved post still shows.
		let mut s = Search::start(site.clone(), "relation[eat]", &Rating::ALL, &store).unwrap();
		// Your link b · eat is the only one with eat in it, so that's what's asked for.
		assert_eq!(s.plan.queries, vec!["b eat"]);
		assert_eq!(ids(&s.next(10, &store).unwrap()), vec![4]);
		// Mixed with a site tag, it searches the site and checks links here.
		let mut s = Search::start(site, "c [b eat]", &Rating::ALL, &store).unwrap();
		assert!(!s.plan.local);
		assert_eq!(ids(&s.next(10, &store).unwrap()), vec![4]);
	}

	#[test]
	fn ratings() {
		assert_eq!(rating_filter(&[Rating::Safe]).unwrap().unwrap().to_string(), "rating:s");
		assert_eq!(rating_filter(&[Rating::Safe, Rating::Questionable]).unwrap().unwrap().to_string(), "-rating:e");
		assert!(rating_filter(&Rating::ALL).unwrap().is_none());
		assert!(rating_filter(&[]).is_err());
	}

	/// A site whose posts carry exactly the tags searched for (each query gets
	/// its own few posts, newest first, ids rising with every query), and that
	/// renames `dog` to `domestic_dog` like e621.
	#[derive(Default)]
	struct Echo {
		queries: Mutex<Vec<String>>,
		/// Tags profiled, to see what was learned and when.
		profiled: Mutex<Vec<String>>,
	}

	impl Site for Echo {
		fn id(&self) -> &str {
			"echo"
		}
		fn name(&self) -> &str {
			"Echo"
		}
		fn caps(&self) -> Capabilities {
			Capabilities { max_tags: 10, tilde_or: true, max_wildcards: 1, metas: &["rating"], free_metas: &[], page_size: 3 }
		}
		fn search(&self, query: &str, _: Option<Cursor>, _: usize) -> Result<Page> {
			let words: Vec<String> = query.split_whitespace().filter(|w| !w.contains(':')).map(String::from).collect();
			self.queries.lock().unwrap().push(words.join(" "));
			static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100);
			let base = NEXT.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
			let posts = (0..3).rev()
				.map(|n| {
					let mut tags = words.clone();
					tags.sort();
					Post {
						site: "echo".into(),
						id: base + n,
						tags,
						categories: BTreeMap::new(),
						rating: Rating::Safe,
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
				})
				.collect();
			Ok(Page { posts, next: None })
		}
		fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
			// The site tags eating as "eating"; "eat" has no posts.
			Ok(names.iter().filter(|n| *n == "eating").map(|n| (n.clone(), 900)).collect())
		}
		fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
			Ok(names.iter().filter(|n| *n == "dog").map(|n| (n.clone(), "domestic_dog".to_string())).collect())
		}
		fn autocomplete(&self, _: &str) -> Result<Vec<TagSuggestion>> {
			Ok(Vec::new())
		}
		fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>> {
			Ok(Vec::new())
		}
		fn total_posts(&self) -> Result<Option<u64>> {
			Ok(Some(7_000_000))
		}
		/// Rabbits come with carrots, dogs with bones, wolves with neither; eating with food.
		fn tag_profile(&self, tag: &str) -> Result<Option<crate::sites::Profile>> {
			self.profiled.lock().unwrap().push(tag.to_string());
			let counts: &[(&str, u32)] = match tag {
				"rabbit" => &[("rabbit", 600), ("long_ears", 400), ("carrot", 30), ("smile", 200)],
				"domestic_dog" => &[("domestic_dog", 600), ("bone", 40), ("smile", 220)],
				"wolf" => &[("wolf", 600), ("smile", 210)],
				"eating" => &[("eating", 600), ("food", 400), ("carrot", 4), ("bone", 5), ("smile", 250)],
				"food" => &[("food", 600), ("carrot", 20), ("bone", 25)],
				_ => &[],
			};
			let sizes = [
				("rabbit", 60_000),
				("domestic_dog", 90_000),
				("wolf", 50_000),
				("eating", 50_000),
				("food", 120_000),
				("carrot", 4_000),
				("bone", 3_000),
				("long_ears", 80_000),
				("smile", 2_000_000),
			];
			Ok(Some(crate::sites::Profile {
				sampled: 600,
				counts: counts.iter().map(|(t, c)| (t.to_string(), *c)).collect(),
				sizes: sizes.iter().map(|(t, s)| (t.to_string(), *s)).collect(),
			}))
		}
	}

	fn relations_store() -> Mutex<Store> {
		let s = Store::in_memory().unwrap();
		for (g, t) in [("animal", "dog"), ("animal", "wolf"), ("animal", "rabbit"), ("object", "guitar"), ("object", "sword"), ("object", "carrot")] {
			s.set_in_group(g, t, true).unwrap();
		}
		for k in ["holding", "eat"] {
			s.graph().add_link_name(k, &s.actor).unwrap();
		}
		for (k, a, b) in [("holding", "dog", "guitar"), ("holding", "wolf", "sword"), ("eat", "rabbit", "carrot"), ("eat", "dog", "carrot")] {
			s.add_rule(k, &[a.into(), b.into()]).unwrap();
		}
		Mutex::new(s)
	}

	fn run(text: &str, store: &Mutex<Store>) -> (Vec<String>, Vec<Post>) {
		let site = Arc::new(Echo::default());
		let mut s = Search::start(site.clone(), text, &Rating::ALL, store).unwrap();
		let posts = s.next(50, store).unwrap();
		(sorted_queries(&site), posts)
	}

	/// The queries a site got, each with its words sorted, in order.
	fn sorted_queries(site: &Echo) -> Vec<String> {
		let mut queries: Vec<String> = site
			.queries
			.lock()
			.unwrap()
			.iter()
			.map(|q| {
				let mut w: Vec<&str> = q.split_whitespace().collect();
				w.sort_unstable();
				w.join(" ")
			})
			.collect();
		queries.sort();
		queries
	}

	#[test]
	fn relations_between_groups_follow_site_names() {
		let store = relations_store();
		// Only related pairs go to the site, `dog` under the site's own name,
		// and every post that comes back has the relation filled in.
		let (queries, posts) = run("holding[@animal @object]", &store);
		assert_eq!(queries, vec!["domestic_dog guitar", "sword wolf"]);
		assert_eq!(posts.len(), 6);
		let s = store.lock().unwrap();
		for p in &posts {
			let links = s.graph().item_links(&p.key(), &s.actor).unwrap();
			assert!(links.iter().any(|l| l.kind == "holding" && l.score > 0), "no link on {:?}", p.tags);
		}
	}

	/// A store with an animal group and nothing related yet.
	fn index_store() -> Mutex<Store> {
		let s = Store::in_memory().unwrap();
		for t in ["dog", "rabbit", "wolf"] {
			s.set_in_group("animal", t, true).unwrap();
		}
		Mutex::new(s)
	}

	#[test]
	fn plain_words_are_plain_tags() {
		// "rabbit eat" is a rabbit eating anything, even with relations around.
		let store = relations_store();
		assert_eq!(run("rabbit eat", &store).0, vec!["eat rabbit"]);
		assert_eq!(run("dog holding solo", &store).0, vec!["domestic_dog holding solo"]);
	}

	#[test]
	fn relation_learns_once_then_uses_the_index() {
		let store = index_store();
		let site = Arc::new(Echo::default());
		// The first ask learns: rabbit ∧ eating ∧ carrot, under the site's tags.
		let mut s = Search::start(site.clone(), "rabbit eat relation", &Rating::ALL, &store).unwrap();
		s.next(10, &store).unwrap();
		assert_eq!(sorted_queries(&site), vec!["carrot eating rabbit"]);
		assert!(s.plan.notes.iter().any(|n| n.contains("what “rabbit” does “eat” with: carrot")), "{:?}", s.plan.notes);
		let a = s.asked.clone().unwrap();
		assert_eq!((a.subject.as_str(), a.verb.as_str(), a.verb_tag.as_str()), ("rabbit", "eat", "eating"));
		assert_eq!(a.pairs[0].1.iter().map(|e| e.object.as_str()).collect::<Vec<_>>(), vec!["carrot"]);
		let profiled = site.profiled.lock().unwrap().len();
		assert!(profiled >= 3);
		// Any word order and placeholder; the second time nothing is learned again.
		for q in ["rabbit relation eat", "rabbit eat link", "rabbit eat @what"] {
			assert_eq!(run(q, &store).0, vec!["carrot eating rabbit"], "{q}");
		}
		let again = Arc::new(Echo::default());
		Search::start(again.clone(), "rabbit eat relation", &Rating::ALL, &store).unwrap();
		assert!(again.profiled.lock().unwrap().is_empty());
	}

	#[test]
	fn a_group_pairs_each_member_with_its_own() {
		let store = index_store();
		// Dogs with bones, rabbits with carrots; never rabbits with bones. Wolves
		// eat nothing the samples show, so they drop out.
		let (queries, posts) = run("@animal eat relation", &store);
		assert_eq!(queries, vec!["bone domestic_dog eating", "carrot eating rabbit"]);
		assert!(posts.iter().all(|p| !(p.has_tag("rabbit") && p.has_tag("bone"))));
		let s = store.lock().unwrap();
		assert_eq!(s.relations("wolf", "eat", true).unwrap(), Vec::new());
		assert!(s.has_learned("wolf", "eat").unwrap());
	}

	#[test]
	fn rejected_relations_stay_out() {
		let store = index_store();
		run("rabbit eat relation", &store);
		store.lock().unwrap().set_relation("rabbit", "eat", "carrot", crate::store::RelationState::Rejected).unwrap();
		let site = Arc::new(Echo::default());
		let err = Search::start(site.clone(), "rabbit eat relation", &Rating::ALL, &store).err().unwrap().to_string();
		assert!(err.contains("Nothing is known yet that rabbit does “eat” with"), "{err}");
		// Not learned again behind your back.
		assert!(site.profiled.lock().unwrap().is_empty());
		// Keeping one you add yourself works without learning.
		store.lock().unwrap().set_relation("rabbit", "eat", "lettuce", crate::store::RelationState::Kept).unwrap();
		assert_eq!(run("rabbit eat relation", &store).0, vec!["eating lettuce rabbit"]);
	}

	#[test]
	fn relations_fill_in_under_other_names() {
		let store = relations_store();
		// A plain search finds dog-with-guitar posts tagged the site's way; the
		// relation (written with "dog") still fills in on them.
		let (_, posts) = run("dog guitar", &store);
		assert_eq!(posts[0].tags, vec!["domestic_dog", "guitar"]);
		let s = store.lock().unwrap();
		let links = s.graph().item_links(&posts[0].key(), &s.actor).unwrap();
		assert_eq!(links.iter().map(|l| (l.kind.as_str(), l.members.clone())).collect::<Vec<_>>(), vec![(
			"holding",
			vec!["domestic_dog".to_string(), "guitar".into()]
		)]);
	}


	#[test]
	fn saved_posts_merge_in_by_age() {
		let store = relations_store();
		// Earlier searches saved posts with filled-in links (ids 10..). A later
		// search for the same relation must not dump them all ahead of new ones,
		// nor return more than asked for.
		run("holding[dog]", &store);
		run("holding[wolf]", &store);
		let site = Arc::new(Echo::default());
		let mut s = Search::start(site, "holding[@animal @object]", &Rating::ALL, &store).unwrap();
		let first = s.next(4, &store).unwrap();
		assert_eq!(first.len(), 4);
		let ids: Vec<u64> = first.iter().map(|p| p.id).collect();
		let mut sorted = ids.clone();
		sorted.sort_unstable_by(|a, b| b.cmp(a));
		assert_eq!(ids, sorted, "not newest first: {ids:?}");
		// The rest still comes, without repeats.
		let mut all = ids;
		while !s.done() {
			all.extend(s.next(4, &store).unwrap().iter().map(|p| p.id));
		}
		let n = all.len();
		all.sort_unstable();
		all.dedup();
		assert_eq!(all.len(), n);
	}


	#[test]
	fn favorites_stay_on_this_computer() {
		let store = index_store();
		// Two posts seen, one favorited.
		let (_, posts) = run("fox smile", &store);
		store.lock().unwrap().set_favorite(&posts[1], true).unwrap();
		let site = Arc::new(Echo::default());
		let mut s = Search::start(site.clone(), "favorited:yes", &Rating::ALL, &store).unwrap();
		let got = s.next(10, &store).unwrap();
		assert_eq!(got.iter().map(|p| p.id).collect::<Vec<_>>(), vec![posts[1].id]);
		// The site was never asked, and favorites aren't pruned.
		assert!(site.queries.lock().unwrap().is_empty());
		store.lock().unwrap().prune_posts().unwrap();
		assert!(store.lock().unwrap().post(&posts[1].key()).unwrap().is_some());
		// Narrowing favorites by tag, and leaving them out of a site search.
		let mut s = Search::start(Arc::new(Echo::default()), "favorited:yes wolf", &Rating::ALL, &store).unwrap();
		assert!(s.next(10, &store).unwrap().is_empty());
		let (_, rest) = run("fox smile favorited:no", &store);
		assert!(rest.iter().all(|p| p.id != posts[1].id || p.key() != posts[1].key()));
		store.lock().unwrap().set_favorite(&posts[1], false).unwrap();
		assert!(!store.lock().unwrap().is_favorite(&posts[1].key()).unwrap());
	}

}
