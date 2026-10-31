// Turns a search into what a site can run. Sites only take a flat list of
// tags, with limits, so the planner sends the narrowest part it can and the
// whole search is checked again on every post that comes back:
//
// - tags the site can take go to it, rarest first, up to its tag limit;
// - one OR group made of plain tags rides along as `~a ~b`;
// - an OR of bigger pieces, `(a b) | (c -d)`, becomes one search per side,
//   merged newest first;
// - a link's tags go to the site like plain tags (they're all on the post),
//   and the link itself is checked here;
// - anything else stays local.

use std::collections::HashMap;

use crate::query::{Expr, LOCAL_META_KEYS};
use crate::sites::Capabilities;

/// Most separate searches one OR may split into.
const MAX_BRANCHES: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
	/// Queries in the site's syntax, merged together. "" means the newest posts.
	pub queries: Vec<String>,
	/// The whole search, checked on every post.
	pub filter: Expr,
	/// Things the person should know about how it runs.
	pub notes: Vec<String>,
	/// Nothing narrow to send, but it names links: search posts saved here instead.
	pub local: bool,
	/// Has `order:`, so pages come in the site's order and can't be merged.
	pub ordered: bool,
}

#[derive(Debug, Clone)]
struct Term {
	text: String,
	/// Tags it uses up from the site's limit.
	cost: usize,
	/// Lower goes first.
	rank: (u8, u64),
	tilde: bool,
	wildcard: bool,
	/// Narrows the search, so the site isn't asked for every post.
	narrows: bool,
}

fn conjuncts(e: &Expr) -> Vec<&Expr> {
	match e {
		Expr::And(es) => es.iter().flat_map(conjuncts).collect(),
		e => vec![e],
	}
}

fn plain_tag(e: &Expr) -> Option<&str> {
	match e {
		Expr::Tag(t) if !t.contains('*') => Some(t),
		_ => None,
	}
}

fn term(e: &Expr, caps: &Capabilities, counts: &HashMap<String, u64>) -> Option<Term> {
	let count = |t: &str| counts.get(t).copied().unwrap_or(u64::MAX / 2);
	let t = |text: String, cost, rank, narrows| Term { text, cost, rank, tilde: false, wildcard: false, narrows };
	let meta_cost = |key: &str| usize::from(!caps.free_metas.contains(&key));
	match e {
		Expr::Tag(tag) if tag.contains('*') => Some(Term { wildcard: true, ..t(tag.clone(), 1, (4, 0), true) }),
		Expr::Tag(tag) => Some(t(tag.clone(), 1, (2, count(tag)), true)),
		Expr::Meta { key, value } if caps.metas.contains(&key.as_str()) => {
			let rank = match key.as_str() {
				"order" => 0,
				"rating" => 1,
				_ => 3,
			};
			Some(t(format!("{key}:{value}"), meta_cost(key), (rank, 0), !matches!(key.as_str(), "order" | "rating")))
		}
		Expr::Not(inner) => match inner.as_ref() {
			Expr::Tag(tag) if !tag.contains('*') => Some(t(format!("-{tag}"), 1, (5, u64::MAX - count(tag)), false)),
			Expr::Meta { key, value } if caps.metas.contains(&key.as_str()) && key != "order" => Some(t(format!("-{key}:{value}"), meta_cost(key), (5, 0), false)),
			_ => None,
		},
		Expr::Or(es) if caps.tilde_or && es.len() > 1 && es.iter().all(|e| plain_tag(e).is_some()) => {
			let names: Vec<&str> = es.iter().filter_map(plain_tag).collect();
			let total = names.iter().map(|n| count(n)).fold(0u64, u64::saturating_add);
			Some(Term {
				tilde: true,
				..t(names.iter().map(|n| format!("~{n}")).collect::<Vec<_>>().join(" "), names.len(), (3, total), true)
			})
		}
		_ => None,
	}
}

/// The terms that fit the site's limits, best first, and how many didn't.
fn select(mut terms: Vec<Term>, caps: &Capabilities) -> (Vec<Term>, Vec<String>) {
	terms.sort_by_key(|t| t.rank);
	let (mut used, mut wildcards, mut tilde) = (0, 0, false);
	let mut out = Vec::new();
	let mut dropped = Vec::new();
	for t in terms {
		if used + t.cost > caps.max_tags || (t.tilde && tilde) || (t.wildcard && wildcards >= caps.max_wildcards) {
			dropped.push(t.text);
			continue;
		}
		used += t.cost;
		wildcards += t.wildcard as usize;
		tilde |= t.tilde;
		out.push(t);
	}
	(out, dropped)
}

fn join(terms: &[Term]) -> String {
	terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Plans a search for a site. `counts` (post counts per tag, from
/// `Site::tag_counts`) lets it pick the rarest tags; it works without them.
pub fn plan(expr: &Expr, caps: &Capabilities, counts: &HashMap<String, u64>) -> Plan {
	let site_expr = push_nots(&links_as_tags(expr));
	let parts = conjuncts(&site_expr);
	let ordered = expr.meta_value("order").is_some();
	let mut notes = Vec::new();

	let mut base: Vec<Term> = Vec::new();
	let mut rest = Vec::new();
	for p in &parts {
		match term(p, caps, counts) {
			// Only one `~` group fits a query; later ones can still be split on.
			Some(t) if t.tilde && base.iter().any(|b| b.tilde) => rest.push(*p),
			Some(t) => base.push(t),
			None => rest.push(*p),
		}
	}

	// Split on the OR with the fewest sides, if there's one worth splitting.
	let split = rest
		.iter()
		.filter_map(|e| match e {
			Expr::Or(es) if es.len() <= MAX_BRANCHES => Some(es),
			_ => None,
		})
		.min_by_key(|es| es.len());
	// An OR too big to split can still narrow by the tags every side has:
	// eat[carrot rabbit] | eat[lettuce rabbit] | … sends `rabbit`.
	for e in &rest {
		if let Expr::Or(sides) = e
			&& split.is_none_or(|s| !std::ptr::eq(s, sides))
		{
			for t in common_tags(sides) {
				if let Some(t) = term(&Expr::Tag(t), caps, counts) {
					base.push(t);
				}
			}
		}
	}
	let mut queries = Vec::new();
	let mut dropped: Vec<String> = Vec::new();
	let mut narrow = true;
	match split {
		Some(sides) if !ordered => {
			for side in sides {
				let mut terms = base.clone();
				terms.extend(conjuncts(side).into_iter().filter_map(|e| term(e, caps, counts)));
				let (chosen, d) = select(terms, caps);
				for t in d {
					if !dropped.contains(&t) {
						dropped.push(t);
					}
				}
				narrow &= chosen.iter().any(|t| t.narrows);
				queries.push(join(&chosen));
			}
			queries.dedup();
			if queries.iter().any(|q| q.is_empty()) {
				queries = vec![String::new()];
			}
			if queries.len() > 1 {
				notes.push(format!("Runs as {} searches on the site, merged newest first.", queries.len()));
			}
		}
		_ => {
			if split.is_some() {
				notes.push("With order:, OR groups are checked here instead of split into separate searches.".into());
			}
			let (chosen, d) = select(base, caps);
			dropped = d;
			narrow = chosen.iter().any(|t| t.narrows);
			queries.push(join(&chosen));
		}
	}

	if !dropped.is_empty() {
		let shown: Vec<&str> = dropped.iter().take(12).map(String::as_str).collect();
		let more = if dropped.len() > shown.len() { format!(" and {} more", dropped.len() - shown.len()) } else { String::new() };
		notes.push(format!(
			"The site takes {} tags at a time, so these are checked here instead: {}{more}. Posts with them are fetched and dropped, \
			so the page fills slower; leaving out fewer tags (or rarer ones) helps.",
			caps.max_tags,
			shown.join(" ")
		));
	}

	let mut unknown = Vec::new();
	expr.walk(&mut |e| {
		if let Expr::Meta { key, .. } = e
			&& !caps.metas.contains(&key.as_str())
			&& !LOCAL_META_KEYS.contains(&key.as_str())
			&& !unknown.contains(key)
		{
			unknown.push(key.clone());
		}
	});
	for key in unknown {
		notes.push(format!("`{key}:` isn't something this site understands, so it's ignored."));
	}

	let has_links = expr.any(&|e| matches!(e, Expr::Link { .. }));
	let local = !narrow && has_links;
	if local {
		notes.push("Searching posts you've linked, saved on this computer.".into());
	} else if !narrow && !matches!(expr, Expr::And(es) if es.is_empty()) {
		notes.push("Nothing narrow enough to send to the site, so the newest posts are checked here. This can take a while.".into());
	}

	Plan { queries, filter: expr.clone(), notes, local, ordered }
}

/// Moves NOTs inward so a site can take them: -(a | b) is -a -b, which is how
/// `-@fat` reaches the site (every tag in the group left out), and --a is a.
fn push_nots(e: &Expr) -> Expr {
	match e {
		Expr::Not(inner) => match inner.as_ref() {
			Expr::Or(es) => Expr::And(es.iter().map(|e| push_nots(&Expr::Not(Box::new(e.clone())))).collect()),
			Expr::Not(e) => push_nots(e),
			_ => e.clone(),
		},
		Expr::And(es) => Expr::And(es.iter().map(push_nots).collect()),
		Expr::Or(es) => Expr::Or(es.iter().map(push_nots).collect()),
		e => e.clone(),
	}
}

/// Plain tags every side of an OR asks for.
fn common_tags(sides: &[Expr]) -> Vec<String> {
	let tags = |e: &Expr| -> Vec<String> { conjuncts(e).into_iter().filter_map(plain_tag).map(String::from).collect() };
	let Some((first, rest)) = sides.split_first() else { return Vec::new() };
	tags(first).into_iter().filter(|t| rest.iter().all(|s| tags(s).contains(t))).collect()
}

/// What a site can be asked in place of links: a link's tags are all on its
/// post, so `eat[rabbit carrot]` narrows like `rabbit carrot`. A link that's
/// ruled out can't narrow anything and is left to the local check.
fn links_as_tags(e: &Expr) -> Expr {
	let has_link = |e: &Expr| e.any(&|e| matches!(e, Expr::Link { .. }));
	match e {
		Expr::Link { tags, .. } => Expr::And(tags.iter().map(|t| Expr::Tag(t.clone())).collect()),
		Expr::Not(inner) if has_link(inner) => Expr::And(Vec::new()),
		Expr::Not(inner) => Expr::Not(Box::new(links_as_tags(inner))),
		Expr::And(es) => Expr::And(es.iter().map(links_as_tags).collect()),
		Expr::Or(es) => Expr::Or(es.iter().map(links_as_tags).collect()),
		e => e.clone(),
	}
}

/// Plain tags a search uses positively, for looking up counts and aliases.
pub fn tags_in(expr: &Expr) -> Vec<String> {
	let mut out: Vec<String> = Vec::new();
	expr.walk(&mut |e| {
		let names: &[String] = match e {
			Expr::Tag(t) => std::slice::from_ref(t),
			// A link's tags are on its post, so they're searched for too.
			Expr::Link { tags, .. } => tags,
			_ => &[],
		};
		for t in names {
			if !t.contains('*') && !out.contains(t) {
				out.push(t.clone());
			}
		}
	});
	out
}

/// Replaces old tag names with the ones the site uses now, so posts that come
/// back (tagged with the new name) still pass the local check.
pub fn apply_aliases(expr: &Expr, aliases: &HashMap<String, String>) -> Expr {
	let name = |t: &String| aliases.get(t).cloned().unwrap_or_else(|| t.clone());
	match expr {
		Expr::Tag(t) => Expr::Tag(name(t)),
		Expr::Link { kind, tags } => Expr::Link { kind: kind.clone(), tags: tags.iter().map(name).collect() },
		Expr::Not(e) => Expr::Not(Box::new(apply_aliases(e, aliases))),
		Expr::And(es) => Expr::And(es.iter().map(|e| apply_aliases(e, aliases)).collect()),
		Expr::Or(es) => Expr::Or(es.iter().map(|e| apply_aliases(e, aliases)).collect()),
		e => e.clone(),
	}
}

/// Swaps tags for what one site calls them (see `Store::translations`). A tag
/// translated to an empty search isn't on the site, so its term is left out:
/// `fox -bar` with `bar` missing searches `fox` alone.
pub fn apply_translations(expr: &Expr, map: &HashMap<String, Expr>) -> Expr {
	fn go(e: &Expr, map: &HashMap<String, Expr>) -> Option<Expr> {
		match e {
			Expr::Tag(t) => match map.get(t) {
				Some(Expr::And(es)) if es.is_empty() => None,
				Some(to) => Some(to.clone()),
				None => Some(e.clone()),
			},
			// A link's tags follow one-tag translations; anything bigger can't sit in a link.
			Expr::Link { kind, tags } => Some(Expr::Link {
				kind: kind.clone(),
				tags: tags.iter().map(|t| if let Some(Expr::Tag(to)) = map.get(t) { to.clone() } else { t.clone() }).collect(),
			}),
			Expr::Not(inner) => go(inner, map).map(|i| Expr::Not(Box::new(i))),
			Expr::And(es) => Some(Expr::And(es.iter().filter_map(|e| go(e, map)).collect())),
			// One side that's always true makes the whole OR true, so it goes too.
			Expr::Or(es) => es.iter().map(|e| go(e, map)).collect::<Option<Vec<_>>>().map(Expr::Or),
			e => Some(e.clone()),
		}
	}
	if map.is_empty() {
		return expr.clone();
	}
	go(expr, map).unwrap_or(Expr::And(Vec::new()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::query::{META_KEYS, parse};

	fn caps(max_tags: usize) -> Capabilities {
		Capabilities { max_tags, tilde_or: true, max_wildcards: 1, metas: META_KEYS, free_metas: &[], page_size: 320 }
	}

	fn run(q: &str, max: usize) -> Plan {
		plan(&parse(q).unwrap(), &caps(max), &HashMap::new())
	}

	#[test]
	fn simple_goes_to_site() {
		let p = run("fox -sketch rating:s", 40);
		assert_eq!(p.queries, vec!["rating:s fox -sketch"]);
		assert!(p.notes.is_empty());
	}

	#[test]
	fn plain_or_becomes_tilde() {
		assert_eq!(run("solo (fox | wolf)", 40).queries, vec!["solo ~fox ~wolf"]);
	}

	#[test]
	fn complex_or_splits() {
		let p = run("solo ((fox smile) | (wolf -sketch))", 40);
		assert_eq!(p.queries, vec!["solo fox smile", "solo wolf -sketch"]);
		assert!(p.notes[0].contains("2 searches"));
	}

	#[test]
	fn rarest_tags_win_the_limit() {
		let counts = HashMap::from([("common".to_string(), 1_000_000), ("rare".to_string(), 10), ("mid".to_string(), 5000)]);
		let p = plan(&parse("common rare mid").unwrap(), &caps(2), &counts);
		assert_eq!(p.queries, vec!["rare mid"]);
		assert!(p.notes[0].contains("checked here instead: common."), "{:?}", p.notes);
	}

	#[test]
	fn order_is_kept_and_blocks_splitting() {
		let p = run("order:score ((a b) | (c d))", 40);
		assert_eq!(p.queries, vec!["order:score"]);
		assert!(p.ordered);
	}

	#[test]
	fn links_narrow_by_their_tags() {
		let p = run("relation[rabbit eat] -relation[fox den]", 40);
		assert!(!p.local);
		assert_eq!(p.queries, vec!["rabbit eat"]);
		assert_eq!(run("eat[rabbit carrot] | eat[dog bone]", 40).queries, vec!["rabbit carrot", "dog bone"]);
		// Only ruled-out links: nothing to ask the site, so saved posts are searched.
		assert!(run("-relation[rabbit eat]", 40).local);
	}

	#[test]
	fn one_wildcard_and_one_tilde_group() {
		assert_eq!(run("fox* wolf*", 40).queries, vec!["fox*"]);
		assert_eq!(run("(a | b) (c | d)", 40).queries.len(), 2);
	}

	#[test]
	fn aliases_rewrite_tags() {
		let e = apply_aliases(&parse("kitty -(doggo | x) [kitty]").unwrap(), &HashMap::from([("kitty".into(), "cat".into()), ("doggo".into(), "dog".into())]));
		assert_eq!(e.to_string(), "cat -(dog | x) [cat]");
	}

	#[test]
	fn free_metas_dont_use_up_the_limit() {
		let caps = Capabilities { free_metas: &["rating", "score"], ..caps(2) };
		let e = crate::query::parse("fox smile wolf rating:s score:>5 -rating:e").unwrap();
		let counts = HashMap::from([("fox".into(), 10), ("smile".into(), 1000), ("wolf".into(), 5)]);
		assert_eq!(plan(&e, &caps, &counts).queries, vec!["rating:s wolf fox score:>5 -rating:e"]);
	}


	#[test]
	fn translations_swap_or_drop_tags() {
		let map = HashMap::from([
			("girl".to_string(), parse("1girl | 2girls").unwrap()),
			("fox".to_string(), parse("fox_ears").unwrap()),
			("sketch".to_string(), parse("").unwrap()),
		]);
		let e = apply_translations(&parse("girl fox -sketch (sketch | smile)").unwrap(), &map);
		assert_eq!(e, parse("(1girl | 2girls) fox_ears").unwrap());
		assert_eq!(run(&e.to_string(), 40).queries, vec!["fox_ears ~1girl ~2girls"]);
	}


	#[test]
	fn big_ors_narrow_by_shared_tags() {
		let sides: Vec<String> = (0..8).map(|n| format!("eat[rabbit food{n}]")).collect();
		let p = run(&sides.join(" | "), 40);
		assert_eq!(p.queries, vec!["rabbit"]);
		// Translations reach inside links when they're one tag.
		let map = HashMap::from([("dog".to_string(), parse("canine").unwrap())]);
		assert_eq!(apply_translations(&parse("eat[dog bone]").unwrap(), &map), parse("eat[canine bone]").unwrap());
	}


	#[test]
	fn left_out_groups_reach_the_site() {
		// -(a | b), as `-@fat` becomes, is sent as -a -b.
		assert_eq!(run("fox -(overweight | obese)", 40).queries, vec!["fox -overweight -obese"]);
		assert_eq!(run("fox --smile", 40).queries, vec!["fox smile"]);
	}

}
