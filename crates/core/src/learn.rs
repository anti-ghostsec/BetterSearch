// Learns relations from how tags appear together, so `rabbit eat relation`
// can find rabbits eating carrots with nothing set up.
//
// An answer to "rabbit eat ?" has to be tied to both sides: to the subject
// (carrot comes with rabbit far more than with posts in general) and to the
// verb, either directly (carrot comes with eating) or through the verb's
// bridges, the big tags that come with it (eating → food, and carrot comes with
// food). Rabbit ears are tied only to rabbit, plates only to eating; carrot
// is tied to both. "Tied" is lift: how many times more common a tag is among a
// tag's posts than on the site as a whole.

use crate::sites::{Profile, Site};
use crate::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct Learned {
	pub object: String,
	pub score: f32,
}

/// How many times more common `tag` is in the profile than on the whole site,
/// counted cautiously: from the low end of what the sample could mean (about
/// 95% sure), so a tag seen 3 times by chance doesn't look telling while one
/// seen 30 times keeps most of its weight.
fn lift(p: &Profile, tag: &str, size: u64, total: u64) -> f32 {
	let k = *p.counts.get(tag).unwrap_or(&0) as f32;
	let low = (k - 1.65 * k.sqrt()).max(0.0);
	let expected = p.sampled.max(1) as f32 * size.max(1) as f32 / total.max(1) as f32;
	low / expected
}

/// The verb's bridges: big tags that come with it on many of its posts (eating → food).
pub fn bridges(verb: &str, pv: &Profile, total: u64, n: usize) -> Vec<String> {
	let big = (total as f64 * 0.0007) as u64;
	let mut out: Vec<(f32, &String)> = pv
		.counts
		.iter()
		.filter(|(t, _)| *t != verb && !t.split('_').any(|part| part == verb))
		.filter_map(|(t, c)| {
			let size = *pv.sizes.get(t)?;
			let l = lift(pv, t, size, total);
			(size >= big && l > 3.0).then(|| (*c as f32 / pv.sampled.max(1) as f32 * l.ln(), t))
		})
		.collect();
	out.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
	out.into_iter().take(n).map(|(_, t)| t.clone()).collect()
}

/// What the subject does the verb with, best first. `ps` profiles the subject,
/// `pv` the verb, `via` the verb's bridges.
pub fn objects(subject: &str, verb: &str, ps: &Profile, pv: &Profile, via: &[(String, Profile)], total: u64, limit: usize) -> Vec<Learned> {
	// Rare tags (one artist's character, one comic) can't be judged from a
	// sample; very broad ones (fully_clothed, humor: on 1 post in 100 or more)
	// describe everything and answer nothing.
	let common_enough = ((total as f64 * 0.00004) as u64).max(100);
	let too_broad = total / 100;
	let mut out: Vec<Learned> = ps
		.counts
		.iter()
		.filter(|(t, c)| {
			**c >= 4
				&& *t != subject
				&& *t != verb
				&& !t.split('_').any(|part| part == verb)
				&& !via.iter().any(|(b, _)| b == *t)
		})
		.filter_map(|(t, _)| {
			let size = *ps.sizes.get(t)?;
			if size < common_enough || size > too_broad {
				return None;
			}
			let with_subject = lift(ps, t, size, total);
			// Tied to the verb: seen with it on a few posts...
			let direct = if pv.counts.get(t).copied().unwrap_or(0) >= 3 { lift(pv, t, size, total) } else { 0.0 };
			// ...and, when the verb has bridges, with a bridge too: what's eaten comes
			// with food (carrot does; a character's sweater seen at a few meals doesn't).
			let bridged = via
				.iter()
				.filter(|(_, p)| p.counts.get(t).copied().unwrap_or(0) >= 3)
				.map(|(_, p)| lift(p, t, size, total))
				.fold(0.0, f32::max);
			let with_verb = if via.is_empty() {
				direct
			} else if bridged > 3.0 {
				direct.max(0.5 * bridged)
			} else {
				0.0
			};
			(with_subject > 3.0 && with_verb > 3.0).then(|| Learned { object: t.clone(), score: with_subject.ln() * with_verb.ln() })
		})
		.collect();
	out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.object.cmp(&b.object)));
	out.truncate(limit);
	out
}

/// Learns on a site: profiles of the subject, the verb and its two bridges.
/// `None` if the site can't profile tags (Gelbooru-style sites).
pub fn learn(site: &dyn Site, subject: &str, verb: &str, limit: usize) -> Result<Option<Vec<Learned>>> {
	let Some(total) = site.total_posts()? else { return Ok(None) };
	let Some(ps) = site.tag_profile(subject)? else { return Ok(None) };
	let Some(pv) = site.tag_profile(verb)? else { return Ok(None) };
	let mut via = Vec::new();
	for b in bridges(verb, &pv, total, 2) {
		if let Some(p) = site.tag_profile(&b)? {
			via.push((b, p));
		}
	}
	Ok(Some(objects(subject, verb, &ps, &pv, &via, total, limit)))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	fn profile(sampled: usize, counts: &[(&str, u32)], sizes: &[(&str, u64)]) -> Profile {
		Profile {
			sampled,
			counts: counts.iter().map(|(t, c)| (t.to_string(), *c)).collect(),
			sizes: sizes.iter().map(|(t, s)| (t.to_string(), *s)).collect::<HashMap<_, _>>(),
		}
	}

	#[test]
	fn carrot_is_what_rabbits_eat() {
		let total = 7_000_000;
		let sizes = [
			("rabbit", 60_000),
			("eating", 50_000),
			("food", 120_000),
			("carrot", 4_000),
			("long_ears", 80_000),
			("plate", 15_000),
			("one_comic_shirt", 30),
			("orange_sweater", 1_500),
			("smile", 2_000_000),
		];
		// Rabbits: long ears everywhere, carrots often, a comic's shirt a few times.
		// A rabbit character in an orange sweater, seen at a few meals.
		let ps = profile(
			600,
			&[("rabbit", 600), ("long_ears", 400), ("carrot", 30), ("smile", 200), ("one_comic_shirt", 6), ("orange_sweater", 8)],
			&sizes,
		);
		// Eating: food on most posts, plates often, carrots now and then.
		let pv = profile(
			600,
			&[("eating", 600), ("food", 400), ("plate", 90), ("carrot", 4), ("smile", 250), ("long_ears", 5), ("orange_sweater", 3)],
			&sizes,
		);
		assert_eq!(bridges("eating", &pv, total, 2)[0], "food");
		let food = profile(600, &[("food", 600), ("carrot", 20), ("plate", 100)], &sizes);
		let got = objects("rabbit", "eating", &ps, &pv, &[("food".into(), food)], total, 5);
		// Carrot: tied to rabbit and to eating/food. Ears: rabbit only. Plate:
		// eating only. Smile: everywhere. The comic shirt: too rare to judge.
		// The sweater: rabbit and a few meals, but never with food.
		assert_eq!(got.iter().map(|l| l.object.as_str()).collect::<Vec<_>>(), vec!["carrot"], "{got:?}");
	}
}
