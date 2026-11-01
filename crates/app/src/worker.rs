// Background threads, so the window never waits on the network: one runs
// searches, one answers autocomplete (latest request wins), and a few fetch
// and decode thumbnails. Results come back as `Msg`s.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use bettersearch_core::search::AskedHere;

use crate::media::{self, Video};
use bettersearch_core::{MultiSearch, Post, Rating, Site, Store, TagSuggestion};
use eframe::egui::{self, ColorImage};

const THUMB_THREADS: usize = 3;
/// Rounds of site requests (up to 8 each) one "load more" may take to fill a page.
const ROUNDS: usize = 5;
/// Largest side of a decoded image; bigger ones are scaled down.
const MAX_SIDE: u32 = 2560;

pub enum Job {
	Start { sites: Vec<Arc<dyn Site>>, text: String, ratings: Vec<Rating>, generation: u64, want: usize },
	More { generation: u64, want: usize },
	/// Posts like this one: by shared telling tags, or (`versions`) other
	/// versions of the picture from the site's picture search.
	Similar { site: Arc<dyn Site>, post: Box<Post>, ratings: Vec<Rating>, generation: u64, versions: bool },
}

pub enum Msg {
	Planned { generation: u64, queries: Vec<String>, notes: Vec<String>, asked: Option<AskedHere> },
	/// `more`: the search is still going and more posts for this page are coming.
	Posts { generation: u64, posts: Vec<Post>, done: bool, checked: usize, more: bool },
	Failed { generation: u64, error: String },
	Suggestions { word: String, list: Vec<TagSuggestion> },
	Image { url: String, image: Result<ColorImage, String> },
	Animation { url: String, frames: Result<media::Frames, String> },
	Video { url: String, video: Result<Box<Video>, String> },
	/// A chat turn came back, or why it didn't.
	Chat(Result<String, String>),
	/// The endpoint's model list, or why it didn't come.
	Models(Result<Vec<String>, String>),
	/// A taste brief came back, or why it didn't.
	Brief(Result<String, String>),
	/// A file was written to disk, or why it wasn't.
	Saved { site: String, id: u64, path: String, error: Option<String> },
}

pub struct Worker {
	jobs: Sender<Job>,
	suggest: Sender<(Arc<dyn Site>, String)>,
	images: Sender<(Arc<dyn Site>, String)>,
	pub msgs: Receiver<Msg>,
	/// For loading GIFs and videos, one thread each.
	tx: Sender<Msg>,
	ctx: egui::Context,
}

impl Worker {
	pub fn spawn(ctx: egui::Context, store: Arc<Mutex<Store>>) -> Worker {
		let (msg_tx, msgs) = channel();
		let send = {
			let ctx = ctx.clone();
			move |tx: &Sender<Msg>, m: Msg| {
				let _ = tx.send(m);
				ctx.request_repaint();
			}
		};

		let (jobs, job_rx) = channel::<Job>();
		{
			let (tx, send) = (msg_tx.clone(), send.clone());
			std::thread::spawn(move || search_thread(job_rx, store, |m| send(&tx, m)));
		}

		let (suggest, suggest_rx) = channel::<(Arc<dyn Site>, String)>();
		{
			let (tx, send) = (msg_tx.clone(), send.clone());
			std::thread::spawn(move || {
				while let Ok(mut req) = suggest_rx.recv() {
					// Skip to the newest request; older words are stale.
					while let Ok(newer) = suggest_rx.try_recv() {
						req = newer;
					}
					let (site, word) = req;
					if let Ok(list) = site.autocomplete(&word) {
						send(&tx, Msg::Suggestions { word, list });
					}
				}
			});
		}

		let (images, image_rx) = channel::<(Arc<dyn Site>, String)>();
		let image_rx = Arc::new(Mutex::new(image_rx));
		for _ in 0..THUMB_THREADS {
			let (rx, tx, send) = (image_rx.clone(), msg_tx.clone(), send.clone());
			std::thread::spawn(move || {
				loop {
					let next = rx.lock().unwrap().recv();
					let Ok((site, url)) = next else { break };
					let image = site.fetch_bytes(&url).map_err(|e| e.to_string()).and_then(|bytes| decode(&bytes));
					send(&tx, Msg::Image { url, image });
				}
			});
		}

		Worker { jobs, suggest, images, msgs, tx: msg_tx, ctx }
	}

	pub fn send(&self, job: Job) {
		let _ = self.jobs.send(job);
	}

	pub fn suggest(&self, site: Arc<dyn Site>, word: String) {
		let _ = self.suggest.send((site, word));
	}

	pub fn image(&self, site: Arc<dyn Site>, url: String) {
		let _ = self.images.send((site, url));
	}

	/// Hands the window a message as if a thread had sent it.
	#[cfg(test)]
	pub fn inject(&self, m: Msg) {
		let _ = self.tx.send(m);
	}

	/// Downloads a GIF (all frames) or a video (into memory, ready to play).
	/// Copies browser cookies for these hosts, off the UI thread.
	pub fn harvest(&self, hosts: Vec<String>, store: Arc<Mutex<Store>>, ctx: egui::Context) {
		std::thread::spawn(move || {
			for (host, header, _) in crate::cookies::harvest(&hosts) {
				bettersearch_core::sites::set_cookie(&host, &header);
				if let Ok(s) = store.lock() {
					let _ = s.set_cookie(&host, &header);
				}
			}
			ctx.request_repaint();
		});
	}

	/// Asks the endpoint which models it has.
	pub fn models(&self, endpoint: String, key: String) {
		let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let got = bettersearch_core::taste::models(&endpoint, &key).map_err(|e| e.to_string());
			let _ = tx.send(Msg::Models(got));
			ctx.request_repaint();
		});
	}

	/// One chat turn with the guide, off the UI thread.
	pub fn chat(&self, endpoint: String, key: String, model: String, system: String, text: String) {
		let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let got = bettersearch_core::taste::chat(&endpoint, &key, &model, &system, &text).map_err(|e| e.to_string());
			let _ = tx.send(Msg::Chat(got));
			ctx.request_repaint();
		});
	}

	/// Asks the model for a brief. Off the UI thread, so typing carries on.
	pub fn brief(&self, endpoint: String, key: String, model: String, tags: Vec<String>, ratings: Vec<Rating>, dial: f32) {
		let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let got = bettersearch_core::taste::brief(&endpoint, &key, &model, &tags, &ratings, dial).map_err(|e| e.to_string());
			let _ = tx.send(Msg::Brief(got));
			ctx.request_repaint();
		});
	}

	/// Fetches the post's real file (never the thumbnail) and writes it to `dir`.
	pub fn save(&self, site: Arc<dyn Site>, post: Post, dest: std::path::PathBuf) {
		let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let (site_id, id) = (post.site.clone(), post.id);
			let url = post.file_url.clone().or(post.sample_url.clone()).or(post.preview_url.clone());
			let mut path = String::new();
			let mut error = None;
			match url {
				None => error = Some("the site sent no file for this post".to_string()),
				Some(url) => {
					match site.fetch_bytes(&url) {
						Err(e) => error = Some(e.to_string()),
						Ok(bytes) => match std::fs::write(&dest, &bytes) {
							Ok(()) => path = dest.display().to_string(),
							Err(e) => error = Some(e.to_string()),
						},
					}
				}
			}
			let _ = tx.send(Msg::Saved { site: site_id, id, path, error });
			ctx.request_repaint();
		});
	}

	pub fn media(&self, site: Arc<dyn Site>, url: String, video: bool) {
		let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let bytes = site.fetch_bytes(&url).map_err(|e| e.to_string());
			let msg = if video {
				Msg::Video { url, video: bytes.and_then(|b| Video::open(&b)).map(Box::new) }
			} else {
				Msg::Animation { url, frames: bytes.and_then(|b| media::decode_animation(&b)) }
			};
			let _ = tx.send(msg);
			ctx.request_repaint();
		});
	}
}

fn search_thread(jobs: Receiver<Job>, store: Arc<Mutex<Store>>, send: impl Fn(Msg)) {
	let mut current: Option<(u64, MultiSearch)> = None;
	let mut pending: Option<Job> = None;
	loop {
		let job = match pending.take() {
			Some(j) => j,
			None => match jobs.recv() {
				Ok(j) => j,
				Err(_) => return,
			},
		};
		// A newer search makes queued work for older ones pointless.
		let job = {
			let mut job = job;
			while let Ok(next) = jobs.try_recv() {
				match (&job, &next) {
					(_, Job::Start { .. } | Job::Similar { .. }) => job = next,
					(Job::Start { .. }, Job::More { .. }) => {
						pending = Some(next);
						break;
					}
					(Job::More { .. }, Job::More { .. }) => job = next,
					// Similar results come all at once; there's no "more" to them.
					(Job::Similar { .. }, Job::More { .. }) => {}
				}
			}
			job
		};
		let (generation, want) = match job {
			Job::Start { sites, text, ratings, generation, want } => match MultiSearch::start(&sites, &text, &ratings, &store) {
				Ok(s) => {
					send(Msg::Planned { generation, queries: s.queries(), notes: s.notes(), asked: s.asked().into_iter().next().map(|(_, a)| a) });
					current = Some((generation, s));
					(generation, want)
				}
				Err(e) => {
					current = None;
					send(Msg::Failed { generation, error: e.to_string() });
					continue;
				}
			},
			Job::More { generation, want } => (generation, want),
			Job::Similar { site, post, ratings, generation, versions } => {
				current = None;
				similar(site.as_ref(), &post, &ratings, versions, &store, generation, &send);
				continue;
			}
		};
		let Some((g, search)) = current.as_mut() else {
			continue;
		};
		if *g != generation {
			continue;
		}
		// When most posts get dropped here (many left-out tags), one round finds
		// few; keep going, a few rounds, sending what's found as it comes.
		let mut got = 0;
		for round in 1..=ROUNDS {
			let problems = search.problems.len();
			match search.next(want - got, &store) {
				Ok(posts) => {
					got += posts.len();
					// A site that failed part way is named in the notes.
					if search.problems.len() != problems {
						send(Msg::Planned { generation, queries: search.queries(), notes: search.notes(), asked: search.asked().into_iter().next().map(|(_, a)| a) });
					}
					// A newer search waiting stops this one.
					if pending.is_none()
						&& let Ok(next) = jobs.try_recv()
					{
						pending = Some(next);
					}
					let more = !search.done() && got < want && round < ROUNDS && pending.is_none();
					send(Msg::Posts { generation, posts, done: search.done(), checked: search.checked(), more });
					if !more {
						break;
					}
				}
				Err(e) => {
					send(Msg::Failed { generation, error: e.to_string() });
					break;
				}
			}
		}
	}
}

/// Runs a "Find similar" or "Other versions" and sends it like a finished search.
fn similar(site: &dyn Site, post: &Post, ratings: &[Rating], versions: bool, store: &Mutex<Store>, generation: u64, send: &impl Fn(Msg)) {
	let found = if versions {
		site.other_versions(post.id).map(|found| match found {
			Some(posts) => {
				let posts: Vec<Post> = posts.into_iter().filter(|p| ratings.contains(&p.rating)).collect();
				let note = if posts.is_empty() {
					format!("{}'s picture search found no other versions of #{}.", site.name(), post.id)
				} else {
					format!("Other versions of #{} (edits, crops, reposts), from {}'s picture search.", post.id, site.name())
				};
				Ok((format!("picture search: #{}", post.id), note, posts))
			}
			None => Err(format!("{} has no picture search this app can use.", site.name())),
		})
	} else {
		bettersearch_core::similar::find(site, post, ratings, 60).map(|(query, tags, posts)| {
			Ok((query, format!("Posts sharing the most telling tags with #{}, searched by: {}.", post.id, tags.join(", ")), posts))
		})
	};
	match found {
		Ok(Ok((query, note, posts))) => {
			let _ = store.lock().unwrap().save_posts(&posts);
			send(Msg::Planned { generation, queries: vec![query], notes: vec![note], asked: None });
			let checked = posts.len();
			send(Msg::Posts { generation, posts, done: true, checked, more: false });
		}
		Ok(Err(e)) => send(Msg::Failed { generation, error: e }),
		Err(e) => send(Msg::Failed { generation, error: e.to_string() }),
	}
}

fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
	let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
	let img = if img.width() > MAX_SIDE || img.height() > MAX_SIDE { img.thumbnail(MAX_SIDE, MAX_SIDE) } else { img };
	let rgba = img.to_rgba8();
	Ok(ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}
