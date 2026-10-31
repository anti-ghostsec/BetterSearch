// The "Groups & relations" window. Groups name a set of tags (@animal: dog,
// rabbit); relations say how two or more tags go together wherever they meet
// (rabbit eat carrot), and fill in as links on every post with those tags.
// Together: eat[@animal @object] finds any animal eating any object.

use std::sync::{Arc, Mutex};

use bettersearch_core::Store;
use bettersearch_core::store::{RelationEntry, RelationState};
use eframe::egui::{self, Color32, RichText};

#[derive(Default)]
pub struct Groups {
	pub open: bool,
	group: String,
	/// The tag box for groups; set from "Add to a group…" on a tag.
	pub tag: String,
	first: String,
	kind: String,
	second: String,
	new_kind: String,
	error: Option<String>,
}

impl Groups {
	/// Draws the window if open; true when something changed.
	pub fn show(&mut self, ctx: &egui::Context, store: &Arc<Mutex<Store>>) -> bool {
		if !self.open {
			return false;
		}
		let (groups, rules, mut kinds, index) = {
			let s = store.lock().unwrap();
			let index: Vec<(String, String, Vec<RelationEntry>)> = s
				.relation_topics()
				.unwrap_or_default()
				.into_iter()
				.map(|(subject, verb)| {
					let entries = s.relations(&subject, &verb, false).unwrap_or_default();
					(subject, verb, entries)
				})
				.collect();
			(s.groups().unwrap_or_default(), s.rules().unwrap_or_default(), s.graph().link_names().unwrap_or_default(), index)
		};
		// The first link name means "any link" in searches, so it isn't a relation.
		if !kinds.is_empty() {
			kinds.remove(0);
		}
		if !kinds.contains(&self.kind) {
			self.kind = kinds.first().cloned().unwrap_or_default();
		}

		enum Do {
			Join(String, String),
			Leave(String, String),
			Relate(String, Vec<String>),
			Unrelate(i64),
			NewKind(String),
			Forget(String, String),
		}
		let mut todo = None;
		let mut open = self.open;
		egui::Window::new("Groups & relations").open(&mut open).default_width(480.0).show(ctx, |ui| {
			ui.label("Put tags in groups and say how tags relate, then search across them:");
			ui.horizontal(|ui| {
				ui.monospace("eat[@animal @object]");
				ui.label(RichText::new("any animal eating any object").weak());
			});
			ui.horizontal(|ui| {
				ui.monospace("@animal smile");
				ui.label(RichText::new("any animal, smiling").weak());
			});

			ui.add_space(8.0);
			ui.strong("Groups");
			if groups.is_empty() {
				ui.label(RichText::new("None yet. e.g. group animal: dog, rabbit · group object: bone, carrot").weak());
			}
			for (g, members) in &groups {
				ui.horizontal_wrapped(|ui| {
					ui.label(RichText::new(format!("@{g}")).strong().color(Color32::from_rgb(150, 150, 220)));
					for m in members {
						ui.label(m);
						if ui.small_button("✕").on_hover_text(format!("Take {m} out of @{g}")).clicked() {
							todo = Some(Do::Leave(g.clone(), m.clone()));
						}
					}
				});
			}
			ui.horizontal(|ui| {
				ui.add(egui::TextEdit::singleline(&mut self.group).hint_text("group: animal").desired_width(120.0));
				let r = ui.add(egui::TextEdit::singleline(&mut self.tag).hint_text("tag: rabbit").desired_width(140.0));
				let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				if ui.button("Add to group").clicked() || enter {
					todo = Some(Do::Join(self.group.clone(), std::mem::take(&mut self.tag)));
				}
			});

			ui.add_space(8.0);
			ui.strong("Relations");
			ui.label(RichText::new("Each one fills in as a link on every post that has those tags, so it's searchable.").weak());
			for r in &rules {
				ui.horizontal(|ui| {
					let (head, rest) = r.members.split_first().map_or(("", &[][..]), |(h, t)| (h.as_str(), t));
					ui.label(format!("{head}  —{}→  {}", r.kind, rest.join(", ")));
					if ui.small_button("✕").on_hover_text("Remove this relation").clicked() {
						todo = Some(Do::Unrelate(r.id));
					}
				});
			}
			ui.horizontal(|ui| {
				ui.add(egui::TextEdit::singleline(&mut self.first).hint_text("rabbit").desired_width(110.0));
				egui::ComboBox::from_id_salt("relation-kind").selected_text(&self.kind).show_ui(ui, |ui| {
					for k in &kinds {
						ui.selectable_value(&mut self.kind, k.clone(), k);
					}
				});
				ui.add(egui::TextEdit::singleline(&mut self.second).hint_text("carrot").desired_width(110.0));
				if ui.button("Relate").clicked() {
					let members = vec![self.first.trim().to_string(), self.second.trim().to_string()];
					todo = Some(Do::Relate(self.kind.clone(), members));
				}
			});
			ui.horizontal(|ui| {
				ui.add(egui::TextEdit::singleline(&mut self.new_kind).hint_text("new relation: eat, likes…").desired_width(160.0));
				if ui.add_enabled(!self.new_kind.trim().is_empty(), egui::Button::new("Add relation name")).clicked() {
					todo = Some(Do::NewKind(std::mem::take(&mut self.new_kind)));
				}
			});
			ui.add_space(8.0);
			ui.strong("Relation index");
			ui.label(
				RichText::new("Learned the first time you search like “rabbit eat relation”, from how tags appear together. Keep or reject them from the tags panel.")
					.weak(),
			);
			if index.is_empty() {
				ui.label(RichText::new("Empty so far.").weak());
			}
			egui::ScrollArea::vertical().max_height(200.0).id_salt("relation-index").show(ui, |ui| {
				for (subject, verb, entries) in &index {
					ui.horizontal_wrapped(|ui| {
						ui.label(RichText::new(format!("{subject} {verb}:")).strong());
						let names: Vec<String> = entries
							.iter()
							.map(|e| if e.state == RelationState::Kept { format!("{} ✓", e.object) } else { e.object.clone() })
							.collect();
						ui.label(if names.is_empty() { "nothing".to_string() } else { names.join(", ") });
						if ui.small_button("Learn again").on_hover_text("Forget what was learned (not what you kept or rejected)").clicked() {
							todo = Some(Do::Forget(subject.clone(), verb.clone()));
						}
					});
				}
			});
			if let Some(e) = &self.error {
				ui.colored_label(Color32::from_rgb(220, 90, 90), e);
			}
		});
		self.open = open;

		let Some(todo) = todo else { return false };
		let s = store.lock().unwrap();
		let result = match todo {
			Do::Join(g, t) => s.set_in_group(&g, &t, true),
			Do::Leave(g, t) => s.set_in_group(&g, &t, false),
			Do::Relate(kind, members) => s.add_rule(&kind, &members).inspect(|_| {
				self.first.clear();
				self.second.clear();
			}),
			Do::Unrelate(id) => s.graph().remove_link(id, &s.actor),
			Do::NewKind(name) => s.graph().add_link_name(&name, &s.actor).map(|_| self.kind = s.graph().normalize(&name)),
			Do::Forget(subject, verb) => s.forget_learned(&subject, &verb),
		};
		self.error = result.err().map(|e| e.to_string());
		true
	}
}
