// One search on several sites at once. Each site gets its own `Search` (with
// its own tag translations, aliases and plan); pages are taken from each in
// turn, and a post already shown from another site (same MD5) is dropped.
//
// Ids and dates don't line up across sites, so the merge takes turns rather
// than sorting: newest first within each site, interleaved.
//
// Sites are asked at the same time, one thread each, so a search waits on the
// slowest site rather than on all of them in a row. Each site keeps its own
// spacing between requests.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::post::{Post, Rating};
use crate::query::parse;
use crate::search::{AskedHere, Search};
use crate::sites::Site;
use crate::store::Store;
use crate::{Error, Result};

pub struct MultiSearch {
	searches: Vec<Search>,
	/// Sites that failed, as "Site: why", shown with the notes.
	pub problems: Vec<String>,
	md5s: HashSet<String>,
	/// Posts dropped because another site had the same file.
	pub repeats: usize,
}

impl MultiSearch {
	pub fn start(sites: &[Arc<dyn Site>], text: &str, ratings: &[Rating], store: &Mutex<Store>) -> Result<MultiSearch> {
		if sites.is_empty() {
			return Err(Error::Query("Pick at least one site.".into()));
		}
		let mut problems = Vec::new();
		// A site without any of the ratings asked for (e926 with Safe unticked) can't find anything.
		let (sites, skipped): (Vec<_>, Vec<_>) = sites.iter().partition(|s| s.only_ratings().is_none_or(|only| only.iter().any(|r| ratings.contains(r))));
		for s in &skipped {
			let has = s.only_ratings().unwrap_or_default().iter().map(|r| r.label()).collect::<Vec<_>>().join(" and ");
			problems.push(format!("{}: it only has {has} posts, and those aren't ticked.", s.name()));
		}
		if sites.is_empty() {
			return Err(Error::Query(format!("None of the ticked sites has posts with the ratings ticked. {}", problems.join(" "))));
		}
		let several = sites.len() > 1;
		// `rabbit eat relation` may learn on the first site what the others then
		// use from the index, so those start in turn.
		let learns = !store.lock().unwrap().read_relations(&parse(text)?)?.asked.is_empty();
		let start = |site: &Arc<dyn Site>| Search::start_with(site.clone(), text, ratings, store, several);
		let started: Vec<Result<Search>> = if learns || !several {
			sites.iter().map(|s| start(s)).collect()
		} else {
			std::thread::scope(|scope| {
				let jobs: Vec<_> = sites.iter().map(|s| scope.spawn(|| start(s))).collect();
				jobs.into_iter().map(|j| j.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
			})
		};
		let mut searches = Vec::new();
		for (site, started) in sites.iter().zip(started) {
			match started {
				Ok(s) => searches.push(s),
				// A search that doesn't read fails the same way everywhere.
				Err(e @ Error::Query(_)) => return Err(e),
				Err(e) if !several => return Err(e),
				Err(e) => problems.push(format!("{}: {e}", site.name())),
			}
		}
		if searches.is_empty() {
			return Err(Error::Query(format!("No site could run this search. {}", problems.join(" "))));
		}
		Ok(MultiSearch { searches, problems, md5s: HashSet::new(), repeats: 0 })
	}

	fn several(&self) -> bool {
		self.searches.len() + self.problems.len() > 1
	}

	/// What goes to each site, with its name in front when there are several.
	pub fn queries(&self) -> Vec<String> {
		let mut out = Vec::new();
		for s in &self.searches {
			for q in &s.plan.queries {
				out.push(if self.several() { format!("{}: {}", s.site().name(), q) } else { q.clone() });
			}
		}
		out
	}

	pub fn notes(&self) -> Vec<String> {
		let mut out = Vec::new();
		for s in &self.searches {
			for n in &s.plan.notes {
				let named = !self.several() || n.contains(s.site().name());
				out.push(if named { n.clone() } else { format!("{}: {n}", s.site().name()) });
			}
		}
		if self.several() {
			out.push("Results take turns between sites; the same file found on two sites shows once.".into());
		}
		out.extend(self.problems.iter().map(|p| format!("Skipped {p}")));
		out
	}

	/// Relations asked about with `@relation`, with the site each was asked on.
	pub fn asked(&self) -> Vec<(Arc<dyn Site>, AskedHere)> {
		self.searches.iter().filter_map(|s| s.asked.clone().map(|a| (s.site().clone(), a))).collect()
	}

	fn shows_images(&self, site: &str) -> bool {
		self.searches.iter().find(|s| s.site().id() == site).is_none_or(|s| s.site().shows_images())
	}

	/// Where a site sits in the list, for keeping the preferred copy of a repeat.
	fn site_rank(&self, site: &str) -> usize {
		self.searches.iter().position(|s| s.site().id() == site).unwrap_or(usize::MAX)
	}

	pub fn done(&self) -> bool {
		self.searches.iter().all(Search::done)
	}

	pub fn checked(&self) -> usize {
		self.searches.iter().map(|s| s.checked).sum()
	}

	/// Up to about `want` more posts, shared out between the sites still going.
	/// A site that fails twice in a row is dropped (and named in `problems`)
	/// while others carry on; with one site, its error comes back as is.
	pub fn next(&mut self, want: usize, store: &Mutex<Store>) -> Result<Vec<Post>> {
		let active = self.searches.iter().filter(|s| !s.done()).count();
		if active == 0 {
			return Ok(Vec::new());
		}
		let share = want.div_ceil(active);
		let several = self.several();
		let found = each(&mut self.searches, |s| if s.done() { None } else { Some(s.next(share, store)) });
		let mut batches = Vec::new();
		let mut failed = Vec::new();
		let mut errors = Vec::new();
		for (i, found) in found.into_iter().enumerate() {
			match found {
				None => {}
				Some(Ok(posts)) => {
					self.searches[i].strikes = 0;
					batches.push(posts.into_iter());
				}
				Some(Err(e)) if !several => return Err(e),
				// One timeout or busy answer isn't the end of a site: it's asked
				// again next time, and only dropped if that fails too.
				Some(Err(e)) if self.searches[i].strikes == 0 => {
					self.searches[i].strikes = 1;
					errors.push(format!("{}: {e}", self.searches[i].site().name()));
				}
				Some(Err(e)) => {
					self.problems.push(format!("{}: {e}", self.searches[i].site().name()));
					failed.push(i);
				}
			}
		}
		for i in failed.into_iter().rev() {
			self.searches.remove(i);
		}
		if self.searches.is_empty() {
			return Err(Error::Query(format!("Every site failed. {}", self.problems.join(" "))));
		}
		// Nothing came back and every site still going failed: say so now, though
		// they all get another try.
		if batches.is_empty() && !errors.is_empty() && self.searches.iter().all(|s| s.done() || s.strikes > 0) {
			return Err(Error::Query(format!("Every site failed; trying again may work. {}", errors.join(" "))));
		}

		let mut out = Vec::new();
		loop {
			let mut any = false;
			for b in &mut batches {
				let Some(post) = b.next() else { continue };
				any = true;
				if let Some(md5) = &post.md5 {
					let md5 = md5.to_lowercase();
					if !self.md5s.insert(md5.clone()) {
						self.repeats += 1;
						// Keep the copy that can show its image; between equals,
						// the site earlier in the list.
						if let Some(kept) = out.iter_mut().find(|p: &&mut Post| p.md5.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(&md5)))
						{
							let better = match (self.shows_images(&kept.site), self.shows_images(&post.site)) {
								(false, true) => true,
								(true, false) => false,
								_ => self.site_rank(&post.site) < self.site_rank(&kept.site),
							};
							if better {
								*kept = post;
							}
						}
						continue;
					}
				}
				out.push(post);
			}
			if !any {
				break;
			}
		}
		Ok(out)
	}

	/// Fetches a page ahead on each site still going, all at once, so the next
	/// `next` of about `want` posts needn't wait. Failures wait for `next`.
	pub fn prefetch(&mut self, want: usize, store: &Mutex<Store>) {
		let active = self.searches.iter().filter(|s| !s.done()).count();
		if active == 0 {
			return;
		}
		let share = want.div_ceil(active);
		each(&mut self.searches, |s| {
			if !s.done() {
				s.prefetch(share, store);
			}
		});
	}
}

/// `f` on every search, each on its own thread (one search needs none),
/// answers in the searches' order.
fn each<T: Send>(searches: &mut [Search], f: impl Fn(&mut Search) -> T + Sync) -> Vec<T> {
	if let [one] = searches {
		return vec![f(one)];
	}
	let f = &f;
	std::thread::scope(|scope| {
		let jobs: Vec<_> = searches.iter_mut().map(|s| scope.spawn(move || f(s))).collect();
		jobs.into_iter().map(|j| j.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sites::{Capabilities, Cursor, Page, TagSuggestion};
	use std::collections::{BTreeMap, HashMap};

	/// Answers every search with its posts, all tagged `fox`, and records queries.
	struct Fake {
		id: &'static str,
		posts: Vec<(u64, &'static str)>,
		queries: Mutex<Vec<String>>,
	}

	impl Site for Fake {
		fn id(&self) -> &str {
			self.id
		}
		fn name(&self) -> &str {
			self.id
		}
		fn base(&self) -> &str {
			"https://example.invalid"
		}
		fn caps(&self) -> Capabilities {
			Capabilities { max_tags: 10, tilde_or: true, max_wildcards: 1, metas: &["rating"], free_metas: &[], page_size: 10 }
		}
		fn search(&self, query: &str, _: Option<Cursor>, _: usize) -> Result<Page> {
			self.queries.lock().unwrap().push(query.to_string());
			let posts = self
				.posts
				.iter()
				.map(|(id, md5)| Post {
					site: self.id.into(),
					id: *id,
					tags: query.split_whitespace().filter(|w| !w.contains(':') && !w.starts_with('-')).map(String::from).collect(),
					categories: BTreeMap::new(),
					rating: Rating::Safe,
					score: 0,
					fav_count: 0,
					width: 1,
					height: 1,
					ext: "png".into(),
					md5: Some(md5.to_string()),
					file_url: None,
					sample_url: None,
					preview_url: None,
					page_url: String::new(),
					sources: Vec::new(),
					created_at: String::new(),
				})
				.collect();
			Ok(Page { posts, next: None })
		}
		fn tag_counts(&self, names: &[String]) -> Result<HashMap<String, u64>> {
			Ok(names.iter().map(|n| (n.clone(), 5)).collect())
		}
		fn aliases(&self, _: &[String]) -> Result<HashMap<String, String>> {
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
	fn takes_turns_drops_repeats_and_translates() {
		let a = Arc::new(Fake { id: "a", posts: vec![(3, "x"), (2, "y")], queries: Mutex::new(Vec::new()) });
		let b = Arc::new(Fake { id: "b", posts: vec![(90, "Y"), (80, "z")], queries: Mutex::new(Vec::new()) });
		let store = Mutex::new(Store::in_memory().unwrap());
		store.lock().unwrap().set_translation("b", "fox", "fox_ears").unwrap();
		let sites: Vec<Arc<dyn Site>> = vec![a.clone(), b.clone()];
		let mut m = MultiSearch::start(&sites, "fox", &[Rating::Safe], &store).unwrap();
		assert_eq!(m.queries(), vec!["a: rating:s fox", "b: rating:s fox_ears"]);
		assert!(m.notes().iter().any(|n| n.contains("“fox” is searched as “fox_ears” on b")));
		let got: Vec<(String, u64)> = m.next(10, &store).unwrap().into_iter().map(|p| (p.site, p.id)).collect();
		// Turns: a3, b90, then a2 (same file as b90, case aside: dropped, and a
		// is earlier in the list so its copy wins the slot), b80.
		assert_eq!(got, vec![("a".to_string(), 3), ("a".into(), 2), ("b".into(), 80)]);
		assert_eq!(m.repeats, 1);
		assert!(m.done());
	}


	#[test]
	fn skips_sites_without_the_ratings() {
		struct Only(Fake);
		impl Site for Only {
			fn id(&self) -> &str {
				"safe"
			}
			fn name(&self) -> &str {
				"safe"
			}
		fn base(&self) -> &str {
			"https://example.invalid"
		}
			fn caps(&self) -> Capabilities {
				self.0.caps()
			}
			fn search(&self, q: &str, c: Option<Cursor>, l: usize) -> Result<Page> {
				self.0.search(q, c, l)
			}
			fn tag_counts(&self, n: &[String]) -> Result<HashMap<String, u64>> {
				self.0.tag_counts(n)
			}
			fn aliases(&self, n: &[String]) -> Result<HashMap<String, String>> {
				self.0.aliases(n)
			}
			fn autocomplete(&self, p: &str) -> Result<Vec<TagSuggestion>> {
				self.0.autocomplete(p)
			}
			fn fetch_bytes(&self, u: &str) -> Result<Vec<u8>> {
				self.0.fetch_bytes(u)
			}
			fn only_ratings(&self) -> Option<&'static [Rating]> {
				Some(&[Rating::Safe])
			}
		}
		let safe = Arc::new(Only(Fake { id: "safe", posts: vec![(1, "a")], queries: Mutex::new(Vec::new()) }));
		let any = Arc::new(Fake { id: "any", posts: vec![(2, "b")], queries: Mutex::new(Vec::new()) });
		let store = Mutex::new(Store::in_memory().unwrap());
		let sites: Vec<Arc<dyn Site>> = vec![safe.clone(), any.clone()];
		let m = MultiSearch::start(&sites, "fox", &[Rating::Explicit], &store).unwrap();
		assert_eq!(m.queries(), vec!["any: rating:e fox"]);
		assert!(m.notes().iter().any(|n| n.contains("safe: it only has Safe posts")), "{:?}", m.notes());
		assert!(safe.0.queries.lock().unwrap().is_empty());
		// Only that site, nothing to search.
		let only: Vec<Arc<dyn Site>> = vec![safe];
		assert!(MultiSearch::start(&only, "fox", &[Rating::Explicit], &store).is_err());
	}


	#[test]
	fn keeps_the_copy_that_shows_its_image() {
		// Same file on both: the Gelbooru-like site (images refused) comes first in turns.
		struct NoImages(Fake);
		impl Site for NoImages {
			fn id(&self) -> &str {
				self.0.id
			}
			fn name(&self) -> &str {
				self.0.id
			}
		fn base(&self) -> &str {
			"https://example.invalid"
		}
			fn caps(&self) -> Capabilities {
				self.0.caps()
			}
			fn search(&self, q: &str, c: Option<Cursor>, l: usize) -> Result<Page> {
				self.0.search(q, c, l)
			}
			fn tag_counts(&self, n: &[String]) -> Result<HashMap<String, u64>> {
				self.0.tag_counts(n)
			}
			fn aliases(&self, n: &[String]) -> Result<HashMap<String, String>> {
				self.0.aliases(n)
			}
			fn autocomplete(&self, p: &str) -> Result<Vec<TagSuggestion>> {
				self.0.autocomplete(p)
			}
			fn fetch_bytes(&self, u: &str) -> Result<Vec<u8>> {
				self.0.fetch_bytes(u)
			}
			fn shows_images(&self) -> bool {
				false
			}
		}
		let gel = Arc::new(NoImages(Fake { id: "gel", posts: vec![(7, "same")], queries: Mutex::new(Vec::new()) }));
		let dan = Arc::new(Fake { id: "dan", posts: vec![(9, "SAME")], queries: Mutex::new(Vec::new()) });
		let store = Mutex::new(Store::in_memory().unwrap());
		let sites: Vec<Arc<dyn Site>> = vec![gel, dan];
		let mut m = MultiSearch::start(&sites, "fox", &[Rating::Safe], &store).unwrap();
		let got: Vec<(String, u64)> = m.next(10, &store).unwrap().into_iter().map(|p| (p.site, p.id)).collect();
		assert_eq!(got, vec![("dan".to_string(), 9)]);
		assert_eq!(m.repeats, 1);
	}

}
