// The tags panel, left of the results: your search as chips you can flip,
// the tags across everything loaded (with how many posts have each), links
// in those posts, and the tags and searches you use most. Hovering a tag
// dims the posts without it.

use std::collections::HashMap;
use std::sync::Mutex;

use bettersearch_core::search::AskedHere;
use bettersearch_core::store::{RelationEntry, RelationState, SavedSearch};
use bettersearch_core::{Post, Store};
use eframe::egui::{self, Color32, RichText, Sense, Vec2};

use crate::{Action, category_color};

/// Rows of result tags shown before "show all".
const SHOWN: usize = 40;

pub struct TagStat {
	pub name: String,
	pub count: usize,
	pub category: String,
}

/// Every tag on the posts, most common first, with its category from the site.
pub fn result_tags(posts: &[Post]) -> Vec<TagStat> {
	let mut counts: HashMap<&str, (usize, &str)> = HashMap::new();
	for p in posts {
		for (cat, tags) in &p.categories {
			for t in tags {
				counts.entry(t.as_str()).or_insert((0, cat.as_str())).0 += 1;
			}
		}
		// Sites without categories only fill `tags`.
		if p.categories.is_empty() {
			for t in &p.tags {
				counts.entry(t.as_str()).or_insert((0, "general")).0 += 1;
			}
		}
	}
	let mut out: Vec<TagStat> =
		counts.into_iter().map(|(name, (count, cat))| TagStat { name: name.to_string(), count, category: cat.to_string() }).collect();
	out.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
	out
}

/// Plain tags at the top level of a search: `Some(true)` for `fox`, `Some(false)` for `-fox`.
pub fn search_tags(query: &str) -> Vec<(String, bool)> {
	let mut out = Vec::new();
	for w in query.split_whitespace() {
		let (on, t) = w.strip_prefix('-').map_or((true, w), |t| (false, t));
		let plain = !t.is_empty() && !t.contains([':', '(', ')', '[', ']', '|', '~', '*']);
		if plain && !out.iter().any(|(n, _)| n == t) {
			out.push((t.to_string(), on));
		}
	}
	out
}

/// The search with `tag` included (`Some(true)`), excluded (`Some(false)`) or
/// taken out (`None`). Only touches the tag where it stands on its own.
pub fn set_tag(query: &str, tag: &str, state: Option<bool>) -> String {
	let neg = format!("-{tag}");
	let mut words: Vec<&str> = query.split_whitespace().filter(|w| *w != tag && *w != neg).collect();
	match state {
		Some(true) => words.push(tag),
		Some(false) => words.push(&neg),
		None => {}
	}
	words.join(" ")
}

impl Default for TagsPanel {
	fn default() -> Self {
		TagsPanel {
			filter: String::new(),
			all: false,
			made_for: (0, 0, 0),
			last: std::time::Instant::now() - std::time::Duration::from_secs(5),
			stats: Vec::new(),
			links: Vec::new(),
			usual: Vec::new(),
			recent: Vec::new(),
			groups: Vec::new(),
			saved: Vec::new(),
			relations: Vec::new(),
			confirm: None,
		}
	}
}

pub struct TagsPanel {
	filter: String,
	all: bool,
	/// (generation, posts, store changes) the cached lists were made for.
	made_for: (u64, usize, u64),
	/// When the lists were last rebuilt, to keep it off every frame.
	last: std::time::Instant,
	stats: Vec<TagStat>,
	links: Vec<(String, Vec<String>, usize)>,
	usual: Vec<(String, usize)>,
	recent: Vec<String>,
	groups: Vec<(String, Vec<String>)>,
	saved: Vec<SavedSearch>,
	/// For a `relation` search: each subject and its entries in the index, read
	/// fresh so keeping and rejecting show at once (rejected ones included).
	relations: Vec<(String, Vec<RelationEntry>)>,
	/// The saved search whose ✕ was pressed, waiting for "Delete".
	confirm: Option<i64>,
}

impl TagsPanel {
	fn refresh(&mut self, posts: &[Post], generation: u64, store: &Mutex<Store>) {
		let s = store.lock().unwrap();
		let key = (generation, posts.len(), s.changes());
		if key == self.made_for {
			return;
		}
		// `changes` ticks on every post written, so without a delay the whole
		// tag list is rebuilt dozens of times while a first page loads.
		if self.last.elapsed() < std::time::Duration::from_millis(400) && key.1 == self.made_for.1 {
			return;
		}
		self.last = std::time::Instant::now();
		if (key.0, key.1) != (self.made_for.0, self.made_for.1) {
			self.stats = result_tags(posts);
		}
		self.made_for = key;
		let g = s.graph();
		let mut links: Vec<(String, Vec<String>, usize)> = Vec::new();
		for p in posts {
			for l in g.item_links(&p.key(), &s.actor).unwrap_or_default() {
				if l.score <= 0 {
					continue;
				}
				match links.iter_mut().find(|(k, m, _)| *k == l.kind && *m == l.members) {
					Some(found) => found.2 += 1,
					None => links.push((l.kind, l.members, 1)),
				}
			}
		}
		links.sort_by_key(|l| std::cmp::Reverse(l.2));
		self.links = links;
		self.usual = s.history_tags(24).unwrap_or_default();
		self.recent = s.history(8).unwrap_or_default().into_iter().map(|(q, _)| q).collect();
		self.groups = s.groups().unwrap_or_default();
		self.saved = s.saved_searches().unwrap_or_default();
	}

	/// Draws the panel; returns the tag under the mouse, if any.
	#[allow(clippy::too_many_arguments)]
	pub fn show(
		&mut self,
		ui: &mut egui::Ui,
		query: &str,
		posts: &[Post],
		asked: Option<&AskedHere>,
		generation: u64,
		store: &Mutex<Store>,
		actions: &mut Vec<Action>,
	) -> Option<String> {
		self.refresh(posts, generation, store);
		if let Some(a) = asked {
			let st = store.lock().unwrap();
			self.relations = a.pairs.iter().map(|(subject, _)| (subject.clone(), st.relations(subject, &a.verb, true).unwrap_or_default())).collect();
		}
		let mut hovered = None;
		ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter tags").desired_width(f32::INFINITY));
		let filter = self.filter.trim().to_lowercase().replace(' ', "_");
		let fits = |name: &str| filter.is_empty() || name.contains(&filter);

		egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
			if let Some(a) = asked {
				heading(ui, &format!("{} {} → relation", a.subject, a.verb));
				ui.label(RichText::new(format!("From the relation index; searched with “{}”. ✓ keeps one, ✕ rejects it.", a.verb_tag)).weak().small());
				for (subject, entries) in &self.relations {
					if self.relations.len() > 1 {
						ui.label(RichText::new(subject).strong());
					}
					if entries.is_empty() {
						ui.label(RichText::new("nothing known").weak().italics());
					}
					for e in entries {
						ui.horizontal(|ui| {
							let rejected = e.state == RelationState::Rejected;
							let text = RichText::new(&e.object).color(Color32::from_rgb(120, 200, 140));
							let r = ui.add(egui::Label::new(if rejected { text.strikethrough().weak() } else { text }).sense(Sense::click()));
							let r = r.on_hover_text(match e.state {
								RelationState::Learned => "Learned from how tags appear together · click to search with just this",
								RelationState::Kept => "Kept · click to search with just this",
								RelationState::Rejected => "Rejected: not used",
							});
							if r.hovered() {
								hovered = Some(e.object.clone());
							}
							if r.clicked() && !rejected {
								actions.push(Action::SetSearch(format!("{subject} {} {}", a.verb_tag, e.object)));
							}
							ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
								let set = |state| Action::SetRelation { subject: subject.clone(), verb: a.verb.clone(), object: e.object.clone(), state };
								match e.state {
									RelationState::Learned => {
										if ui.small_button("✕").on_hover_text("Reject: not something it does").clicked() {
											actions.push(set(RelationState::Rejected));
										}
										if ui.small_button("✓").on_hover_text("Keep").clicked() {
											actions.push(set(RelationState::Kept));
										}
									}
									RelationState::Kept => {
										ui.label(RichText::new("✓ kept").small().color(Color32::from_rgb(110, 200, 120)));
									}
									RelationState::Rejected => {
										if ui.small_button("undo").clicked() {
											actions.push(set(RelationState::Learned));
										}
									}
								}
							});
						});
					}
				}
				if ui.small_button("Search again").on_hover_text("Run the search with your changes").clicked() {
					actions.push(Action::Search);
				}
			}
			if !self.saved.is_empty() {
				heading(ui, "Saved searches");
				for s in self.saved.iter().filter(|s| fits(&s.name) || s.query.contains(&filter)) {
					ui.horizontal(|ui| {
						if self.confirm == Some(s.id) {
							ui.label(RichText::new(format!("Delete “{}”?", s.name)).color(Color32::from_rgb(230, 100, 100)));
							if ui.small_button("Delete").clicked() {
								actions.push(Action::DeleteSaved(s.id));
								self.confirm = None;
							}
							if ui.small_button("Keep").clicked() {
								self.confirm = None;
							}
							return;
						}
						let r = ui.link(&s.name).on_hover_text(format!("{}\non {}", s.query, s.sites.join(", ")));
						if r.clicked() {
							actions.push(Action::RunSaved(s.id));
						}
						ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
							if ui.small_button("✕").on_hover_text("Delete this saved search").clicked() {
								self.confirm = Some(s.id);
							}
						});
					});
				}
			}

			let in_search = search_tags(query);
			if !in_search.is_empty() {
				heading(ui, "In your search");
				ui.horizontal_wrapped(|ui| {
					for (t, on) in &in_search {
						let (mark, color) =
							if *on { ("✓", Color32::from_rgb(110, 200, 120)) } else { ("⊘", Color32::from_rgb(230, 100, 100)) };
						let r = ui
							.add(egui::Button::new(RichText::new(format!("{mark} {t}")).color(color)).small())
							.on_hover_text(if *on { "Click to exclude it instead" } else { "Click to take it out" });
						if r.hovered() {
							hovered = Some(t.clone());
						}
						if r.clicked() {
							let next = if *on { Some(false) } else { None };
							actions.push(Action::SetSearch(set_tag(query, t, next)));
						}
					}
				});
			}

			if !posts.is_empty() {
				heading(ui, &format!("In these {} posts", posts.len()));
				let max = self.stats.first().map_or(1, |s| s.count).max(1);
				let shown: Vec<&TagStat> =
					self.stats.iter().filter(|s| fits(&s.name)).take(if self.all { usize::MAX } else { SHOWN }).collect();
				for s in &shown {
					let state = in_search.iter().find(|(t, _)| *t == s.name).map(|(_, on)| *on);
					if let Some(h) = self.row(ui, query, s, max, posts.len(), state, actions) {
						hovered = Some(h);
					}
				}
				let more = self.stats.iter().filter(|s| fits(&s.name)).count();
				if more > SHOWN && ui.small_button(if self.all { "show fewer" } else { "show all" }).clicked() {
					self.all = !self.all;
				}
			}

			heading(ui, "Your groups");
			ui.horizontal_wrapped(|ui| {
				for (g, members) in self.groups.iter().filter(|(g, m)| fits(g) || m.iter().any(|t| fits(t))) {
					let r = ui
						.add(egui::Button::new(RichText::new(format!("@{g}")).color(Color32::from_rgb(150, 150, 220))).small())
						.on_hover_text(format!("{} · click to search any of them", members.join(", ")));
					if r.clicked() {
						actions.push(Action::SetSearch(set_tag(query, &format!("@{g}"), Some(true))));
					}
				}
				if ui.small_button("Groups & relations…").clicked() {
					actions.push(Action::OpenGroups(None));
				}
			});

			let links: Vec<_> = self.links.iter().filter(|(k, m, _)| fits(k) || m.iter().any(|t| fits(t))).collect();
			if !links.is_empty() {
				heading(ui, "Links in these posts");
				for (kind, members, n) in links.into_iter().take(20) {
					let r = ui.link(format!("{kind}: {}  ({n})", members.join(" · "))).on_hover_text("Search for this link");
					if r.clicked() {
						actions.push(Action::SetSearch(format!("{kind}[{}]", members.join(" "))));
					}
				}
			}

			let usual: Vec<_> = self.usual.iter().filter(|(t, _)| fits(t)).collect();
			if !usual.is_empty() {
				heading(ui, "Your usual tags");
				ui.horizontal_wrapped(|ui| {
					for (t, n) in usual {
						let r = ui
							.add(egui::Button::new(RichText::new(t).small()).small())
							.on_hover_text(format!("In {n} of your searches · click to add"));
						if r.hovered() {
							hovered = Some(t.clone());
						}
						if r.clicked() {
							actions.push(Action::SetSearch(set_tag(query, t, Some(true))));
						}
					}
				});
			}

			if filter.is_empty() && !self.recent.is_empty() {
				heading(ui, "Recent searches");
				for q in &self.recent {
					if ui.link(RichText::new(q).monospace().small()).clicked() {
						actions.push(Action::SetSearch(q.clone()));
					}
				}
			}
		});
		hovered
	}

	/// One result tag: name (click: search with it), a bar for how many posts
	/// have it, and − to exclude it.
	#[allow(clippy::too_many_arguments)]
	fn row(
		&self,
		ui: &mut egui::Ui,
		query: &str,
		s: &TagStat,
		max: usize,
		total: usize,
		state: Option<bool>,
		actions: &mut Vec<Action>,
	) -> Option<String> {
		let mut hovered = None;
		ui.horizontal(|ui| {
			let color = category_color(&s.category);
			let name = match state {
				Some(true) => RichText::new(format!("✓ {}", s.name)).color(color).strong(),
				Some(false) => RichText::new(format!("⊘ {}", s.name)).color(color).strikethrough(),
				None => RichText::new(&s.name).color(color),
			};
			let width = (ui.available_width() - 70.0).max(60.0);
			let (rect, r) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::click());
			// The bar sits behind the name: how many of the loaded posts have it.
			let share = s.count as f32 / max as f32;
			let bar = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width() * share, rect.height()));
			ui.painter().rect_filled(bar, 3.0, color.gamma_multiply(if r.hovered() { 0.28 } else { 0.14 }));
			ui.painter().text(
				rect.left_center() + Vec2::new(4.0, 0.0),
				egui::Align2::LEFT_CENTER,
				name.text(),
				egui::FontId::proportional(13.0),
				color,
			);
			let r = r.on_hover_text(format!("On {} of {total} posts · click to search with it", s.count));
			if r.hovered() {
				hovered = Some(s.name.clone());
			}
			if r.clicked() {
				let next = if state == Some(true) { None } else { Some(true) };
				actions.push(Action::SetSearch(set_tag(query, &s.name, next)));
			}
			ui.label(RichText::new(s.count.to_string()).weak().small());
			if ui.small_button("−").on_hover_text("Exclude it").clicked() {
				actions.push(Action::SetSearch(set_tag(query, &s.name, if state == Some(false) { None } else { Some(false) })));
			}
		});
		hovered
	}
}

fn heading(ui: &mut egui::Ui, text: &str) {
	ui.add_space(8.0);
	ui.label(RichText::new(text.to_uppercase()).small().strong().weak());
	ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn flips_tags_in_a_search() {
		assert_eq!(search_tags("fox -sketch (a | b) score:>5 fox"), vec![("fox".to_string(), true), ("sketch".into(), false)]);
		assert_eq!(set_tag("fox smile", "smile", Some(false)), "fox -smile");
		assert_eq!(set_tag("fox -smile", "smile", None), "fox");
		assert_eq!(set_tag("(smile | grin)", "smile", Some(true)), "(smile | grin) smile");
		assert_eq!(set_tag("", "fox", Some(true)), "fox");
	}
}
