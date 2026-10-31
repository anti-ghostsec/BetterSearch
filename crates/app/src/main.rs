// BetterSearch: a desktop booru search with real boolean searches and links
// between tags. The engine lives in bettersearch-core; this is the window.
// Everything it keeps is in an encrypted vault, opened with a PIN (lock.rs).

#[allow(dead_code)]
mod cache;
mod groups;
mod lock;
mod media;
mod settings;
mod tags_panel;
mod translations;
mod vault;
mod worker;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use bettersearch_core::sites::{Danbooru, E621, Gelbooru, user_agent};
use bettersearch_core::store::RelationState;
use bettersearch_core::taggraph::{ItemLink, ItemTag, LinkSuggestion};
use bettersearch_core::{Post, Rating, Site, Store, TagSuggestion};
use eframe::egui::{self, Color32, RichText, Sense, TextureHandle, Vec2};
use zeroize::Zeroize;

use groups::Groups;
use lock::{Root, Saver};
use settings::Settings;
use tags_panel::TagsPanel;
use translations::Translations;
use worker::{Job, Msg, Worker};

/// A site's name and its answer to "Check logins": who, or why not.
type LoginCheck = (String, Result<String, String>);

/// Matching posts asked for per page.
const PAGE: usize = 60;
const THUMB: f32 = 150.0;

fn main() -> eframe::Result {
	let options = eframe::NativeOptions {
		// OpenGL starts on more machines than wgpu's Vulkan path, which can hang at adapter setup.
		renderer: eframe::Renderer::Glow,
		viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]).with_title("BetterSearch"),
		..Default::default()
	};
	eframe::run_native("BetterSearch", options, Box::new(|cc| Ok(Box::new(Root::new(cc.egui_ctx.clone())))))
}

/// The GIF or video in the large view.
enum Playing {
	Loading(String),
	Anim {
		url: String,
		frames: Vec<(TextureHandle, u32)>,
		total: u32,
		clock: std::time::Instant,
		/// Where it was paused, in milliseconds into the loop.
		paused: Option<u32>,
	},
	Video {
		url: String,
		video: Box<media::Video>,
		tex: Option<TextureHandle>,
		seen: u64,
	},
	Failed(String, String),
}

impl Playing {
	fn url(&self) -> &str {
		match self {
			Playing::Loading(u) | Playing::Failed(u, _) | Playing::Anim { url: u, .. } | Playing::Video { url: u, .. } => u,
		}
	}
}

enum Tex {
	Loading,
	Ready(TextureHandle),
	Failed,
}

/// The selected post's tags and links from the graph, reloaded after each edit.
struct Detail {
	key: String,
	mine: Vec<ItemTag>,
	links: Vec<ItemLink>,
	link_names: Vec<String>,
	/// Links made before that fit this post, when they aren't filled in on their own.
	offered: Vec<LinkSuggestion>,
	kind: String,
	picked: BTreeSet<String>,
	new_tag: String,
	new_link_name: String,
	error: Option<String>,
}

enum Action {
	ToggleFavorite(usize),
	/// Pauses or plays the GIF or video in the large view.
	TogglePlay,
	ToggleMute,
	/// Jumps the video to this many seconds in.
	Seek(f64),
	/// Posts like this one: by shared tags, or (true) other versions of the picture.
	Similar(usize, bool),
	/// Your favorites, on every site.
	ShowFavorites,
	/// Shows a post large.
	View(usize),
	CloseViewer,
	/// Moves to the post this many places on (the one shown large, or selected).
	Step(i64),
	/// Keep, reject or put back one relation in the index: dog eat bone.
	SetRelation { subject: String, verb: String, object: String, state: RelationState },
	RunSaved(i64),
	DeleteSaved(i64),
	OpenSave,
	/// Opens Groups & relations, with a tag ready to put in a group.
	OpenGroups(Option<String>),
	KeepLink(i64),
	RejectLink(i64),
	AddOffered(String, Vec<String>),
	Search,
	More,
	Select(usize),
	/// Adds a tag to the search, or takes it out if it's in already.
	Include(String),
	/// Leaves a tag out of the search, or stops leaving it out.
	Exclude(String),
	SetSearch(String),
	Accept(String),
	VoteTag(String, i8),
	AddLink,
	RemoveLink(i64),
	AddLinkName,
	Open(String),
	Translate(String),
}

struct App {
	settings: Settings,
	settings_open: bool,
	translations: Translations,
	groups: Groups,
	sites: Vec<Arc<dyn Site>>,
	store: Arc<Mutex<Store>>,
	worker: Worker,
	query: String,
	generation: u64,
	/// A search has run, so an empty grid means "nothing matched".
	searched: bool,
	posts: Vec<Post>,
	busy: bool,
	done: bool,
	checked: usize,
	error: Option<String>,
	queries: Vec<String>,
	notes: Vec<String>,
	textures: HashMap<String, Tex>,
	suggestions: Vec<TagSuggestion>,
	suggest_word: String,
	selected: Option<usize>,
	detail: Option<Detail>,
	tags: TagsPanel,
	/// The saved search the results came from, and the newest post per site
	/// its last run saw; newer posts get a NEW mark.
	/// A relation asked about with `relation`, and what the index gave for it.
	asked: Option<bettersearch_core::search::AskedHere>,
	running_saved: Option<i64>,
	new_after: HashMap<String, u64>,
	save_open: bool,
	save_name: String,
	save_note: Option<String>,
	/// The tag under the mouse in the tags panel; posts without it are dimmed.
	hover_tag: Option<String>,
	saver: Saver,
	/// Something of yours changed, so the vault is saved soon rather than later.
	urgent: bool,
	/// Settings asked to lock; the PIN screen takes over after this frame.
	pub lock_requested: bool,
	new_pin: String,
	new_pin_again: String,
	pin_note: Option<String>,
	/// Answers from "Check logins", one line per site, filled in as they come.
	login_checks: Arc<Mutex<Vec<LoginCheck>>>,
	/// Keys of your favorites (kept in the vault), to mark them.
	favorites: HashSet<String>,
	/// The post shown large, if any; whether as the original file, and at full size.
	viewer: Option<usize>,
	viewer_original: bool,
	viewer_actual: bool,
	/// Thumbnails per row, for moving with ↑ ↓, and rows that fit on screen.
	grid_cols: usize,
	grid_rows: usize,
	/// Pages fetched on their own to fill the screen, since the last search.
	auto_loads: u8,
	scroll_to_selected: bool,
	/// Big images (post panel, viewer) in the order they were shown; only the
	/// last few are kept in memory.
	big_textures: std::collections::VecDeque<String>,
	/// What the grid shows when it isn't your search: "Posts like #123".
	showing: Option<String>,
	/// The GIF or video playing in the large view.
	playing: Option<Playing>,
	ctx: egui::Context,
}

fn make_sites(s: &Settings) -> Vec<Arc<dyn Site>> {
	// Your name goes only to e621/e926, which ask for it; other sites get the plain app name.
	let ua = user_agent(&s.contact);
	let anon = user_agent("");
	let mut out: Vec<Arc<dyn Site>> = Vec::new();
	// With an API key but no login name, the name at the top of Settings is the e621 username.
	let e621_user = if s.e621_user.trim().is_empty() { &s.contact } else { &s.e621_user };
	if let Ok(site) = E621::e926(&ua) {
		out.push(Arc::new(site.with_login(e621_user, &s.e621_key)));
	}
	if let Ok(site) = E621::e621(&ua) {
		out.push(Arc::new(site.with_login(e621_user, &s.e621_key)));
	}
	if let Ok(site) = Danbooru::danbooru(&anon) {
		out.push(Arc::new(site.with_login(&s.danbooru_user, &s.danbooru_key)));
	}
	if let Ok(site) = Gelbooru::safebooru(&anon) {
		out.push(Arc::new(site));
	}
	if let Ok(site) = Gelbooru::gelbooru(&anon) {
		out.push(Arc::new(site.with_login(&s.gelbooru_user, &s.gelbooru_key)));
	}
	out
}

/// The word being typed at the end of the search, without `-`, `~`, `(` or `kind[`.
fn typing_word(query: &str) -> Option<(usize, &str)> {
	if query.ends_with(char::is_whitespace) {
		return None;
	}
	let start = query.rfind(char::is_whitespace).map_or(0, |i| i + 1);
	let mut w = &query[start..];
	let mut at = start;
	if let Some(open) = w.find('[') {
		at += open + 1;
		w = &w[open + 1..];
	}
	let trimmed = w.trim_start_matches(['-', '~', '(']);
	at += w.len() - trimmed.len();
	(trimmed.chars().count() >= 2 && !trimmed.contains(':') && !trimmed.contains(['(', ')', ']'])).then_some((at, trimmed))
}

impl App {
	fn new(ctx: egui::Context, mut store: Store, saver: Saver) -> App {
		let settings = Settings::load(&store);
		store.auto_links = settings.auto_links;
		let store = Arc::new(Mutex::new(store));
		let favorites = store.lock().unwrap().favorite_keys().unwrap_or_default();
		let worker = Worker::spawn(ctx.clone(), store.clone());
		App {
			sites: make_sites(&settings),
			query: settings.last_search.clone(),
			settings,
			settings_open: false,
			translations: Translations::default(),
			groups: Groups::default(),
			store,
			worker,
			generation: 0,
			searched: false,
			posts: Vec::new(),
			busy: false,
			done: true,
			checked: 0,
			error: None,
			queries: Vec::new(),
			notes: Vec::new(),
			textures: HashMap::new(),
			suggestions: Vec::new(),
			suggest_word: String::new(),
			selected: None,
			detail: None,
			tags: TagsPanel::default(),
			asked: None,
			running_saved: None,
			new_after: HashMap::new(),
			save_open: false,
			save_name: String::new(),
			save_note: None,
			hover_tag: None,
			saver,
			urgent: false,
			lock_requested: false,
			new_pin: String::new(),
			new_pin_again: String::new(),
			pin_note: None,
			login_checks: Arc::new(Mutex::new(Vec::new())),
			favorites,
			viewer: None,
			viewer_original: false,
			viewer_actual: false,
			grid_cols: 1,
			grid_rows: 1,
			auto_loads: 0,
			scroll_to_selected: false,
			big_textures: std::collections::VecDeque::new(),
			showing: None,
			playing: None,
			ctx: ctx.clone(),
		}
	}

	fn save_settings(&mut self) {
		let mut s = self.store.lock().unwrap();
		s.auto_links = self.settings.auto_links;
		self.settings.save(&s);
		self.urgent = true;
	}

	/// Writes the vault one last time; called on lock and exit.
	fn shutdown(&mut self) {
		self.save_settings();
		self.saver.flush(&self.store);
	}

	/// The sites ticked, in menu order; the first if none are.
	fn current_sites(&self) -> Vec<Arc<dyn Site>> {
		let picked: Vec<_> = self.sites.iter().filter(|s| self.settings.sites.iter().any(|id| id == s.id())).cloned().collect();
		if picked.is_empty() { self.sites.first().cloned().into_iter().collect() } else { picked }
	}

	fn site_by_id(&self, id: &str) -> Option<Arc<dyn Site>> {
		self.sites.iter().find(|s| s.id() == id).cloned()
	}

	fn search(&mut self) {
		let sites = self.current_sites();
		self.search_on(sites);
	}

	fn search_on(&mut self, sites: Vec<Arc<dyn Site>>) {
		if sites.is_empty() {
			return;
		}
		self.new_results();
		self.settings.last_search = self.query.clone();
		self.save_settings();
		if let Ok(s) = self.store.lock() {
			let ids: Vec<String> = sites.iter().map(|s| s.id().to_string()).collect();
			let _ = s.add_history(&self.query, &ids);
		}
		self.worker.send(Job::Start {
			sites,
			text: self.query.clone(),
			ratings: self.settings.ratings.clone(),
			generation: self.generation,
			want: PAGE,
		});
	}

	/// Posts like the selected one, by shared telling tags or (`versions`) other
	/// versions of the picture. They replace the results; the search box stays.
	fn similar(&mut self, i: usize, versions: bool) {
		let Some(post) = self.posts.get(i).cloned() else { return };
		let Some(site) = self.site_by_id(&post.site) else { return };
		self.new_results();
		self.showing = Some(if versions { format!("Other versions of #{}", post.id) } else { format!("Posts like #{}", post.id) });
		self.worker.send(Job::Similar {
			site,
			post: Box::new(post),
			ratings: self.settings.ratings.clone(),
			generation: self.generation,
			versions,
		});
	}

	/// Clears the results for a new search (or similar posts).
	fn new_results(&mut self) {
		self.generation += 1;
		self.playing = None;
		self.auto_loads = 0;
		self.viewer = None;
		self.showing = None;
		self.running_saved = None;
		self.new_after.clear();
		self.asked = None;
		self.posts.clear();
		self.selected = None;
		self.detail = None;
		self.error = None;
		self.queries.clear();
		self.notes.clear();
		self.suggestions.clear();
		self.busy = true;
		self.done = false;
		self.checked = 0;
		self.searched = true;
	}

	fn more(&mut self) {
		if !self.busy && !self.done {
			self.busy = true;
			self.worker.send(Job::More { generation: self.generation, want: PAGE });
		}
	}

	fn receive(&mut self, ctx: &egui::Context) {
		while let Ok(msg) = self.worker.msgs.try_recv() {
			match msg {
				Msg::Planned { generation, queries, notes, asked } if generation == self.generation => {
					self.asked = asked;
					self.queries = queries;
					self.notes = notes;
				}
				Msg::Posts { generation, posts, done, checked, more } if generation == self.generation => {
					if let Some(id) = self.running_saved {
						let mut newest: HashMap<String, u64> = HashMap::new();
						for p in &posts {
							let n = newest.entry(p.site.clone()).or_insert(0);
							*n = (*n).max(p.id);
						}
						let _ = self.store.lock().unwrap().mark_seen(id, &newest);
						self.urgent = true;
					}
					self.posts.extend(posts);
					self.done = done;
					self.checked = checked;
					self.busy = more;
					// Still room on screen once this page is in: fetch more, a few times.
					let shown = (0..self.posts.len()).filter(|&i| self.visible(i)).count();
					if !more && !done && shown < self.grid_cols * self.grid_rows && self.auto_loads < 3 {
						self.auto_loads += 1;
						self.more();
					}
				}
				Msg::Failed { generation, error } if generation == self.generation => {
					self.error = Some(error);
					self.busy = false;
					self.done = true;
				}
				Msg::Suggestions { word, list } => {
					if typing_word(&self.query).is_some_and(|(_, w)| w == word) {
						self.suggestions = list;
					}
				}
				Msg::Animation { url, frames } => {
					if self.playing.as_ref().is_some_and(|p| p.url() == url) {
						self.playing = Some(match frames {
							Ok(frames) => {
								let total = frames.iter().map(|(_, ms)| ms).sum::<u32>().max(1);
								let frames = frames
									.into_iter()
									.enumerate()
									.map(|(i, (img, ms))| (ctx.load_texture(format!("{url}#{i}"), img, egui::TextureOptions::LINEAR), ms))
									.collect();
								Playing::Anim { url, frames, total, clock: std::time::Instant::now(), paused: None }
							}
							Err(e) => Playing::Failed(url, e),
						});
					}
				}
				Msg::Video { url, video } => {
					if self.playing.as_ref().is_some_and(|p| p.url() == url) {
						self.playing = Some(match video {
							Ok(mut video) => {
								video.play_from(0.0, ctx);
								Playing::Video { url, video, tex: None, seen: 0 }
							}
							Err(e) => Playing::Failed(url, e),
						});
					}
				}
				Msg::Image { url, image } => {
					let tex = match image {
						Ok(img) => Tex::Ready(ctx.load_texture(&url, img, egui::TextureOptions::LINEAR)),
						Err(_) => Tex::Failed,
					};
					self.textures.insert(url, tex);
				}
				_ => {}
			}
		}
	}

	/// An image's texture, fetched with the client of the site it's from.
	fn texture(&mut self, site: &str, url: &str) -> Option<&TextureHandle> {
		if !self.textures.contains_key(url) {
			if let Some(site) = self.site_by_id(site) {
				self.textures.insert(url.to_string(), Tex::Loading);
				self.worker.image(site, url.to_string());
			}
			return None;
		}
		match self.textures.get(url) {
			Some(Tex::Ready(t)) => Some(t),
			_ => None,
		}
	}

	/// A big image's texture; only the last few big ones stay in memory.
	fn big_texture(&mut self, site: &str, url: &str) -> Option<TextureHandle> {
		if !self.big_textures.iter().any(|u| u == url) {
			self.big_textures.push_back(url.to_string());
			while self.big_textures.len() > 6 {
				if let Some(old) = self.big_textures.pop_front() {
					self.textures.remove(&old);
				}
			}
		}
		self.texture(site, url).cloned()
	}

	/// Whether a site lets this app load its images (only if it needs a referer, like gelbooru.com).
	fn site_shows(&self, id: &str) -> bool {
		self.site_by_id(id).is_none_or(|s| s.shows_images())
	}

	/// Whether a post shows in the grid (posts without images can be hidden).
	fn visible(&self, i: usize) -> bool {
		self.posts.get(i).is_some_and(|p| self.settings.show_imageless || !imageless(p))
	}

	/// Moves the selection (and the large view) `by` visible posts, loading more at the end.
	fn step(&mut self, by: i64) {
		let Some(mut i) = self.viewer.or(self.selected) else {
			if let Some(first) = (0..self.posts.len()).find(|&i| self.visible(i)) {
				self.select(first);
			}
			return;
		};
		for _ in 0..by.unsigned_abs() {
			let next = if by > 0 {
				(i + 1..self.posts.len()).find(|&j| self.visible(j))
			} else {
				(0..i).rev().find(|&j| self.visible(j))
			};
			match next {
				Some(j) => i = j,
				None => {
					if by > 0 {
						self.more();
					}
					break;
				}
			}
		}
		self.select(i);
	}

	fn select(&mut self, i: usize) {
		self.selected = Some(i);
		self.scroll_to_selected = true;
		if self.viewer.is_some() {
			self.viewer = Some(i);
		}
		self.load_detail(None);
	}

	/// Keys, when no text field is being typed in: ← → ↑ ↓ move, Enter shows
	/// large, Esc closes or deselects, F favorites, Z switches fit and full size.
	fn keys(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
		if ctx.egui_wants_keyboard_input() || self.settings_open {
			return;
		}
		use egui::Key;
		let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
		let row = self.grid_cols.max(1) as i64;
		if pressed(Key::ArrowRight) {
			actions.push(Action::Step(1));
		}
		if pressed(Key::ArrowLeft) {
			actions.push(Action::Step(-1));
		}
		if self.viewer.is_none() {
			if pressed(Key::ArrowDown) {
				actions.push(Action::Step(row));
			}
			if pressed(Key::ArrowUp) {
				actions.push(Action::Step(-row));
			}
		}
		// Enter in the search box searches; it isn't also "show large".
		if pressed(Key::Enter)
			&& !actions.iter().any(|a| matches!(a, Action::Search))
			&& let Some(i) = self.selected
		{
			actions.push(Action::View(i));
		}
		if pressed(Key::Escape) {
			if self.viewer.is_some() {
				actions.push(Action::CloseViewer);
			} else {
				self.selected = None;
				self.detail = None;
			}
		}
		if pressed(Key::F)
			&& let Some(i) = self.viewer.or(self.selected)
		{
			actions.push(Action::ToggleFavorite(i));
		}
		if pressed(Key::Z) && self.viewer.is_some() {
			self.viewer_actual = !self.viewer_actual;
		}
		if self.viewer.is_some() && self.playing.is_some() {
			if pressed(Key::Space) {
				actions.push(Action::TogglePlay);
			}
			if pressed(Key::M) {
				actions.push(Action::ToggleMute);
			}
		}
	}

	fn load_detail(&mut self, keep: Option<Detail>) {
		let Some(post) = self.selected.and_then(|i| self.posts.get(i)) else {
			self.detail = None;
			return;
		};
		let key = post.key();
		let store = self.store.lock().unwrap();
		// Links made since this post loaded fill in now; with that off, they're offered.
		let _ = store.auto_link(std::slice::from_ref(post));
		let offered = if store.auto_links { Vec::new() } else { store.suggest_links(post).unwrap_or_default() };
		let g = store.graph();
		let mut d = Detail {
			offered,
			mine: g.item_tags(&key, &store.actor).unwrap_or_default(),
			links: g.item_links(&key, &store.actor).unwrap_or_default(),
			link_names: g.link_names().unwrap_or_default(),
			kind: "relation".into(),
			picked: BTreeSet::new(),
			new_tag: String::new(),
			new_link_name: String::new(),
			error: None,
			key,
		};
		if let Some(old) = keep.filter(|o| o.key == d.key) {
			d.kind = old.kind;
			d.error = old.error;
		}
		if !d.link_names.contains(&d.kind) {
			d.kind = d.link_names.get(1).or(d.link_names.first()).cloned().unwrap_or_default();
		}
		self.detail = Some(d);
	}

	fn apply(&mut self, actions: Vec<Action>) {
		for a in actions {
			match a {
				Action::Search => self.search(),
				Action::ToggleFavorite(i) => {
					if let Some(post) = self.posts.get(i).cloned() {
						let key = post.key();
						let on = !self.favorites.contains(&key);
						match self.store.lock().unwrap().set_favorite(&post, on) {
							Ok(()) if on => {
								self.favorites.insert(key);
							}
							Ok(()) => {
								self.favorites.remove(&key);
							}
							Err(e) => self.error = Some(e.to_string()),
						}
						self.urgent = true;
					}
				}
				Action::ShowFavorites => {
					self.query = "favorited:yes".into();
					let all = self.sites.clone();
					self.search_on(all);
				}
				Action::View(i) => {
					self.viewer = Some(i);
					self.viewer_original = false;
					self.select(i);
				}
				Action::CloseViewer => {
					self.viewer = None;
					self.playing = None;
					self.scroll_to_selected = true;
				}
				Action::Step(by) => self.step(by),
				Action::TogglePlay => match &mut self.playing {
					Some(Playing::Anim { clock, total, paused, .. }) => match paused.take() {
						Some(at) => *clock = std::time::Instant::now() - std::time::Duration::from_millis(at as u64),
						None => *paused = Some((clock.elapsed().as_millis() as u64 % *total as u64) as u32),
					},
					Some(Playing::Video { video, .. }) => video.toggle_pause(&self.ctx),
					_ => {}
				},
				Action::ToggleMute => {
					if let Some(Playing::Video { video, .. }) = &mut self.playing {
						video.toggle_mute(&self.ctx);
					}
				}
				Action::Seek(at) => {
					if let Some(Playing::Video { video, .. }) = &mut self.playing {
						video.play_from(at, &self.ctx);
					}
				}
				Action::Similar(i, versions) => self.similar(i, versions),
				Action::SetRelation { subject, verb, object, state } => {
					if let Err(e) = self.store.lock().unwrap().set_relation(&subject, &verb, &object, state) {
						self.error = Some(e.to_string());
					}
					self.urgent = true;
				}
				Action::RunSaved(id) => {
					let saved = self.store.lock().unwrap().saved_searches().unwrap_or_default().into_iter().find(|s| s.id == id);
					if let Some(s) = saved {
						self.query = s.query;
						if s.sites.iter().any(|id| self.sites.iter().any(|x| x.id() == id)) {
							self.settings.sites = s.sites;
						}
						if !s.ratings.is_empty() {
							self.settings.ratings = s.ratings;
						}
						self.search();
						self.running_saved = Some(id);
						self.new_after = s.seen;
					}
				}
				Action::DeleteSaved(id) => {
					let _ = self.store.lock().unwrap().delete_search(id);
					if self.running_saved == Some(id) {
						self.running_saved = None;
						self.new_after.clear();
					}
					self.urgent = true;
				}
				Action::OpenSave => {
					self.save_open = true;
					self.save_note = None;
					self.save_name = self.query.chars().take(40).collect();
				}
				Action::More => self.more(),
				Action::Select(i) => {
					self.selected = Some(i);
					self.load_detail(None);
				}
				// Clicking a tag again takes it back out, so it's never added twice.
				Action::Include(t) => {
					let on = tags_panel::search_tags(&self.query).into_iter().find(|(n, _)| *n == t).map(|(_, on)| on);
					self.query = tags_panel::set_tag(&self.query, &t, if on == Some(true) { None } else { Some(true) });
				}
				Action::Exclude(t) => {
					let on = tags_panel::search_tags(&self.query).into_iter().find(|(n, _)| *n == t).map(|(_, on)| on);
					self.query = tags_panel::set_tag(&self.query, &t, if on == Some(false) { None } else { Some(false) });
				}
				Action::SetSearch(q) => {
					self.query = q;
					self.search();
				}
				Action::Accept(name) => {
					if let Some((at, _)) = typing_word(&self.query) {
						self.query.truncate(at);
						self.query.push_str(&name);
						self.query.push(' ');
					}
					self.suggestions.clear();
				}
				Action::OpenGroups(tag) => {
					self.groups.open = true;
					if let Some(t) = tag {
						self.groups.tag = t;
					}
				}
				Action::Translate(tag) => {
					self.translations.open = true;
					self.translations.tag = tag;
				}
				// Only web links from site data go to the browser: no files, no other schemes,
				// nothing that could read as an option.
				Action::Open(url) => {
					if url.starts_with("https://") || url.starts_with("http://") {
						let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
					}
				}
				Action::VoteTag(..)
				| Action::AddLink
				| Action::RemoveLink(_)
				| Action::AddLinkName
				| Action::KeepLink(_)
				| Action::RejectLink(_)
				| Action::AddOffered(..) => self.edit_graph(a),
			}
		}
	}

	fn edit_graph(&mut self, a: Action) {
		let Some(mut d) = self.detail.take() else {
			return;
		};
		let site_tags: Vec<String> = self.selected.and_then(|i| self.posts.get(i)).map(|p| p.tags.clone()).unwrap_or_default();
		let result = {
			let store = self.store.lock().unwrap();
			let g = store.graph();
			let actor = store.actor.as_str();
			match a {
				Action::VoteTag(name, vote) => g.vote_tag(&d.key, &name, actor, vote),
				Action::AddLink => {
					let members: Vec<String> = d.picked.iter().cloned().collect();
					let mut on_item = site_tags;
					on_item.extend(d.mine.iter().map(|t| t.name.clone()));
					g.add_link(&d.key, &d.kind, &members, actor, &on_item).map(|_| ())
				}
				Action::RemoveLink(id) => g.remove_link(id, actor),
				Action::KeepLink(id) => g.vote_link(id, actor, 1),
				Action::RejectLink(id) => g.vote_link(id, actor, -1),
				Action::AddOffered(kind, members) => {
					let mut on_item = site_tags;
					on_item.extend(d.mine.iter().map(|t| t.name.clone()));
					g.add_link(&d.key, &kind, &members, actor, &on_item).map(|_| ())
				}
				Action::AddLinkName => g.add_link_name(&d.new_link_name, actor).map(|_| d.kind = g.normalize(&d.new_link_name)),
				_ => Ok(()),
			}
		};
		d.error = result.err().map(|e| e.to_string());
		self.urgent = true;
		self.load_detail(Some(d));
	}

	fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
		let mut toggled_tags = false;
		ui.add_space(4.0);
		ui.horizontal(|ui| {
			if ui.selectable_label(self.settings.show_tags, "☰ Tags").on_hover_text("Show the tags panel").clicked() {
				self.settings.show_tags = !self.settings.show_tags;
				toggled_tags = true;
			}
			let names: Vec<(String, String)> = self.sites.iter().map(|s| (s.id().to_string(), s.name().to_string())).collect();
			let picked: Vec<String> = self.current_sites().iter().map(|s| s.name().to_string()).collect();
			let current = match picked.as_slice() {
				[] => "pick a site".to_string(),
				[one] => one.clone(),
				many => format!("{} sites", many.len()),
			};
			egui::ComboBox::from_id_salt("site")
				.selected_text(current)
				.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
				.show_ui(ui, |ui| {
					ui.label(RichText::new("Tick several to search them together").weak());
					for (id, name) in &names {
						let mut on = self.settings.sites.contains(id);
						if ui.checkbox(&mut on, name).changed() {
							self.settings.sites.retain(|s| s != id);
							if on {
								self.settings.sites.push(id.clone());
							}
							self.save_settings();
						}
					}
					ui.separator();
					if ui.button("Tag translations…").clicked() {
						self.translations.open = true;
						ui.close();
					}
				});
			// Laid out from the right, so Settings and the buttons always fit and
			// the search box takes whatever width is left.
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				if ui.button("⚙").on_hover_text("Settings").clicked() {
					self.settings_open = true;
				}
				let rated = |r: Rating| self.settings.ratings.contains(&r);
				let summary = match Rating::ALL.iter().filter(|r| rated(**r)).count() {
					3 => "All ratings".to_string(),
					0 => "No rating".to_string(),
					_ => Rating::ALL.iter().filter(|r| rated(**r)).map(|r| r.label()).collect::<Vec<_>>().join(" + "),
				};
				egui::ComboBox::from_id_salt("ratings")
					.selected_text(summary)
					.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
					.show_ui(ui, |ui| {
						for r in Rating::ALL {
							let mut on = self.settings.ratings.contains(&r);
							if ui.checkbox(&mut on, r.label()).changed() {
								self.settings.ratings.retain(|x| *x != r);
								if on {
									self.settings.ratings.push(r);
									self.settings.ratings.sort();
								}
								self.save_settings();
							}
						}
					});
				if ui.button("♥").on_hover_text("Your favorites (kept on this computer)").clicked() {
					actions.push(Action::ShowFavorites);
				}
				if ui.add_enabled(!self.query.trim().is_empty(), egui::Button::new("☆ Save")).on_hover_text("Save this search").clicked() {
					actions.push(Action::OpenSave);
				}
				let search_clicked = ui.button("Search").clicked();
				let edit = egui::TextEdit::singleline(&mut self.query)
					.hint_text("fox (smile | grin) -sketch   ·   dog eat")
					.desired_width(ui.available_width());
				let response = ui.add(edit);
				if response.changed() {
					match typing_word(&self.query) {
						Some((_, w)) if w != self.suggest_word => {
							self.suggest_word = w.to_string();
							if let Some(site) = self.current_sites().into_iter().next() {
								self.worker.suggest(site, w.to_string());
							}
						}
						None => {
							self.suggest_word.clear();
							self.suggestions.clear();
						}
						_ => {}
					}
				}
				let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				let tab = response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Tab));
				if tab && let Some(first) = self.suggestions.first() {
					actions.push(Action::Accept(first.name.clone()));
					response.request_focus();
				}
				if search_clicked || enter {
					actions.push(Action::Search);
				}
			});
		});
		if !self.suggestions.is_empty() && typing_word(&self.query).is_some() {
			ui.horizontal_wrapped(|ui| {
				ui.label(RichText::new("Tab ⇥").weak());
				for s in self.suggestions.iter().take(10) {
					let label = match &s.alias {
						Some(a) => format!("{a} → {} ({})", s.name, short_count(s.count)),
						None => format!("{} ({})", s.name, short_count(s.count)),
					};
					if ui.button(RichText::new(label).color(category_color(&s.category))).clicked() {
						actions.push(Action::Accept(s.name.clone()));
					}
				}
			});
		}
		if !self.queries.is_empty() || !self.notes.is_empty() {
			egui::CollapsingHeader::new(RichText::new("How this search runs").weak()).id_salt("plan").show(ui, |ui| {
				for q in &self.queries {
					ui.monospace(if q.is_empty() { "(newest posts)" } else { q });
				}
				for n in &self.notes {
					ui.label(RichText::new(n).weak());
				}
			});
		}
		ui.add_space(2.0);
		if toggled_tags {
			self.save_settings();
		}
	}

	fn grid(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
		if let Some(what) = &self.showing {
			ui.horizontal(|ui| {
				ui.strong(what);
				if ui.button("Back to search").on_hover_text("Run the search in the box again").clicked() {
					actions.push(Action::Search);
				}
			});
		}
		if let Some(e) = &self.error {
			ui.colored_label(Color32::from_rgb(220, 90, 90), e);
		}
		if self.posts.is_empty() {
			ui.centered_and_justified(|ui| {
				ui.label(if self.busy {
					"Searching…".to_string()
				} else if self.searched && self.error.is_none() {
					format!("Nothing matched (checked {} posts).", self.checked)
				} else if self.error.is_some() {
					"Change the search and press Enter.".to_string()
				} else {
					"Type a search and press Enter.".to_string()
				});
			});
			return;
		}
		egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
			self.grid_cols = ((ui.available_width() + 6.0) / (THUMB + 6.0)).floor().max(1.0) as usize;
			self.grid_rows = (ui.available_height() / (THUMB + 6.0)).ceil().max(1.0) as usize;
			ui.horizontal_wrapped(|ui| {
				ui.spacing_mut().item_spacing = Vec2::splat(6.0);
				for i in 0..self.posts.len() {
					if !self.settings.show_imageless && imageless(&self.posts[i]) {
						continue;
					}
					let site = self.posts[i].site.clone();
					// Gelbooru refuses apps its images; don't ask for them.
					let url = self.posts[i].preview_url.clone().filter(|_| self.site_shows(&site));
					let tex = url.as_deref().and_then(|u| self.texture(&site, u)).cloned();
					let failed = url.as_deref().is_some_and(|u| matches!(self.textures.get(u), Some(Tex::Failed)));
					let (rect, response) = ui.allocate_exact_size(Vec2::splat(THUMB), Sense::click());
					let painter = ui.painter_at(rect);
					painter.rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
					match &tex {
						Some(t) => {
							let size = t.size_vec2();
							let scale = (THUMB / size.x).min(THUMB / size.y);
							let img = egui::Rect::from_center_size(rect.center(), size * scale);
							painter.image(
								t.id(),
								img,
								egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
								Color32::WHITE,
							);
						}
						None => {
							let p = &self.posts[i];
							let text = if imageless(p) {
								format!("{} hides this\nwhen logged out", p.site)
							} else if !self.site_shows(&p.site) {
								format!("{} only shows\nits images on\nits own website", p.site)
							} else if failed {
								"couldn't load".to_string()
							} else if p.preview_url.is_none() {
								format!("{} · no preview", p.ext)
							} else {
								"…".into()
							};
							painter.text(
								rect.center(),
								egui::Align2::CENTER_CENTER,
								text,
								egui::FontId::proportional(13.0),
								ui.visuals().weak_text_color(),
							);
						}
					}
					let p = &self.posts[i];
					if matches!(p.ext.as_str(), "webm" | "mp4" | "swf" | "gif") {
						painter.text(
							rect.left_top() + Vec2::new(6.0, 4.0),
							egui::Align2::LEFT_TOP,
							p.ext.to_uppercase(),
							egui::FontId::monospace(11.0),
							Color32::YELLOW,
						);
					}
					if let Some(t) = &self.hover_tag
						&& !p.has_tag(t)
					{
						painter.rect_filled(rect, 4.0, Color32::from_black_alpha(190));
					}
					if self.new_after.get(&p.site).is_some_and(|seen| p.id > *seen) {
						let badge = egui::Rect::from_min_size(rect.right_top() + Vec2::new(-44.0, 6.0), Vec2::new(38.0, 18.0));
						painter.rect_filled(badge, 4.0, Color32::from_rgb(220, 70, 90));
						painter.text(badge.center(), egui::Align2::CENTER_CENTER, "NEW", egui::FontId::proportional(11.0), Color32::WHITE);
					}
					if self.settings.sites.len() > 1 {
						painter.text(
							rect.right_bottom() - Vec2::new(6.0, 4.0),
							egui::Align2::RIGHT_BOTTOM,
							&p.site,
							egui::FontId::proportional(11.0),
							Color32::from_white_alpha(200),
						);
					}
					if self.favorites.contains(&p.key()) {
						painter.text(rect.left_bottom() + Vec2::new(6.0, -4.0), egui::Align2::LEFT_BOTTOM, "♥", egui::FontId::proportional(16.0), Color32::from_rgb(230, 90, 120));
					}
					if self.selected == Some(i) {
						if self.scroll_to_selected {
							ui.scroll_to_rect(rect, None);
							self.scroll_to_selected = false;
						}
						painter.rect_stroke(rect, 4.0, egui::Stroke::new(3.0, ui.visuals().selection.bg_fill), egui::StrokeKind::Inside);
					} else if response.hovered() {
						painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, ui.visuals().weak_text_color()), egui::StrokeKind::Inside);
					}
					let response = response.on_hover_text(format!("#{} · score {} · {}×{}", p.id, p.score, p.width, p.height));
					if response.double_clicked() {
						actions.push(Action::View(i));
					} else if response.clicked() {
						actions.push(Action::Select(i));
					}
					response.context_menu(|ui| {
						if ui.button(if self.favorites.contains(&self.posts[i].key()) { "Remove from favorites" } else { "Add to favorites" }).clicked() {
							actions.push(Action::ToggleFavorite(i));
						}
						if ui.button("View large").clicked() {
							actions.push(Action::View(i));
						}
					});
				}
			});
			ui.add_space(12.0);
			ui.horizontal(|ui| {
				let hidden = if self.settings.show_imageless { 0 } else { self.posts.iter().filter(|p| imageless(p)).count() };
				ui.label(RichText::new(format!("{} shown · {} checked", self.posts.len() - hidden, self.checked)).weak());
				if hidden > 0 {
					let sites: BTreeSet<&str> = self.posts.iter().filter(|p| imageless(p)).map(|p| p.site.as_str()).collect();
					let sites = sites.into_iter().collect::<Vec<_>>().join(", ");
					ui.label(RichText::new(format!("· {hidden} hidden by {sites} when logged out"))
						.color(Color32::from_rgb(230, 170, 60)))
						.on_hover_text("The site sent these without images because you're not logged in. Add your login in ⚙ Settings to see them.");
				}
				if self.running_saved.is_some() {
					let new = self.posts.iter().filter(|p| self.new_after.get(&p.site).is_some_and(|seen| p.id > *seen)).count();
					ui.label(RichText::new(format!("· {new} new since last time")).color(Color32::from_rgb(220, 70, 90)));
				}
				if self.busy {
					ui.spinner();
				} else if !self.done && ui.button("Load more").clicked() {
					actions.push(Action::More);
				}
			});
			ui.add_space(12.0);
		});
	}

	fn detail(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
		let Some(post) = self.selected.and_then(|i| self.posts.get(i)).cloned() else {
			return;
		};
		let query = self.query.clone();
		egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
			let url = post.sample_url.clone().or(post.preview_url.clone());
			if !self.site_shows(&post.site) {
				ui.label(
					RichText::new(format!(
						"{} only lets its own website show its images (hotlink protection), so it can't be shown here. Open page shows it in your browser.",
						post.site
					))
					.weak(),
				);
			} else if let Some(t) = url.as_deref().and_then(|u| self.big_texture(&post.site, u)) {
				let w = ui.available_width();
				let size = t.size_vec2();
				ui.add(egui::Image::from_texture(&t).fit_to_exact_size(Vec2::new(w, size.y * w / size.x)));
			} else if url.is_some() {
				ui.spinner();
			}
			ui.horizontal(|ui| {
				ui.strong(format!("#{}", post.id));
				ui.label(format!(
					"{} · score {} · ♥ {} · {}×{} {}",
					post.rating.label(),
					post.score,
					post.fav_count,
					post.width,
					post.height,
					post.ext
				));
			});
			ui.horizontal(|ui| {
				let fav = self.favorites.contains(&post.key());
				let heart = RichText::new(if fav { "♥ Favorite" } else { "♡ Favorite" });
				let heart = if fav { heart.color(Color32::from_rgb(230, 90, 120)) } else { heart };
				if ui.button(heart).on_hover_text("Kept on this computer, encrypted (F)").clicked()
					&& let Some(i) = self.selected
				{
					actions.push(Action::ToggleFavorite(i));
				}
				if ui.button("View large").on_hover_text("Or double-click it · Enter").clicked()
					&& let Some(i) = self.selected
				{
					actions.push(Action::View(i));
				}
				if ui.button("Open page").clicked() {
					actions.push(Action::Open(post.page_url.clone()));
				}
				if let Some(f) = &post.file_url
					&& ui.button("Open file").clicked()
				{
					actions.push(Action::Open(f.clone()));
				}
			});
			ui.horizontal(|ui| {
				if ui.button("Find similar").on_hover_text("Posts sharing the most telling tags with this one").clicked()
					&& let Some(i) = self.selected
				{
					actions.push(Action::Similar(i, false));
				}
				// e621's picture search is open without an account; Danbooru's isn't.
				if matches!(post.site.as_str(), "e621" | "e926")
					&& ui.button("Other versions").on_hover_text("Edits, crops and reposts of this picture, from e621's picture search").clicked()
					&& let Some(i) = self.selected
				{
					actions.push(Action::Similar(i, true));
				}
			});
			let Some(d) = self.detail.as_mut() else { return };
			if let Some(e) = &d.error {
				ui.colored_label(Color32::from_rgb(220, 90, 90), e);
			}

			ui.separator();
			ui.heading("Links");
			let shown: Vec<&ItemLink> = d.links.iter().filter(|l| l.score > 0).collect();
			if shown.is_empty() && d.offered.is_empty() {
				ui.label(
					RichText::new(
						"Tick tags below, then link them: e.g. relation: rabbit · eat · carrot. Posts with the same tags get the link filled in.",
					)
					.weak(),
				);
			}
			for l in shown {
				ui.horizontal(|ui| {
					let text = format!("{}: {}", l.kind, l.members.join(" · "));
					if ui.link(&text).on_hover_text("Search for this link").clicked() {
						actions.push(Action::SetSearch(format!("{}[{}]", l.kind, l.members.join(" "))));
					}
					if l.mine == 0 {
						// Filled in from a link made before: keep it, or turn it down for this post.
						ui.label(RichText::new("filled in").small().italics().color(Color32::from_rgb(150, 150, 220)));
						if ui.small_button("✓").on_hover_text("Keep it").clicked() {
							actions.push(Action::KeepLink(l.id));
						}
						if ui.small_button("✕").on_hover_text("Not on this post").clicked() {
							actions.push(Action::RejectLink(l.id));
						}
					} else if ui.small_button("✕").on_hover_text("Remove link").clicked() {
						actions.push(Action::RemoveLink(l.id));
					}
				});
			}
			for s in &d.offered {
				ui.horizontal(|ui| {
					if ui.small_button("+").on_hover_text(format!("You've made this link on {} other posts", s.uses)).clicked() {
						actions.push(Action::AddOffered(s.kind.clone(), s.members.clone()));
					}
					ui.label(RichText::new(format!("{}: {}", s.kind, s.members.join(" · "))).weak().italics());
				});
			}
			ui.horizontal(|ui| {
				egui::ComboBox::from_id_salt("kind").selected_text(&d.kind).show_ui(ui, |ui| {
					for n in &d.link_names {
						ui.selectable_value(&mut d.kind, n.clone(), n);
					}
				});
				let label = format!("Link {} ticked", d.picked.len());
				if ui.add_enabled(d.picked.len() >= 2, egui::Button::new(label)).clicked() {
					actions.push(Action::AddLink);
				}
			});
			ui.horizontal(|ui| {
				ui.add(egui::TextEdit::singleline(&mut d.new_link_name).hint_text("new link name").desired_width(140.0));
				if ui.add_enabled(!d.new_link_name.trim().is_empty(), egui::Button::new("Add name")).clicked() {
					actions.push(Action::AddLinkName);
				}
			});

			ui.separator();
			ui.heading("Your tags");
			for t in &d.mine {
				ui.horizontal(|ui| {
					let mut on = d.picked.contains(&t.name);
					if ui.checkbox(&mut on, "").changed() {
						toggle(&mut d.picked, &t.name, on);
					}
					tag_button(ui, &t.name, Color32::from_rgb(120, 200, 140), &query, actions);
					if ui.small_button("✕").on_hover_text("Remove your tag").clicked() {
						actions.push(Action::VoteTag(t.name.clone(), 0));
					}
				});
			}
			ui.horizontal(|ui| {
				let r = ui.add(egui::TextEdit::singleline(&mut d.new_tag).hint_text("add a tag of your own").desired_width(160.0));
				let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				if (ui.button("Add").clicked() || enter) && !d.new_tag.trim().is_empty() {
					actions.push(Action::VoteTag(std::mem::take(&mut d.new_tag), 1));
				}
			});

			ui.separator();
			ui.heading("Site tags");
			ui.label(RichText::new("Click to add to the search, right-click for more.").weak());
			for (cat, tags) in &post.categories {
				ui.label(RichText::new(cat).strong().color(category_color(cat)));
				for t in tags {
					ui.horizontal(|ui| {
						let mut on = d.picked.contains(t);
						if ui.checkbox(&mut on, "").changed() {
							toggle(&mut d.picked, t, on);
						}
						tag_button(ui, t, category_color(cat), &query, actions);
					});
				}
			}
		});
	}

	/// One post large, with ◀ ▶ (← →) through the results, fit or full size
	/// (Z), the original file, and ♥ (F). Esc goes back to the grid.
	fn viewer(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
		let Some(i) = self.viewer else { return };
		let Some(post) = self.posts.get(i).cloned() else {
			self.viewer = None;
			return;
		};
		let shown: Vec<usize> = (0..self.posts.len()).filter(|&j| self.visible(j)).collect();
		let at = shown.iter().position(|&j| j == i).map_or(0, |p| p + 1);
		let video = matches!(post.ext.as_str(), "webm" | "mp4" | "swf");
		ui.horizontal(|ui| {
			if ui.button("◀").on_hover_text("Previous (←)").clicked() {
				actions.push(Action::Step(-1));
			}
			if ui.button("▶").on_hover_text("Next (→)").clicked() {
				actions.push(Action::Step(1));
			}
			ui.label(format!("{at} of {}{}", shown.len(), if self.done { "" } else { "+" }));
			ui.separator();
			ui.label(RichText::new(format!("#{} · {} · {}×{} {}", post.id, post.site, post.width, post.height, post.ext)).weak());
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				if ui.button("✕").on_hover_text("Back to the results (Esc)").clicked() {
					actions.push(Action::CloseViewer);
				}
				let size_label = if self.viewer_actual { "Full size" } else { "Fit" };
				if ui.button(size_label).on_hover_text("Fit to the window or full size (Z)").clicked() {
					self.viewer_actual = !self.viewer_actual;
				}
				if post.file_url.is_some() && !video {
					ui.checkbox(&mut self.viewer_original, "Original").on_hover_text("The full file instead of the site's sample");
				}
				let fav = self.favorites.contains(&post.key());
				if ui.button(RichText::new(if fav { "♥" } else { "♡" }).color(Color32::from_rgb(230, 90, 120))).on_hover_text("Favorite (F)").clicked() {
					actions.push(Action::ToggleFavorite(i));
				}
				if ui.button("Open page").clicked() {
					actions.push(Action::Open(post.page_url.clone()));
				}
				if (video || post.ext == "gif")
					&& let Some(f) = &post.file_url
					&& ui.button("Open file").on_hover_text("In your own player or browser").clicked()
				{
					actions.push(Action::Open(f.clone()));
				}
			});
		});
		ui.separator();
		if !self.site_shows(&post.site) {
			ui.centered_and_justified(|ui| {
				ui.label(format!(
					"{} only lets its own website show its images (hotlink protection). “Open page” shows it in your browser.",
					post.site
				))
			});
			return;
		}
		// GIFs and videos play here: the file, downloaded into memory.
		if (video || post.ext == "gif") && media_ok(video)
			&& let Some(file) = post.file_url.clone()
		{
			if self.playing.as_ref().is_none_or(|p| p.url() != file) {
				if let Some(site) = self.site_by_id(&post.site) {
					self.worker.media(site, file.clone(), video);
				}
				self.playing = Some(Playing::Loading(file));
			}
			self.play(ui, actions);
			return;
		}
		let url = if video {
			post.sample_url.clone().or(post.preview_url.clone())
		} else if self.viewer_original {
			post.file_url.clone().or(post.sample_url.clone())
		} else {
			post.sample_url.clone().or(post.file_url.clone())
		}
		.or(post.preview_url.clone());
		let Some(url) = url else {
			ui.centered_and_justified(|ui| ui.label(format!("{} sent no image for this post.", post.site)));
			return;
		};
		match self.big_texture(&post.site, &url) {
			Some(t) => {
				let size = t.size_vec2();
				if self.viewer_actual {
					egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
						ui.add(egui::Image::from_texture(&t).fit_to_exact_size(size));
					});
				} else {
					let room = ui.available_size();
					let scale = (room.x / size.x).min(room.y / size.y).min(3.0);
					ui.centered_and_justified(|ui| {
						ui.add(egui::Image::from_texture(&t).fit_to_exact_size(size * scale));
					});
				}
			}
			None => {
				ui.centered_and_justified(|ui| ui.spinner());
			}
		}
	}

	/// Draws the GIF or video in the large view, with its controls.
	fn play(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
		let actual = self.viewer_actual;
		let show = |ui: &mut egui::Ui, t: &TextureHandle, room: Vec2| {
			let size = t.size_vec2();
			if actual {
				egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
					ui.add(egui::Image::from_texture(t).fit_to_exact_size(size));
				});
			} else {
				let scale = (room.x / size.x).min(room.y / size.y).min(3.0);
				ui.allocate_ui(room, |ui| {
					ui.centered_and_justified(|ui| {
						ui.add(egui::Image::from_texture(t).fit_to_exact_size(size * scale));
					});
				});
			}
		};
		let ctx = ui.ctx().clone();
		match &mut self.playing {
			None => {}
			Some(Playing::Loading(_)) => {
				ui.centered_and_justified(|ui| {
					ui.horizontal(|ui| {
						ui.spinner();
						ui.label("Loading…");
					});
				});
			}
			Some(Playing::Failed(_, e)) => {
				ui.centered_and_justified(|ui| ui.label(format!("Couldn't play this here: {e}. “Open file” plays it elsewhere.")));
			}
			Some(Playing::Anim { frames, total, clock, paused, .. }) => {
				let at = paused.unwrap_or((clock.elapsed().as_millis() as u64 % *total as u64) as u32);
				let mut acc = 0;
				let mut shown = 0;
				for (i, (_, ms)) in frames.iter().enumerate() {
					if at < acc + ms {
						shown = i;
						break;
					}
					acc += ms;
				}
				let room = ui.available_size() - Vec2::new(0.0, 28.0);
				let tex = frames[shown].0.clone();
				show(ui, &tex, room);
				ui.horizontal(|ui| {
					if ui.button(if paused.is_some() { "▶" } else { "⏸" }).on_hover_text("Play or pause (Space)").clicked() {
						actions.push(Action::TogglePlay);
					}
					ui.label(RichText::new(format!("frame {} of {}", shown + 1, frames.len())).weak());
				});
				if paused.is_none() && frames.len() > 1 {
					ctx.request_repaint_after(std::time::Duration::from_millis((acc + frames[shown].1).saturating_sub(at).max(10) as u64));
				}
			}
			Some(Playing::Video { video, tex, seen, .. }) => {
				if video.ended() && !video.paused {
					video.play_from(0.0, &ctx);
					*seen = 0;
				}
				if let Some((n, img)) = video.frame_after(*seen) {
					*seen = n;
					match tex {
						Some(t) => t.set(img, egui::TextureOptions::LINEAR),
						None => *tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR)),
					}
				}
				let room = ui.available_size() - Vec2::new(0.0, 32.0);
				match tex {
					Some(t) => show(ui, t, room),
					None => {
						ui.allocate_ui(room, |ui| ui.centered_and_justified(|ui| ui.spinner()));
					}
				}
				ui.horizontal(|ui| {
					if ui.button(if video.paused { "▶" } else { "⏸" }).on_hover_text("Play or pause (Space)").clicked() {
						actions.push(Action::TogglePlay);
					}
					let mut at = video.position();
					if video.duration > 0.0 {
						ui.spacing_mut().slider_width = (ui.available_width() - 200.0).max(120.0);
						let r = ui.add(egui::Slider::new(&mut at, 0.0..=video.duration).show_value(false));
						if r.drag_stopped() || (r.changed() && !r.dragged()) {
							actions.push(Action::Seek(at));
						}
					}
					ui.label(format!("{} / {}", media::clock(video.position()), media::clock(video.duration)));
					if video.has_audio {
						if ui.button(if video.muted { "🔇" } else { "🔊" }).on_hover_text("Sound on or off (M)").clicked() {
							actions.push(Action::ToggleMute);
						}
					} else {
						ui.label(RichText::new("no sound").weak());
					}
				});
				if !video.paused {
					ctx.request_repaint_after(std::time::Duration::from_millis(15));
				}
			}
		}
	}

	fn save_window(&mut self, ctx: &egui::Context) {
		if !self.save_open {
			return;
		}
		let mut open = true;
		let mut save = false;
		egui::Window::new("Save search").open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
			ui.monospace(&self.query);
			ui.label(RichText::new(format!("on {} · {}", self.current_sites().iter().map(|s| s.name()).collect::<Vec<_>>().join(", "), self.settings.ratings.iter().map(|r| r.label()).collect::<Vec<_>>().join(", "))).weak());
			let r = ui.add(egui::TextEdit::singleline(&mut self.save_name).hint_text("name").desired_width(260.0));
			save = ui.button("Save").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
			ui.label(RichText::new("Kept encrypted in your vault. Next time it marks what's new.").weak());
			if let Some(n) = &self.save_note {
				ui.colored_label(Color32::from_rgb(220, 90, 90), n);
			}
		});
		if save {
			let ids: Vec<String> = self.current_sites().iter().map(|s| s.id().to_string()).collect();
			let saved = self.store.lock().unwrap().save_search(&self.save_name, &self.query, &ids, &self.settings.ratings);
			match saved {
				Ok(id) => {
					self.save_open = false;
					self.running_saved = Some(id);
					// What's showing now counts as seen.
					let mut newest: HashMap<String, u64> = HashMap::new();
					for p in &self.posts {
						let n = newest.entry(p.site.clone()).or_insert(0);
						*n = (*n).max(p.id);
					}
					let _ = self.store.lock().unwrap().mark_seen(id, &newest);
					self.urgent = true;
				}
				Err(e) => self.save_note = Some(e.to_string()),
			}
		}
		self.save_open &= open;
	}

	fn settings_window(&mut self, ctx: &egui::Context) {
		let mut open = self.settings_open;
		let mut changed = false;
		let (mut lock, mut clear_history, mut change_pin, mut check_logins) = (false, false, false, false);
		egui::Window::new("Settings").open(&mut open).resizable(false).show(ctx, |ui| {
			ui.label("Your name or e621 username, sent in the User-Agent as the site asks:");
			changed |= ui.text_edit_singleline(&mut self.settings.contact).changed();
			ui.add_space(8.0);
			ui.label("Optional e621 login (API key from your account settings). e621 leaves images out for logged-out users on some posts; logged in, you see them:");
			ui.horizontal(|ui| {
				ui.label("Username");
				changed |= ui.text_edit_singleline(&mut self.settings.e621_user).changed();
			});
			ui.horizontal(|ui| {
				ui.label("API key ");
				changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.e621_key).password(true)).changed();
			});
			ui.add_space(8.0);
			ui.label("Optional Danbooru login (API key from your profile); Gold and up can search more than 2 tags:");
			ui.horizontal(|ui| {
				ui.label("Username");
				changed |= ui.text_edit_singleline(&mut self.settings.danbooru_user).changed();
			});
			ui.horizontal(|ui| {
				ui.label("API key ");
				changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.danbooru_key).password(true)).changed();
			});
			ui.add_space(8.0);
			ui.label("gelbooru.com login (user id and API key from its account options page); its API needs one:");
			ui.horizontal(|ui| {
				ui.label("User id ");
				changed |= ui.text_edit_singleline(&mut self.settings.gelbooru_user).changed();
			});
			ui.horizontal(|ui| {
				ui.label("API key ");
				changed |= ui.add(egui::TextEdit::singleline(&mut self.settings.gelbooru_key).password(true)).changed();
			});
			ui.add_space(4.0);
			if ui.button("Check logins").on_hover_text("Ask each site whether it accepts the login above").clicked() {
				check_logins = true;
			}
			for (site, answer) in self.login_checks.lock().unwrap().iter() {
				match answer {
					Ok(who) => ui.label(RichText::new(format!("{site}: logged in as {who} ✓")).color(Color32::from_rgb(110, 200, 120))),
					Err(e) => ui.colored_label(Color32::from_rgb(230, 120, 90), format!("{site}: {e}")),
				};
			}
			ui.add_space(8.0);
			ui.separator();
			ui.strong("Links");
			changed |= ui.checkbox(&mut self.settings.auto_links, "Fill in links I've made before on posts with the same tags").changed();
			changed |= ui
				.checkbox(&mut self.settings.show_imageless, "Show posts the site sent without images")
				.on_hover_text("e621 leaves the images out for logged-out users on posts with tags from its default blacklist. Logging in above shows them.")
				.changed();
			ui.add_space(8.0);
			ui.separator();
			ui.strong("Privacy");
			ui.label(
				RichText::new(format!(
					"Settings, search history, links and posts seen are kept encrypted in {}, and only unlocked in memory while the app is open.",
					self.saver.vault().path().display()
				))
				.weak(),
			);
			if let Some(e) = self.saver.error.lock().unwrap().as_ref() {
				ui.colored_label(Color32::from_rgb(220, 90, 90), format!("Last save failed: {e}"));
			}
			ui.horizontal(|ui| {
				if ui.button("Lock now").clicked() {
					lock = true;
				}
				if ui.button("Clear search history").clicked() {
					clear_history = true;
				}
			});
			ui.add_space(4.0);
			ui.label("Change PIN:");
			ui.horizontal(|ui| {
				ui.add(egui::TextEdit::singleline(&mut self.new_pin).password(true).hint_text("new PIN").desired_width(100.0));
				ui.add(egui::TextEdit::singleline(&mut self.new_pin_again).password(true).hint_text("again").desired_width(100.0));
				if ui.button("Change").clicked() {
					change_pin = true;
				}
			});
			if let Some(n) = &self.pin_note {
				ui.label(RichText::new(n).weak());
			}
		});
		if lock {
			self.lock_requested = true;
		}
		if check_logins {
			self.login_checks.lock().unwrap().clear();
			for site in self.sites.clone() {
				let (out, ctx) = (self.login_checks.clone(), ctx.clone());
				std::thread::spawn(move || {
					let answer = match site.check_login() {
						None => Err("no login set".to_string()),
						Some(Ok(who)) => Ok(who),
						Some(Err(e)) => Err(format!("refused: {e}")),
					};
					out.lock().unwrap().push((site.name().to_string(), answer));
					ctx.request_repaint();
				});
			}
		}
		if clear_history {
			let _ = self.store.lock().unwrap().clear_history();
			self.urgent = true;
		}
		if change_pin {
			self.pin_note = Some(if self.new_pin.chars().count() < vault::MIN_PIN {
				format!("Use at least {} characters.", vault::MIN_PIN)
			} else if self.new_pin != self.new_pin_again {
				"The two don't match.".into()
			} else {
				match vault::Key::new(&self.new_pin) {
					Ok(key) => {
						self.saver.change_key(key, &self.store);
						"PIN changed.".into()
					}
					Err(e) => e.to_string(),
				}
			});
			self.new_pin.zeroize();
			self.new_pin_again.zeroize();
		}
		if changed {
			self.save_settings();
			self.sites = make_sites(&self.settings);
		}
		self.settings_open = open;
	}
}

/// Whether this kind can play here: GIFs always, videos when ffmpeg is installed.
fn media_ok(video: bool) -> bool {
	static FFMPEG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	!video || *FFMPEG.get_or_init(media::can_play_video)
}

/// A post the site sent with no image at all: e621 does that for logged-out
/// users on posts with tags from its default blacklist; logging in shows them.
fn imageless(p: &Post) -> bool {
	p.file_url.is_none() && p.preview_url.is_none() && p.sample_url.is_none()
}

fn toggle(set: &mut BTreeSet<String>, name: &str, on: bool) {
	if on {
		set.insert(name.to_string());
	} else {
		set.remove(name);
	}
}

/// A tag in the post panel: click to add it to the search (again to take it
/// out), − to leave it out; ✓ and ⊘ show what the search does with it now.
fn tag_button(ui: &mut egui::Ui, name: &str, color: Color32, query: &str, actions: &mut Vec<Action>) {
	let state = tags_panel::search_tags(query).into_iter().find(|(n, _)| n == name).map(|(_, on)| on);
	let text = match state {
		Some(true) => RichText::new(format!("✓ {name}")).color(color).strong(),
		Some(false) => RichText::new(format!("⊘ {name}")).color(color).strikethrough(),
		None => RichText::new(name).color(color),
	};
	let r = ui.add(egui::Label::new(text).sense(Sense::click())).on_hover_text(match state {
		Some(true) => "In the search · click to take it out",
		_ => "Click to add to the search",
	});
	if r.clicked() {
		actions.push(Action::Include(name.to_string()));
	}
	let minus = ui.small_button(if state == Some(false) { "+" } else { "−" }).on_hover_text(if state == Some(false) {
		"Stop leaving it out"
	} else {
		"Leave it out of the search"
	});
	if minus.clicked() {
		actions.push(Action::Exclude(name.to_string()));
	}
	r.context_menu(|ui| {
		if ui.button("Search only this").clicked() {
			actions.push(Action::SetSearch(name.to_string()));
		}
		if ui.button("Posts where it's linked").clicked() {
			actions.push(Action::SetSearch(format!("[{name}]")));
		}
		if ui.button("Add to a group…").clicked() {
			actions.push(Action::OpenGroups(Some(name.to_string())));
		}
		if ui.button("Translate for other sites…").clicked() {
			actions.push(Action::Translate(name.to_string()));
		}
	});
}

fn category_color(cat: &str) -> Color32 {
	match cat {
		"artist" => Color32::from_rgb(242, 172, 8),
		"copyright" => Color32::from_rgb(221, 0, 221),
		"character" => Color32::from_rgb(0, 170, 0),
		"species" => Color32::from_rgb(237, 93, 31),
		"meta" => Color32::from_rgb(170, 170, 170),
		"lore" => Color32::from_rgb(34, 136, 34),
		"invalid" => Color32::from_rgb(255, 61, 61),
		_ => Color32::from_rgb(120, 170, 255),
	}
}

fn short_count(n: u64) -> String {
	match n {
		0..1_000 => n.to_string(),
		1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
		_ => format!("{:.1}M", n as f64 / 1e6),
	}
}

impl App {
	fn ui(&mut self, ui: &mut egui::Ui) {
		let ctx = ui.ctx().clone();
		self.receive(&ctx);
		let mut actions = Vec::new();
		egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &mut actions));
		self.hover_tag = None;
		self.keys(&ctx, &mut actions);
		if self.viewer.is_some() {
			egui::CentralPanel::default().show(ui, |ui| self.viewer(ui, &mut actions));
		} else {
		if self.settings.show_tags {
			egui::Panel::left("tags").resizable(true).default_size(250.0).min_size(180.0).max_size(420.0).show(ui, |ui| {
				self.hover_tag = self.tags.show(ui, &self.query, &self.posts, self.asked.as_ref(), self.generation, &self.store, &mut actions);
			});
		}
		if self.selected.is_some() {
			egui::Panel::right("detail").resizable(true).default_size(380.0).min_size(260.0).show(ui, |ui| self.detail(ui, &mut actions));
		}
		egui::CentralPanel::default().show(ui, |ui| self.grid(ui, &mut actions));
		}
		self.settings_window(&ctx);
		self.urgent |= self.translations.show(&ctx, &self.store, &self.sites);
		self.save_window(&ctx);
		if self.groups.show(&ctx, &self.store) {
			self.urgent = true;
			// A new relation fills in on what's loaded now, not only on the next search.
			let _ = self.store.lock().unwrap().auto_link(&self.posts);
			if let Some(d) = self.detail.take() {
				self.load_detail(Some(d));
			}
		}
		self.apply(actions);
		if self.saver.tick(&self.store, self.urgent) {
			self.urgent = false;
		} else {
			ctx.request_repaint_after(std::time::Duration::from_secs(1));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::typing_word;
	use eframe::egui;

	#[test]
	fn finds_the_word_being_typed() {
		assert_eq!(typing_word("fox wo"), Some((4, "wo")));
		assert_eq!(typing_word("fox -(wo"), Some((6, "wo")));
		assert_eq!(typing_word("relation[rab"), Some((9, "rab")));
		assert_eq!(typing_word("fox "), None);
		assert_eq!(typing_word("score:>1"), None);
		assert_eq!(typing_word("f"), None);
	}

	/// Every text drawn in one frame of the window at this width, with where it ended up.
	fn texts(app: &mut super::App, ctx: &egui::Context, width: f32) -> Vec<(String, egui::Rect)> {
		texts_with(app, ctx, width, Vec::new())
	}

	fn texts_with(app: &mut super::App, ctx: &egui::Context, width: f32, events: Vec<egui::Event>) -> Vec<(String, egui::Rect)> {
		let input = egui::RawInput {
			events,
			screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 800.0))),
			focused: true,
			..Default::default()
		};
		let mut out = ctx.run_ui(input, |ui| app.ui(ui));
		out.textures_delta.clear();
		let mut found = Vec::new();
		for clipped in &out.shapes {
			if let egui::epaint::Shape::Text(t) = &clipped.shape {
				found.push((t.galley.job.text.clone(), t.visual_bounding_rect()));
			}
		}
		found
	}

	/// The top bar used to push the ratings and Settings off the right edge when
	/// several sites were picked; they must stay in the window at any width.
	#[test]
	fn top_bar_fits_narrow_windows() {
		let dir = std::env::temp_dir().join(format!("bs-topbar-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let store = bettersearch_core::Store::in_memory().unwrap();
		let saver = super::Saver::new(super::vault::Vault::in_dir(&dir), std::sync::Arc::new(super::vault::Key::for_tests()), store.changes());
		let ctx = egui::Context::default();
		let mut app = super::App::new(ctx.clone(), store, saver);
		app.settings.sites = app.sites.iter().map(|s| s.id().to_string()).collect();
		app.query = "dog eat link".into();
		for width in [900.0, 1300.0] {
			let mut seen = Vec::new();
			for _ in 0..3 {
				seen = texts(&mut app, &ctx, width);
			}
			for want in ["⚙", "Safe", "☆ Save", "Search", "5 sites"] {
				let Some((_, rect)) = seen.iter().find(|(t, _)| t == want) else {
					panic!("“{want}” not drawn at width {width}: {:?}", seen.iter().map(|(t, _)| t).collect::<Vec<_>>())
				};
				assert!(rect.max.x <= width, "“{want}” ends at {} in a {width} wide window", rect.max.x);
			}
		}
		std::fs::remove_dir_all(&dir).unwrap();
	}


	/// `rabbit eat relation`: the panel lists the index's objects with keep and
	/// reject; rejecting crosses one out, and it's left out of the next search.
	#[test]
	fn relation_panel_keeps_and_rejects() {
		use bettersearch_core::store::RelationState;
		let dir = std::env::temp_dir().join(format!("bs-asked-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let store = bettersearch_core::Store::in_memory().unwrap();
		let learned = [
			bettersearch_core::learn::Learned { object: "carrot".into(), score: 9.0 },
			bettersearch_core::learn::Learned { object: "police_uniform".into(), score: 2.0 },
		];
		store.remember_learned("rabbit", "eat", "e926", &learned).unwrap();
		let entries = store.relations("rabbit", "eat", false).unwrap();
		let saver = super::Saver::new(super::vault::Vault::in_dir(&dir), std::sync::Arc::new(super::vault::Key::for_tests()), store.changes());
		let ctx = egui::Context::default();
		let mut app = super::App::new(ctx.clone(), store, saver);
		app.query = "rabbit eat relation".into();
		app.asked = Some(bettersearch_core::search::AskedHere {
			subject: "rabbit".into(),
			verb: "eat".into(),
			verb_tag: "eating".into(),
			pairs: vec![("rabbit".into(), entries)],
		});
		let mut seen = Vec::new();
		for _ in 0..3 {
			seen = texts(&mut app, &ctx, 1300.0);
		}
		let has = |seen: &[(String, egui::Rect)], t: &str| seen.iter().any(|(s, _)| s == t);
		assert!(has(&seen, "RABBIT EAT → RELATION"), "{:?}", seen.iter().map(|(t, _)| t).collect::<Vec<_>>());
		assert!(has(&seen, "carrot") && has(&seen, "police_uniform") && has(&seen, "✕"));

		let set = |object: &str, state| super::Action::SetRelation { subject: "rabbit".into(), verb: "eat".into(), object: object.into(), state };
		app.apply(vec![set("police_uniform", RelationState::Rejected), set("carrot", RelationState::Kept)]);
		assert_eq!(app.error, None);
		for _ in 0..2 {
			seen = texts(&mut app, &ctx, 1300.0);
		}
		assert!(has(&seen, "✓ kept") && has(&seen, "undo"));
		let left: Vec<String> = app.store.lock().unwrap().relations("rabbit", "eat", false).unwrap().into_iter().map(|e| e.object).collect();
		assert_eq!(left, vec!["carrot"]);
		std::fs::remove_dir_all(&dir).unwrap();
	}


	/// e621 sends some posts without images to logged-out users. They're left
	/// out of the grid, counted under it with why, and shown again on request.
	#[test]
	fn imageless_posts_are_skipped_and_counted() {
		let dir = std::env::temp_dir().join(format!("bs-imageless-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let store = bettersearch_core::Store::in_memory().unwrap();
		let saver = super::Saver::new(super::vault::Vault::in_dir(&dir), std::sync::Arc::new(super::vault::Key::for_tests()), store.changes());
		let ctx = egui::Context::default();
		let mut app = super::App::new(ctx.clone(), store, saver);
		let post = |id: u64, url: Option<&str>| bettersearch_core::Post {
			site: "e621".into(),
			id,
			tags: vec!["fox".into()],
			categories: Default::default(),
			rating: bettersearch_core::Rating::Explicit,
			score: 0,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: "png".into(),
			md5: Some("ab".into()),
			file_url: url.map(String::from),
			sample_url: None,
			preview_url: url.map(String::from),
			page_url: String::new(),
			sources: Vec::new(),
			created_at: String::new(),
		};
		app.posts = vec![post(1, Some("https://example.invalid/1.jpg")), post(2, None)];
		app.searched = true;
		let has = |seen: &[(String, egui::Rect)], t: &str| seen.iter().any(|(s, _)| s.contains(t));
		let mut seen = Vec::new();
		for _ in 0..2 {
			seen = texts(&mut app, &ctx, 1300.0);
		}
		assert!(has(&seen, "1 shown") && has(&seen, "1 hidden by e621 when logged out"), "{:?}", seen.iter().map(|(t, _)| t).collect::<Vec<_>>());
		assert!(!has(&seen, "hides this"));
		app.settings.show_imageless = true;
		for _ in 0..2 {
			seen = texts(&mut app, &ctx, 1300.0);
		}
		assert!(has(&seen, "2 shown") && has(&seen, "e621 hides this") && !has(&seen, "hidden by"));
		std::fs::remove_dir_all(&dir).unwrap();
	}


	fn key(k: egui::Key) -> egui::Event {
		egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
	}

	fn app_with_posts(name: &str, n: u64) -> (super::App, egui::Context, std::path::PathBuf) {
		let dir = std::env::temp_dir().join(format!("bs-{name}-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let store = bettersearch_core::Store::in_memory().unwrap();
		let saver = super::Saver::new(super::vault::Vault::in_dir(&dir), std::sync::Arc::new(super::vault::Key::for_tests()), store.changes());
		let ctx = egui::Context::default();
		let mut app = super::App::new(ctx.clone(), store, saver);
		app.posts = (1..=n)
			.map(|id| bettersearch_core::Post {
				site: "e926".into(),
				id,
				tags: vec!["fox".into(), "smile".into()],
				categories: Default::default(),
				rating: bettersearch_core::Rating::Safe,
				score: 0,
				fav_count: 0,
				width: 1,
				height: 1,
				ext: "png".into(),
				md5: None,
				file_url: Some(format!("https://example.invalid/{id}.png")),
				sample_url: None,
				preview_url: Some(format!("https://example.invalid/p{id}.png")),
				page_url: String::new(),
				sources: Vec::new(),
				created_at: String::new(),
			})
			.collect();
		app.searched = true;
		app.done = true;
		(app, ctx, dir)
	}

	/// Clicking a tag in the post panel again takes it back out, and − leaves it out.
	#[test]
	fn post_panel_tags_toggle() {
		let (mut app, _ctx, dir) = app_with_posts("toggle", 1);
		app.query = "wolf".into();
		app.apply(vec![super::Action::Include("fox".into())]);
		assert_eq!(app.query, "wolf fox");
		app.apply(vec![super::Action::Include("fox".into())]);
		assert_eq!(app.query, "wolf");
		app.apply(vec![super::Action::Exclude("fox".into())]);
		assert_eq!(app.query, "wolf -fox");
		app.apply(vec![super::Action::Include("fox".into())]);
		assert_eq!(app.query, "wolf fox");
		app.apply(vec![super::Action::Exclude("fox".into()), super::Action::Exclude("fox".into())]);
		assert_eq!(app.query, "wolf");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// ♥ keeps a post in the vault's favorites, and again takes it out.
	#[test]
	fn favorites_toggle_and_persist() {
		let (mut app, _ctx, dir) = app_with_posts("fav", 2);
		let key = app.posts[1].key();
		app.apply(vec![super::Action::ToggleFavorite(1)]);
		assert!(app.favorites.contains(&key) && app.store.lock().unwrap().is_favorite(&key).unwrap());
		assert_eq!(app.store.lock().unwrap().favorite_posts("e926").unwrap().len(), 1);
		app.apply(vec![super::Action::ToggleFavorite(1)]);
		assert!(!app.favorites.contains(&key) && !app.store.lock().unwrap().is_favorite(&key).unwrap());
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// Keys move through the grid and the large view; Esc goes back.
	#[test]
	fn keys_and_viewer() {
		let (mut app, ctx, dir) = app_with_posts("keys", 5);
		let frame = |app: &mut super::App, events| {
			texts_with(app, &ctx, 1300.0, events);
		};
		frame(&mut app, vec![]);
		// → selects the first post, → again the next; ↓ jumps a row.
		frame(&mut app, vec![key(egui::Key::ArrowRight)]);
		assert_eq!(app.selected, Some(0));
		frame(&mut app, vec![key(egui::Key::ArrowRight)]);
		assert_eq!(app.selected, Some(1));
		assert!(app.grid_cols >= 3, "{}", app.grid_cols);
		frame(&mut app, vec![key(egui::Key::ArrowLeft)]);
		assert_eq!(app.selected, Some(0));
		// Enter shows it large; → moves the large view; F favorites it; Esc returns.
		frame(&mut app, vec![key(egui::Key::Enter)]);
		assert_eq!(app.viewer, Some(0));
		let seen = texts(&mut app, &ctx, 1300.0);
		assert!(seen.iter().any(|(t, _)| t == "1 of 5"), "{:?}", seen.iter().map(|(t, _)| t).collect::<Vec<_>>());
		frame(&mut app, vec![key(egui::Key::ArrowRight)]);
		assert_eq!((app.viewer, app.selected), (Some(1), Some(1)));
		frame(&mut app, vec![key(egui::Key::F)]);
		assert!(app.favorites.contains(&app.posts[1].key()));
		frame(&mut app, vec![key(egui::Key::Escape)]);
		assert_eq!((app.viewer, app.selected), (None, Some(1)));
		// At the last post, → stays put (nothing more to load).
		app.apply(vec![super::Action::View(4)]);
		frame(&mut app, vec![key(egui::Key::ArrowRight)]);
		assert_eq!(app.viewer, Some(4));
		std::fs::remove_dir_all(&dir).unwrap();
	}


	/// A GIF post plays in the large view: all frames, Space pauses.
	#[test]
	fn gif_plays_in_the_viewer() {
		let (mut app, ctx, dir) = app_with_posts("gif", 1);
		app.posts[0].ext = "gif".into();
		let url = app.posts[0].file_url.clone().unwrap();
		app.apply(vec![super::Action::View(0)]);
		texts(&mut app, &ctx, 1300.0);
		assert!(matches!(app.playing, Some(super::Playing::Loading(ref u)) if *u == url));
		let frame = |shade: u8| (egui::ColorImage::new([4, 4], vec![egui::Color32::from_gray(shade); 16]), 100);
		app.worker.inject(super::worker::Msg::Animation { url: url.clone(), frames: Ok(vec![frame(0), frame(255)]) });
		let seen = texts(&mut app, &ctx, 1300.0);
		assert!(seen.iter().any(|(t, _)| t.starts_with("frame ") && t.ends_with(" of 2")), "{:?}", seen.iter().map(|(t, _)| t).collect::<Vec<_>>());
		texts_with(&mut app, &ctx, 1300.0, vec![key(egui::Key::Space)]);
		assert!(matches!(app.playing, Some(super::Playing::Anim { paused: Some(_), .. })));
		// Leaving the large view stops it.
		texts_with(&mut app, &ctx, 1300.0, vec![key(egui::Key::Escape)]);
		assert!(app.playing.is_none());
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// A video plays in the large view, from memory, with its controls.
	/// Skipped where ffmpeg isn't installed.
	#[test]
	fn video_plays_in_the_viewer() {
		if !super::media::can_play_video() {
			return;
		}
		let made = std::process::Command::new("ffmpeg")
			.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=2"])
			.args(["-f", "webm", "-c:v", "libvpx", "pipe:1"])
			.stdin(std::process::Stdio::null())
			.output()
			.unwrap();
		let (mut app, ctx, dir) = app_with_posts("video", 1);
		app.posts[0].ext = "webm".into();
		let url = app.posts[0].file_url.clone().unwrap();
		app.apply(vec![super::Action::View(0)]);
		texts(&mut app, &ctx, 1300.0);
		let video = super::media::Video::open(&made.stdout).map(Box::new);
		app.worker.inject(super::worker::Msg::Video { url, video });
		let mut seen = Vec::new();
		for _ in 0..40 {
			seen = texts(&mut app, &ctx, 1300.0);
			if matches!(app.playing, Some(super::Playing::Video { seen: n, .. }) if n > 2) {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(50));
		}
		assert!(matches!(app.playing, Some(super::Playing::Video { seen: n, .. }) if n > 2), "no frames shown");
		assert!(seen.iter().any(|(t, _)| t == "⏸") && seen.iter().any(|(t, _)| t == "no sound"));
		texts_with(&mut app, &ctx, 1300.0, vec![key(egui::Key::Space)]);
		assert!(matches!(app.playing, Some(super::Playing::Video { ref video, .. }) if video.paused));
		app.apply(vec![super::Action::CloseViewer]);
		assert!(app.playing.is_none());
		std::fs::remove_dir_all(&dir).unwrap();
	}

}
