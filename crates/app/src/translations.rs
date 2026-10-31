// The "Tag translations" window: what each site calls a tag. Searches use your
// words; each site gets its own version before planning.

use std::sync::{Arc, Mutex};

use bettersearch_core::store::Translation;
use bettersearch_core::{Site, Store};
use eframe::egui::{self, Color32, RichText};

#[derive(Default)]
pub struct Translations {
	pub open: bool,
	pub tag: String,
	site: String,
	target: String,
	error: Option<String>,
	/// Loaded from the store when shown; `None` after an edit.
	rows: Option<Vec<Translation>>,
}

impl Translations {
	/// Draws the window if open; true when a translation changed.
	pub fn show(&mut self, ctx: &egui::Context, store: &Arc<Mutex<Store>>, sites: &[Arc<dyn Site>]) -> bool {
		if !self.open {
			self.rows = None;
			return false;
		}
		if self.site.is_empty()
			&& let Some(s) = sites.first()
		{
			self.site = s.id().to_string();
		}
		let rows = match &self.rows {
			Some(r) => r.clone(),
			None => {
				let r = store.lock().unwrap().all_translations().unwrap_or_default();
				self.rows = Some(r.clone());
				r
			}
		};
		let name_of = |id: &str| sites.iter().find(|s| s.id() == id).map_or(id.to_string(), |s| s.name().to_string());

		let mut open = self.open;
		let mut save = false;
		let mut remove = None;
		egui::Window::new("Tag translations").open(&mut open).default_width(520.0).show(ctx, |ui| {
			ui.label(
				"Write searches in your own words. When a site calls a tag something else, say what to search there \
				instead: a tag, or a search like “1girl | 2girls”. Leave it empty when the site has nothing like it, \
				and the tag is left out there.",
			);
			ui.add_space(6.0);
			egui::Grid::new("translate").num_columns(2).show(ui, |ui| {
				ui.label("Tag");
				ui.add(egui::TextEdit::singleline(&mut self.tag).hint_text("girl").desired_width(260.0));
				ui.end_row();
				ui.label("On");
				egui::ComboBox::from_id_salt("translate-site").selected_text(name_of(&self.site)).show_ui(ui, |ui| {
					for s in sites {
						ui.selectable_value(&mut self.site, s.id().to_string(), s.name());
					}
				});
				ui.end_row();
				ui.label("Search as");
				let r = ui.add(
					egui::TextEdit::singleline(&mut self.target)
						.hint_text("1girl | 2girls   (empty: not on this site)")
						.desired_width(260.0),
				);
				save |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				ui.end_row();
			});
			save |= ui.button("Save").clicked();
			if let Some(e) = &self.error {
				ui.colored_label(Color32::from_rgb(220, 90, 90), e);
			}
			ui.separator();
			if rows.is_empty() {
				ui.label(RichText::new("No translations yet. Right-click a tag on a post to add one.").weak());
			}
			egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
				let mut last = "";
				for t in &rows {
					if t.tag != last {
						ui.add_space(4.0);
						ui.strong(&t.tag);
						last = &t.tag;
					}
					ui.horizontal(|ui| {
						ui.add_space(12.0);
						let target = if t.target.is_empty() { "(not on this site)".to_string() } else { t.target.clone() };
						if ui.link(format!("{}: {target}", name_of(&t.site))).on_hover_text("Edit").clicked() {
							self.tag = t.tag.clone();
							self.site = t.site.clone();
							self.target = t.target.clone();
						}
						if ui.small_button("✕").on_hover_text("Remove").clicked() {
							remove = Some((t.site.clone(), t.tag.clone()));
						}
					});
				}
			});
		});
		self.open = open;

		if save {
			let result = store.lock().unwrap().set_translation(&self.site, &self.tag, &self.target);
			self.error = result.err().map(|e| e.to_string());
			if self.error.is_none() {
				self.target.clear();
			}
			self.rows = None;
		}
		let removed = remove.is_some();
		if let Some((site, tag)) = remove {
			self.error = store.lock().unwrap().remove_translation(&site, &tag).err().map(|e| e.to_string());
			self.rows = None;
		}
		save || removed
	}
}
