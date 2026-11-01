// Runs a plan: pages through each query, merges them newest first, drops
// repeats and keeps the posts that pass the whole search.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::plan::{Plan, apply_aliases, apply_translations, plan, tags_in};
use crate::post::{Post, Rating};
use crate::query::{Expr, Subject, parse};
use crate::sites::{Cursor, Site};
use crate::learn::learn;
use crate::store::{Asked, RelationEntry, Store};
use crate::{Error, Result};

/// Most site requests one `next` call may make, so a search that matches
/// little doesn't run forever. Calling `next` again carries on.
const MAX_REQUESTS: usize = 8;
/// Fewest posts one request asks for. A request asks for about what's still
/// wanted, not the site's whole page: e621 takes three times as long to send
/// 320 posts as 60, and the rest would only wait in the buffer.
const MIN_LIMIT: usize = 20;

struct Stream {
	query: String,
	started: bool,
	next: Option<Cursor>,
	buf: VecDeque<Post>,
	/// The size of the first page, which numbered pages have to keep.
	limit: usize,
	/// Tags the query asked the site for. The site matched them, under
	/// whatever name it gives each tag now, so they count as on its posts.
	vouched: Vec<String>,
}

impl Stream {
	fn new(query: String, filter: &Expr) -> Stream {
		let tags = tags_in_all(filter);
		let vouched =
			query.split_whitespace().filter(|w| !w.starts_with(['-', '~']) && !w.contains('*') && tags.iter().any(|t| t == w)).map(String::from).collect();
		Stream { query, started: false, next: None, buf: VecDeque::new(), limit: 0, vouched }
	}
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
	/// Posts the site has sent so far.
	fetched: usize,
	/// The empty-result note is added once, not every page.
	named_missing: bool,

}

/// A post with its query's vouched tags counted as on it. Without this, a
/// site without an alias list loses every post here: Safebooru answers
/// `golden_eyes` with posts tagged `amber_eyes`, which the check would drop.
struct Vouched<'a> {
	subject: &'a dyn Subject,
	tags: &'a [String],
}

impl Subject for Vouched<'_> {
	fn has_tag(&self, tag: &str) -> bool {
		self.tags.iter().any(|t| t == tag) || self.subject.has_tag(tag)
	}
	fn meta(&self, key: &str, value: &str) -> Option<bool> {
		self.subject.meta(key, value)
	}
	fn linked(&self, kind: Option<&str>, tags: &[String]) -> bool {
		self.subject.linked(kind, tags)
	}
	fn tags(&self) -> Box<dyn Iterator<Item = &str> + '_> {
		self.subject.tags()
	}
}

/// Post counts for these tags, asking the site only for the ones not looked
/// up this run. An error only when the site was asked and failed.
fn tag_counts(site: &dyn Site, store: &Mutex<Store>, names: &[String]) -> Result<HashMap<String, u64>> {
	let mut out = HashMap::new();
	let unknown: Vec<String> = {
		let s = store.lock().unwrap();
		names
			.iter()
			.filter(|n| !n.contains('*'))
			.filter(|n| match s.cached_count(site.id(), n) {
				Some(c) => {
					out.insert((*n).clone(), c);
					false
				}
				None => true,
			})
			.cloned()
			.collect()
	};
	if !unknown.is_empty() {
		let found = site.tag_counts(&unknown)?;
		let mut s = store.lock().unwrap();
		// Only what came back: some sites look up a few names at most, so a
		// missing one may just not have been asked.
		for (n, c) in &found {
			s.remember_count(site.id(), n, *c);
		}
		out.extend(found);
	}
	Ok(out)
}

/// The search for the ratings ticked: one is `rating:x`, two is `-rating:y`, all three adds nothing.
/// Adjacent plain-word pairs that could be one underscored tag.
fn spaced_pairs(expr: &Expr) -> Vec<String> {
	fn walk(e: &Expr, out: &mut Vec<String>) {
		match e {
			Expr::And(list) => {
				for pair in list.windows(2) {
					if let (Expr::Tag(a), Expr::Tag(b)) = (&pair[0], &pair[1])
						&& plain(a)
						&& plain(b)
					{
						out.push(format!("{a}_{b}"));
					}
				}
				list.iter().for_each(|e| walk(e, out));
			}
			Expr::Or(list) => list.iter().for_each(|e| walk(e, out)),
			Expr::Not(e) => walk(e, out),
			_ => {}
		}
	}
	let mut out = Vec::new();
	walk(expr, &mut out);
	out
}

fn plain(t: &str) -> bool {
	!t.is_empty() && !t.contains(['_', '*', ':', '(', ')', '~'])
}

/// Rewrites pairs whose joined form the site has with at least `least` posts.
fn join_tags(expr: Expr, counts: &HashMap<String, u64>, least: u64) -> (Expr, Vec<String>) {
	fn go(e: Expr, counts: &HashMap<String, u64>, least: u64, made: &mut Vec<String>) -> Expr {
		match e {
			Expr::And(list) => {
				let list: Vec<Expr> = list.into_iter().map(|e| go(e, counts, least, made)).collect();
				let mut out: Vec<Expr> = Vec::with_capacity(list.len());
				let mut i = 0;
				while i < list.len() {
					if i + 1 < list.len()
						&& let (Expr::Tag(a), Expr::Tag(b)) = (&list[i], &list[i + 1])
					{
						let joined = format!("{a}_{b}");
						if plain(a) && plain(b) && counts.get(&joined).is_some_and(|n| *n >= least) {
							made.push(joined.clone());
							out.push(Expr::Tag(joined));
							i += 2;
							continue;
						}
					}
					out.push(list[i].clone());
					i += 1;
				}
				Expr::And(out)
			}
			Expr::Or(list) => Expr::Or(list.into_iter().map(|e| go(e, counts, least, made)).collect()),
			Expr::Not(inner) => Expr::Not(Box::new(go(*inner, counts, least, made))),
			other => other,
		}
	}
	let mut made = Vec::new();
	(go(expr, counts, least, &mut made), made)
}

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

	/// `check_tags`: if the site sends nothing, look up which tags it doesn't
	/// have, which matters most when one search runs on several sites.
	pub fn start_with(site: Arc<dyn Site>, text: &str, ratings: &[Rating], store: &Mutex<Store>, _check_tags: bool) -> Result<Search> {
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
		// "red fox" is nearly always the tag `red_fox`, typed with a space.
		// Only joined names the site actually has are used, and the note says so.
		let pairs = spaced_pairs(&expr);
		if !pairs.is_empty() {
			let counts = tag_counts(site.as_ref(), store, &pairs.iter().take(3).cloned().collect::<Vec<_>>()).unwrap_or_default();
			let (joined, made) = join_tags(expr, &counts, 50);
			for pair in made {
				notes.push(format!("“{}” is searched as “{pair}”.", pair.replace('_', " ")));
			}
			expr = joined;
		}
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
		// The site follows its own aliases, and a post counts as having the tags
		// it was found by. So when the whole search goes to the site as written,
		// the site's names change nothing here and the lookup is skipped (on e621
		// it holds the search up a second). Your links' tags are still looked up.
		let whole = || {
			let e = apply_aliases(&expr, &aliases);
			site_takes_all(&e, &plan(&e, &caps, &HashMap::new()))
		};
		if !unknown.is_empty() && !(unknown.iter().all(|n| asked.contains(n)) && whole()) {
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

		// Counts only change the plan when some tags don't fit: they pick the rarest.
		let first = plan(&expr, &caps, &HashMap::new());
		let mut plan = if first.crowded { plan(&expr, &caps, &tag_counts(site.as_ref(), store, &tags_in(&expr)).unwrap_or_default()) } else { first };
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
			plan.queries.iter().map(|q| Stream::new(q.clone(), &plan.filter)).collect()
		};
		Ok(Search {
			site,
			plan,
			asked: relation,
			favorites_only,
			streams,
			seen: HashSet::new(),
			local_done: false,
			checked: 0,
			fetched: 0,
			named_missing: false,
		})
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
			let saved_posts = {
				let s = store.lock().unwrap();
				if self.favorites_only { s.favorite_posts(self.site.id())? } else { s.linked_posts(self.site.id())? }
			};
			let mut saved = Stream::new(String::new(), &self.plan.filter);
			saved.started = true;
			saved.buf = saved_posts.into();
			self.streams.push(saved);
		}

		let page_size = self.site.caps().page_size;
		let mut requests = 0;
		// Requests made to each stream in this call.
		let mut asked = vec![0; self.streams.len()];
		'outer: while out.len() < want {
			// Every stream with more to give needs a post buffered before the
			// newest can be picked.
			for (i, made) in asked.iter_mut().enumerate() {
				let s = &self.streams[i];
				if s.buf.is_empty() && s.has_more() {
					if requests >= MAX_REQUESTS {
						break 'outer;
					}
					requests += 1;
					// A second request in one call means posts are being dropped
					// here: a full page finds the rest in fewer requests.
					let need = if *made == 0 { want - out.len() } else { page_size };
					*made += 1;
					self.fetch(i, need, store)?;
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
				let subject = s.subject(&post)?;
				self.plan.filter.matches(&Vouched { subject: &subject, tags: &self.streams[i].vouched })
			};
			if keep {
				out.push(post);
			}
		}
		// Nothing came back at all: name the tags the site has no posts for, so a
		// dead tag isn't mistaken for a bad search. Counts are cached, and this
		// only runs on an empty result.
		if self.fetched == 0 && !self.plan.local && self.streams.iter().all(|s| s.started) && !self.named_missing {
			self.named_missing = true;
			self.name_missing_tags(store);
		}
		Ok(out)
	}

	/// Fetches one page ahead for the query with the fewest posts waiting, if
	/// it has fewer than `want`, so the next `next` needn't wait on the site.
	/// A failure is left for `next` to meet and report.
	pub fn prefetch(&mut self, want: usize, store: &Mutex<Store>) {
		let short = (0..self.streams.len())
			.filter(|&i| self.streams[i].started && self.streams[i].has_more() && self.streams[i].buf.len() < want)
			.min_by_key(|&i| self.streams[i].buf.len());
		if let Some(i) = short {
			let _ = self.fetch(i, want, store);
		}
	}

	/// Asks the site for stream `i`'s next page, about `need` posts.
	fn fetch(&mut self, i: usize, need: usize, store: &Mutex<Store>) -> Result<()> {
		let page_size = self.site.caps().page_size;
		let s = &mut self.streams[i];
		let limit = match s.next {
			// Page n only follows on from the pages before it at their size.
			Some(Cursor::Page(_)) => s.limit,
			_ => need.max(MIN_LIMIT).min(page_size),
		};
		let page = self.site.search(&s.query, s.next, limit)?;
		if !s.started {
			s.limit = limit;
		}
		s.started = true;
		s.next = page.next;
		{
			let st = store.lock().unwrap();
			st.save_posts(&page.posts)?;
			// Before the check, so searches for links see filled-in ones too.
			st.auto_link(&page.posts)?;
		}
		let sent = page.posts.len();
		s.buf.extend(page.posts);
		self.fetched += sent;
		Ok(())
	}

	/// The site sent nothing at all: names the tags it has no posts for.
	fn name_missing_tags(&mut self, store: &Mutex<Store>) {
		let names = tags_in(&self.plan.filter);
		if let Ok(counts) = tag_counts(self.site.as_ref(), store, &names) {
			for n in names.iter().filter(|n| counts.get(*n).is_none_or(|c| *c == 0)) {
				self.plan.notes.push(format!("“{n}” has no posts on {}; a tag translation can fix that.", self.site.name()));
			}
		}
	}
}

/// Whether every tag goes to the site as written, in every query, so the site
/// alone decides which posts have them.
fn site_takes_all(expr: &Expr, plan: &Plan) -> bool {
	!plan.crowded
		&& !plan.local
		// Links and favorites are checked against what's saved here, by name.
		&& !expr.any(&|e| matches!(e, Expr::Link { .. }))
		&& expr.meta_value("favorited").is_none()
		&& tags_in_all(expr).iter().all(|t| plan.queries.iter().all(|q| q.split_whitespace().any(|w| w.strip_prefix('-').unwrap_or(w) == t)))
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
		fn base(&self) -> &str {
			"https://example.invalid"
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
		// More tags than the site takes: some are checked here, by the site's names.
		let mut s = Search::start(site, "old -zzz -yyy -xxx", &Rating::ALL, &store).unwrap();
		assert_eq!(s.plan.queries, vec!["a -zzz"]);
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
		fn base(&self) -> &str {
			"https://example.invalid"
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
	fn spaced_words_become_a_tag_when_the_site_has_it() {
		let expr = Expr::And(vec![Expr::Tag("red".into()), Expr::Tag("fox".into())]);
		let counts = HashMap::from([("red_fox".to_string(), 50_000u64)]);
		let (got, made) = join_tags(expr, &counts, 50);
		assert_eq!(format!("{got}"), "red_fox");
		assert_eq!(made, vec!["red_fox"]);
	}

	#[test]
	fn a_rare_join_is_left_alone() {
		let expr = Expr::And(vec![Expr::Tag("red".into()), Expr::Tag("fox".into())]);
		let counts = HashMap::from([("red_fox".to_string(), 3u64)]);
		let (got, made) = join_tags(expr, &counts, 50);
		assert_eq!(format!("{got}"), "red fox");
		assert!(made.is_empty());
	}

	#[test]
	fn underscores_and_wildcards_are_never_joined() {
		assert!(spaced_pairs(&Expr::And(vec![Expr::Tag("red_fox".into()), Expr::Tag("fox".into())])).is_empty());
		assert!(spaced_pairs(&Expr::And(vec![Expr::Tag("red".into()), Expr::Tag("fox*".into())])).is_empty());
		assert_eq!(spaced_pairs(&Expr::And(vec![Expr::Tag("red".into()), Expr::Tag("fox".into())])), vec!["red_fox"]);
	}

	#[test]
	fn join_works_inside_or() {
		let expr = Expr::Or(vec![Expr::And(vec![Expr::Tag("red".into()), Expr::Tag("fox".into())]), Expr::Tag("wolf".into())]);
		let counts = HashMap::from([("red_fox".to_string(), 900u64)]);
		let (got, _) = join_tags(expr, &counts, 50);
		assert_eq!(format!("{got}"), "(red_fox) | wolf");
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


	/// 300 posts, all tagged `new`, which the site also finds as `old` (its
	/// alias) without saying so. Ignores negated tags, so they're checked
	/// here. Records each request's limit and cursor.
	#[derive(Default)]
	struct Renaming {
		requests: Mutex<Vec<(usize, Option<Cursor>)>>,
		counted: Mutex<Vec<String>>,
		aliased: Mutex<Vec<String>>,
	}

	impl Site for Renaming {
		fn id(&self) -> &str {
			"renaming"
		}
		fn name(&self) -> &str {
			"Renaming"
		}
		fn base(&self) -> &str {
			"https://example.invalid"
		}
		fn caps(&self) -> Capabilities {
			Capabilities { max_tags: 6, tilde_or: true, max_wildcards: 1, metas: &["rating", "order"], free_metas: &[], page_size: 100 }
		}
		fn search(&self, query: &str, cursor: Option<Cursor>, limit: usize) -> Result<Page> {
			self.requests.lock().unwrap().push((limit, cursor));
			let words: Vec<&str> = query.split_whitespace().filter(|w| !w.contains(':') && !w.starts_with('-')).collect();
			if !words.iter().all(|w| matches!(*w, "old" | "new")) {
				return Ok(Page { posts: Vec::new(), next: None });
			}
			let ids: Vec<u64> = match cursor {
				Some(Cursor::Before(id)) => (1..id.min(301)).rev().take(limit).collect(),
				Some(Cursor::Page(n)) => (1..=300).rev().skip((n as usize - 1) * limit).take(limit).collect(),
				None => (1..=300).rev().take(limit).collect(),
			};
			let posts: Vec<Post> = ids.iter().map(|&id| Post { site: "renaming".into(), ..post(id, &["new"]) }).collect();
			let ordered = query.contains("order:");
			let next = match (posts.len() == limit, ordered, cursor) {
				(false, _, _) => None,
				(true, false, _) => posts.last().map(|p| Cursor::Before(p.id)),
				(true, true, Some(Cursor::Page(n))) => Some(Cursor::Page(n + 1)),
				(true, true, _) => Some(Cursor::Page(2)),
			};
			Ok(Page { posts, next })
		}
		fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
			self.counted.lock().unwrap().extend(names.iter().cloned());
			Ok(names.iter().filter(|n| *n == "new").map(|n| (n.clone(), 300)).collect())
		}
		fn aliases(&self, names: &[String]) -> Result<HashMap<String, String>> {
			self.aliased.lock().unwrap().extend(names.iter().cloned());
			Ok(HashMap::new())
		}
		fn autocomplete(&self, _: &str) -> Result<Vec<TagSuggestion>> {
			Ok(Vec::new())
		}
		fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>> {
			Ok(Vec::new())
		}
	}

	#[test]
	fn a_tag_the_site_renames_still_counts() {
		// Like Safebooru answering `golden_eyes` with `amber_eyes` posts.
		let site = Arc::new(Renaming::default());
		let store = Mutex::new(Store::in_memory().unwrap());
		let mut s = Search::start(site.clone(), "old", &Rating::ALL, &store).unwrap();
		assert_eq!(s.next(10, &store).unwrap().len(), 10);
		// The whole search went to the site, so its names weren't asked for.
		assert!(site.aliased.lock().unwrap().is_empty());
		// What the site leaves out is still checked here.
		let site = Arc::new(Renaming::default());
		let mut s = Search::start(site.clone(), "old -new", &Rating::ALL, &store).unwrap();
		assert_eq!(s.plan.queries, vec!["old -new"]);
		assert!(s.next(10, &store).unwrap().is_empty());
		assert!(site.requests.lock().unwrap().len() > 1);
	}

	#[test]
	fn asks_for_about_what_is_wanted_and_reads_ahead() {
		let site = Arc::new(Renaming::default());
		let store = Mutex::new(Store::in_memory().unwrap());
		let mut s = Search::start(site.clone(), "new", &Rating::ALL, &store).unwrap();
		assert_eq!(ids(&s.next(30, &store).unwrap()), (271..=300).rev().collect::<Vec<_>>());
		s.prefetch(30, &store);
		assert_eq!(*site.requests.lock().unwrap(), vec![(30, None), (30, Some(Cursor::Before(271)))]);
		// The next page is already here, and nothing is skipped or repeated.
		assert_eq!(ids(&s.next(30, &store).unwrap()), (241..=270).rev().collect::<Vec<_>>());
		assert_eq!(site.requests.lock().unwrap().len(), 2);
		// A few posts wanted still asks for a sensible number.
		s.next(70, &store).unwrap();
		s.next(3, &store).unwrap();
		assert_eq!(site.requests.lock().unwrap()[2..], [(70, Some(Cursor::Before(241))), (20, Some(Cursor::Before(171)))]);
	}

	#[test]
	fn numbered_pages_keep_their_size() {
		let site = Arc::new(Renaming::default());
		let store = Mutex::new(Store::in_memory().unwrap());
		let mut s = Search::start(site.clone(), "new order:score", &Rating::ALL, &store).unwrap();
		let mut all = ids(&s.next(30, &store).unwrap());
		all.extend(ids(&s.next(90, &store).unwrap()));
		assert!(site.requests.lock().unwrap().iter().all(|(limit, _)| *limit == 30));
		assert_eq!(all, (181..=300).rev().collect::<Vec<_>>());
	}

	#[test]
	fn missing_tags_are_named_only_when_nothing_comes() {
		let store = Mutex::new(Store::in_memory().unwrap());
		let site = Arc::new(Renaming::default());
		let mut s = Search::start_with(site.clone(), "new", &Rating::ALL, &store, true).unwrap();
		s.next(10, &store).unwrap();
		assert!(site.counted.lock().unwrap().is_empty());
		let site = Arc::new(Renaming::default());
		let mut s = Search::start_with(site.clone(), "new zzz", &Rating::ALL, &store, true).unwrap();
		assert!(s.next(10, &store).unwrap().is_empty());
		assert!(s.plan.notes.iter().any(|n| n == "“zzz” has no posts on Renaming; a tag translation can fix that."), "{:?}", s.plan.notes);
		assert!(!s.plan.notes.iter().any(|n| n.contains("“new”")));
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
