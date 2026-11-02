// Runs a search against safe-only sites and prints the plan. Several sites
// (comma-separated) search together.
//   cargo run -p bettersearch-core --example live -- "fox (smile | grin) -sketch"
//   cargo run -p bettersearch-core --example live -- "fox_ears (smile | blush) -hat" safebooru
//   cargo run -p bettersearch-core --example live -- "fox_ears smile" e926,safebooru,danbooru
// `SIMILAR=<post id>` shows posts like that one and its other versions instead.
// `TRANSLATE=site:tag=search;…` adds tag translations for the run,
// `TAG_GROUPS=animal:fox,wolf;…` groups and `RELATIONS=holding:fox,sword;…` relations:
//   TAG_GROUPS="animal:fox,wolf;object:sword,guitar" RELATIONS="holding:fox,sword;holding:wolf,guitar" \
//   cargo run -p bettersearch-core --example live -- "holding[@animal @object]"
//   TAG_GROUPS="animal:rabbit,domestic_cat,horse" cargo run -p bettersearch-core --example live -- "@animal eat relation"

use std::sync::{Arc, Mutex};

use bettersearch_core::sites::{Danbooru, E621, Gelbooru, user_agent};
use bettersearch_core::{MultiSearch, Rating, Site, Store};

fn main() -> bettersearch_core::Result<()> {
	let text = std::env::args().nth(1).unwrap_or_else(|| "fox (smile | grin) -sketch".into());
	let ua = user_agent("");
	let mut sites: Vec<Arc<dyn Site>> = Vec::new();
	for id in std::env::args().nth(2).unwrap_or_else(|| "e926".into()).split(',') {
		sites.push(match id {
			"danbooru" => Arc::new(Danbooru::danbooru(&ua)?),
			"safebooru" => Arc::new(Gelbooru::safebooru(&ua)?),
			_ => Arc::new(E621::e926(&ua)?),
		});
	}
	let store = Mutex::new(Store::in_memory()?);
	// `SIMILAR=<post id>`: posts like that one on the first site, then other versions of it.
	if let Ok(id) = std::env::var("SIMILAR") {
		let site = &sites[0];
		let post = site.search(&format!("id:{id}"), None, 1)?.posts.into_iter().next().expect("no such post");
		println!("#{} tags: {}", post.id, post.tags.join(" "));
		let found = bettersearch_core::similar::find(site.as_ref(), &post, &[Rating::Safe], 12)?;
		println!("similar, by {}: query {}", found.by, found.query);
		for p in &found.posts {
			let shared: Vec<&String> = found.tags.iter().filter(|t| p.has_tag(t.trim_start_matches('~'))).collect();
			println!("  #{} shares {:?}", p.id, shared);
		}
		match site.other_versions(post.id)? {
			Some(v) => println!("other versions: {:?}", v.iter().map(|p| p.id).collect::<Vec<_>>()),
			None => println!("other versions: not offered"),
		}
		return Ok(());
	}
	for rule in std::env::var("TRANSLATE").unwrap_or_default().split(';').filter(|r| !r.is_empty()) {
		let (site, rest) = rule.split_once(':').expect("site:tag=search");
		let (tag, target) = rest.split_once('=').expect("site:tag=search");
		store.lock().unwrap().set_translation(site, tag, target)?;
	}
	{
		let s = store.lock().unwrap();
		for rule in std::env::var("TAG_GROUPS").unwrap_or_default().split(';').filter(|r| !r.is_empty()) {
			let (group, tags) = rule.split_once(':').expect("group:tag,tag");
			for t in tags.split(',') {
				s.set_in_group(group, t, true)?;
			}
		}
		for rule in std::env::var("RELATIONS").unwrap_or_default().split(';').filter(|r| !r.is_empty()) {
			let (kind, tags) = rule.split_once(':').expect("kind:tag,tag");
			if !s.graph().link_names()?.contains(&kind.to_string()) {
				s.graph().add_link_name(kind, &s.actor)?;
			}
			s.add_rule(kind, &tags.split(',').map(String::from).collect::<Vec<_>>())?;
		}
	}
	let mut search = MultiSearch::start(&sites, &text, &[Rating::Safe], &store)?;
	for q in search.queries() {
		println!("query:   {q}");
	}
	for n in search.notes() {
		println!("note:    {n}");
	}
	let want: usize = std::env::var("WANT").ok().and_then(|w| w.parse().ok()).unwrap_or(9);
	let posts = search.next(want, &store)?;
	for p in &posts {
		let pick = ["fox", "wolf", "dog", "domestic_dog", "rabbit", "sword", "guitar", "carrot", "eating"];
		let has: Vec<&str> = pick.into_iter().filter(|t| p.has_tag(t)).collect();
		println!("{}#{} {:?} score {} has: {}", p.site, p.id, p.rating, p.score, has.join(" "));
	}
	println!("checked {} posts, {} repeats dropped, done: {}", search.checked(), search.repeats, search.done());
	// For `rabbit eat relation`: what the relation index used (learned on first ask).
	if let Some((_, asked)) = search.asked().into_iter().next() {
		for (subject, entries) in &asked.pairs {
			let objects: Vec<&str> = entries.iter().map(|e| e.object.as_str()).collect();
			println!("index: {subject} {} → {}", asked.verb, objects.join(", "));
		}
	}
	let sugg = sites[0].autocomplete("fox_e")?;
	println!("autocomplete fox_e: {:?}", sugg.iter().take(3).map(|s| (&s.name, s.count)).collect::<Vec<_>>());
	Ok(())
}
