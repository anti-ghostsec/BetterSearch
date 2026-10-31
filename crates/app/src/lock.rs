// The PIN screen in front of everything, and the saver that writes the vault.
//
// Nothing is readable until the PIN is entered: settings, history, links and
// posts seen all live in the vault. Once unlocked, the database sits in memory
// and the saver seals a copy back to disk now and then (soon after your own
// edits, every half minute while posts load, and on lock or exit).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bettersearch_core::Store;
use eframe::egui::{self, Color32, RichText};
use zeroize::{Zeroize, Zeroizing};

use crate::App;
use crate::settings::{Settings, data_dir};
use crate::vault::{Key, MIN_PIN, Vault, VaultError};

/// How long after the last save your own edits wait, and posts loading.
const SOON: Duration = Duration::from_secs(1);
const LATER: Duration = Duration::from_secs(30);

/// A copy of the database to seal, with the key to seal it with.
type SaveJob = (Arc<Key>, Zeroizing<Vec<u8>>);

pub struct Saver {
	vault: Vault,
	key: Arc<Key>,
	tx: Option<Sender<SaveJob>>,
	thread: Option<JoinHandle<()>>,
	/// `Store::changes` at the last save.
	saved: u64,
	last: Instant,
	/// The last write that failed, to show in the window.
	pub error: Arc<Mutex<Option<String>>>,
}

impl Saver {
	pub fn new(vault: Vault, key: Arc<Key>, saved: u64) -> Saver {
		let (tx, rx) = channel::<SaveJob>();
		let error = Arc::new(Mutex::new(None));
		let thread = {
			let (vault, error) = (vault.clone(), error.clone());
			std::thread::spawn(move || {
				while let Ok(mut job) = rx.recv() {
					// Only the newest copy matters.
					while let Ok(newer) = rx.try_recv() {
						job = newer;
					}
					let (key, bytes) = job;
					*error.lock().unwrap() = vault.write(&key, &bytes).err().map(|e| e.to_string());
				}
			})
		};
		Saver { vault, key, tx: Some(tx), thread: Some(thread), saved, last: Instant::now(), error }
	}

	pub fn vault(&self) -> &Vault {
		&self.vault
	}

	/// Saves if something changed and it's been long enough. Returns true when
	/// the vault is up to date (or a save is on its way).
	pub fn tick(&mut self, store: &Mutex<Store>, urgent: bool) -> bool {
		if store.lock().unwrap().changes() == self.saved {
			return true;
		}
		if self.last.elapsed() < if urgent { SOON } else { LATER } {
			return false;
		}
		self.save(store);
		true
	}

	fn save(&mut self, store: &Mutex<Store>) {
		let bytes = {
			let mut s = store.lock().unwrap();
			let _ = s.prune_posts();
			let bytes = s.to_bytes();
			self.saved = s.changes();
			bytes
		};
		self.last = Instant::now();
		match bytes {
			Ok(b) => {
				if let Some(tx) = &self.tx {
					let _ = tx.send((self.key.clone(), b));
				}
			}
			Err(e) => *self.error.lock().unwrap() = Some(e.to_string()),
		}
	}

	/// Saves whatever is left and waits for the write to finish.
	pub fn flush(&mut self, store: &Mutex<Store>) {
		if store.lock().unwrap().changes() != self.saved {
			self.save(store);
		}
		self.tx = None;
		if let Some(t) = self.thread.take() {
			let _ = t.join();
		}
	}

	/// Seals everything with a new PIN's key from now on, starting right away.
	pub fn change_key(&mut self, key: Key, store: &Mutex<Store>) {
		self.key = Arc::new(key);
		self.save(store);
	}
}

#[derive(Default)]
struct LockScreen {
	pin: String,
	confirm: String,
	error: Option<String>,
	fails: u32,
	/// Time (egui seconds) before which another try isn't taken.
	retry_at: f64,
	erase: String,
	/// The PIN field took focus when the screen opened. Only once: asking
	/// again every frame stopped all keyboard and mouse input reaching the
	/// window (each request resets the window's text input).
	focused_once: bool,
}

type Opened = Result<(Arc<Key>, Store), (String, bool)>;

enum State {
	Locked(LockScreen),
	/// Making a key off the window's thread (it takes a second). `renew`: the
	/// PIN was shorter than `MIN_PIN`, so a new one is asked for once it opens.
	Busy {
		rx: Receiver<Opened>,
		label: &'static str,
		fails: u32,
		renew: bool,
	},
	/// Opened with a PIN from when 4 characters were enough: a new one is
	/// chosen before anything else.
	Renew(LockScreen, Box<Store>),
	Open(Box<App>),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
	/// No vault yet: choose a PIN.
	First,
	Unlock,
	/// Opened with a short PIN: choose a new one.
	Renew,
}

pub struct Root {
	ctx: egui::Context,
	vault: Vault,
	state: State,
}

impl Root {
	pub fn new(ctx: egui::Context) -> Root {
		Root { ctx, vault: Vault::in_dir(&data_dir()), state: State::Locked(LockScreen::default()) }
	}

	/// Runs `job` on its own thread, showing `label` meanwhile.
	fn run(&mut self, label: &'static str, fails: u32, renew: bool, job: impl FnOnce(&Vault) -> Opened + Send + 'static) {
		let (tx, rx) = channel();
		let (vault, ctx) = (self.vault.clone(), self.ctx.clone());
		std::thread::spawn(move || {
			let _ = tx.send(job(&vault));
			ctx.request_repaint();
		});
		self.state = State::Busy { rx, label, fails, renew };
	}

	/// What the PIN screen is for, when it's showing.
	fn mode(&self) -> Option<Mode> {
		match &self.state {
			State::Locked(_) if self.vault.exists() => Some(Mode::Unlock),
			State::Locked(_) => Some(Mode::First),
			State::Renew(..) => Some(Mode::Renew),
			_ => None,
		}
	}

	fn lock_screen(&mut self, ui: &mut egui::Ui) {
		let Some(mode) = self.mode() else {
			return;
		};
		let choosing = mode != Mode::Unlock;
		let now = ui.input(|i| i.time);
		let (State::Locked(s) | State::Renew(s, _)) = &mut self.state else {
			return;
		};
		let mut go = false;
		let mut erase = false;
		egui::CentralPanel::default().show(ui, |ui| {
			ui.add_space(ui.available_height() * 0.18);
			let width = ui.available_width().min(380.0);
			ui.horizontal(|ui| {
				ui.add_space((ui.available_width() - width) / 2.0);
				ui.vertical(|ui| {
				ui.set_width(width);
				ui.vertical_centered(|ui| {
				ui.heading(RichText::new("BetterSearch").size(28.0));
				ui.add_space(12.0);
				match mode {
					Mode::First => {
						ui.label("Choose a PIN. Your settings, search history, links and the posts you've seen are kept encrypted with it, and only unlocked in memory while the app is open.");
						ui.add_space(6.0);
						ui.label(RichText::new(PIN_ADVICE).weak());
						if data_dir().join("bettersearch.db").exists() || Settings::legacy_path().exists() {
							ui.add_space(6.0);
							ui.label(RichText::new("Your existing data will be moved in, and the old unencrypted files deleted.").weak());
						}
					}
					Mode::Renew => {
						ui.label(format!(
							"Your PIN is shorter than {MIN_PIN} characters, short enough that someone with a copy of your vault file could guess it within hours. Choose a new one to go on; nothing else changes."
						));
						ui.add_space(6.0);
						ui.label(RichText::new(PIN_ADVICE).weak());
					}
					Mode::Unlock => {
						ui.label("Enter your PIN to unlock.");
					}
				}
				ui.add_space(10.0);
				if !ui.input(|i| i.focused) {
					ui.label(RichText::new("Click here to type.").color(Color32::from_rgb(230, 170, 60)));
				}
				// Fixed ids: the hint above comes and goes with window focus, and
				// would otherwise shift the fields' ids and drop their focus.
				let r = ui.add(egui::TextEdit::singleline(&mut s.pin).id_salt("pin").password(true).hint_text("PIN").desired_width(200.0));
				if !s.focused_once {
					s.focused_once = true;
					r.request_focus();
				}
				let mut enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				if choosing {
					let r = ui.add(egui::TextEdit::singleline(&mut s.confirm).id_salt("pin-again").password(true).hint_text("again").desired_width(200.0));
					enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
				}
				ui.add_space(6.0);
				let waiting = now < s.retry_at;
				let label = match mode {
					Mode::First => "Set PIN",
					Mode::Renew => "Set new PIN",
					Mode::Unlock => "Unlock",
				};
				if ui.add_enabled(!waiting, egui::Button::new(label)).clicked() || (enter && !waiting) {
					go = true;
				}
				if waiting {
					ui.label(RichText::new(format!("Try again in {:.0}s", s.retry_at - now)).weak());
					ui.ctx().request_repaint_after(Duration::from_millis(250));
				}
				if let Some(e) = &s.error {
					ui.colored_label(Color32::from_rgb(220, 90, 90), e);
				}
				if mode == Mode::Unlock {
					ui.add_space(24.0);
					egui::CollapsingHeader::new(RichText::new("Forgot your PIN?").weak()).show(ui, |ui| {
						ui.label("It can't be recovered. You can erase everything and start over: links, tags, history and settings are gone for good.");
						ui.add(egui::TextEdit::singleline(&mut s.erase).hint_text("type ERASE").desired_width(120.0));
						if ui.add_enabled(s.erase == "ERASE", egui::Button::new("Erase and start over")).clicked() {
							erase = true;
						}
					});
				}
				});
				});
			});
		});
		if erase {
			s.error = self.vault.erase().err().map(|e| e.to_string());
			s.erase.clear();
			return;
		}
		if go {
			self.submit();
		}
	}

	/// The PIN typed in, checked and put to use: unlocking, setting up, or
	/// replacing a short PIN.
	fn submit(&mut self) {
		let Some(mode) = self.mode() else {
			return;
		};
		let (State::Locked(s) | State::Renew(s, _)) = &mut self.state else {
			return;
		};
		if let Some(e) = pin_problem(mode, &s.pin, &s.confirm) {
			s.error = Some(e);
			return;
		}
		let (pin, fails) = (Zeroizing::new(std::mem::take(&mut s.pin)), s.fails);
		s.confirm.zeroize();
		match mode {
			Mode::First => self.run("Setting up…", fails, false, move |v| create(v, &pin)),
			Mode::Unlock => {
				let renew = pin.chars().count() < MIN_PIN;
				self.run("Unlocking…", fails, renew, move |v| open(v, &pin));
			}
			Mode::Renew => {
				let State::Renew(_, store) = std::mem::replace(&mut self.state, State::Locked(LockScreen::default())) else {
					return;
				};
				self.run("Saving your new PIN…", 0, false, move |v| rekey(v, &pin, *store));
			}
		}
	}
}

const PIN_ADVICE: &str = "There's no way back in without it. Someone who copies your files can try about one PIN a second per processor core: \
	every 6-digit PIN within days, where 4 digits took hours. A few words, or a longer mix of letters and digits, holds out for years.";

/// What's wrong with a PIN typed on the lock screen, if anything. Only a PIN
/// being chosen has to be long enough: one from when 4 was the least still unlocks.
fn pin_problem(mode: Mode, pin: &str, confirm: &str) -> Option<String> {
	if mode == Mode::Unlock {
		pin.is_empty().then(|| "Type your PIN first.".into())
	} else if pin.chars().count() < MIN_PIN {
		Some(format!("Use at least {MIN_PIN} characters."))
	} else if pin != confirm {
		Some("The two don't match.".into())
	} else {
		None
	}
}

/// First run: a key for the new PIN, and a database with anything from before
/// the vault moved in. The old plain files go only once the vault reads back.
fn create(vault: &Vault, pin: &str) -> Opened {
	let fail = |e: String| Err((e, false));
	let key = Key::new(pin).map_err(|e| (e.to_string(), false))?;
	let legacy_db = data_dir().join("bettersearch.db");
	let mut store = match if legacy_db.exists() { Store::import_file(&legacy_db) } else { Store::in_memory() } {
		Ok(s) => s,
		Err(e) => return fail(format!("Couldn't read the old database: {e}")),
	};
	if let Ok(json) = std::fs::read_to_string(Settings::legacy_path()).map(Zeroizing::new) {
		let _ = store.set_value("settings", &json);
	}
	let bytes = store.to_bytes().map_err(|e| (e.to_string(), false))?;
	vault.write(&key, &bytes).map_err(|e| (e.to_string(), false))?;
	match vault.unlock(pin) {
		Ok((_, back)) if back.as_slice() == bytes.as_slice() => {}
		_ => return fail("The vault didn't read back; the old files were left alone.".into()),
	}
	let old = [legacy_db.clone(), legacy_db.with_extension("db-wal"), legacy_db.with_extension("db-shm"), Settings::legacy_path()];
	for f in old {
		let _ = std::fs::remove_file(f);
	}
	Ok((Arc::new(key), store))
}

fn open(vault: &Vault, pin: &str) -> Opened {
	let (key, bytes) = vault.unlock(pin).map_err(|e| (e.to_string(), matches!(e, VaultError::WrongPin)))?;
	let store = Store::from_bytes(&bytes).map_err(|e| (format!("The vault opened but its database didn't: {e}"), false))?;
	Ok((Arc::new(key), store))
}

/// A key for a new PIN, and the vault sealed with it in place of the old one.
fn rekey(vault: &Vault, pin: &str, mut store: Store) -> Opened {
	let fail = |e: String| (e, false);
	let key = Key::new(pin).map_err(|e| fail(e.to_string()))?;
	let bytes = store.to_bytes().map_err(|e| fail(e.to_string()))?;
	vault.write(&key, &bytes).map_err(|e| fail(e.to_string()))?;
	Ok((Arc::new(key), store))
}

impl Root {
	fn screen(&mut self, ui: &mut egui::Ui) {
		match &mut self.state {
			State::Locked(_) | State::Renew(..) => self.lock_screen(ui),
			State::Busy { rx, label, fails, renew } => {
				let (label, fails, renew) = (*label, *fails, *renew);
				match rx.try_recv() {
					// The short PIN's key is dropped here: nothing is sealed with it again.
					Ok(Ok((_, store))) if renew => self.state = State::Renew(LockScreen::default(), Box::new(store)),
					Ok(Ok((key, store))) => {
						let saver = Saver::new(self.vault.clone(), key, store.changes());
						self.state = State::Open(Box::new(App::new(self.ctx.clone(), store, saver)));
					}
					Ok(Err((error, wrong))) => {
						let fails = fails + wrong as u32;
						// Each wrong PIN waits a little longer, up to half a minute.
						let retry_at = if wrong { ui.input(|i| i.time) + f64::from(fails.min(30)) } else { 0.0 };
						self.state = State::Locked(LockScreen { error: Some(error), fails, retry_at, ..Default::default() });
					}
					Err(_) => {
						egui::CentralPanel::default().show(ui, |ui| {
							ui.centered_and_justified(|ui| {
								ui.horizontal(|ui| {
									ui.spinner();
									ui.label(label);
								});
							});
						});
					}
				}
			}
			State::Open(app) => {
				app.ui(ui);
				if app.lock_requested {
					app.shutdown();
					self.state = State::Locked(LockScreen::default());
				}
			}
		}
	}
}

impl eframe::App for Root {
	fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
		self.screen(ui);
	}

	fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
		if let State::Open(app) = &mut self.state {
			app.shutdown();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// One frame of the PIN screen with these input events, like eframe runs it.
	fn frame(root: &mut Root, events: Vec<egui::Event>) {
		frame_with(root, events, true);
	}

	fn frame_with(root: &mut Root, events: Vec<egui::Event>, window_focused: bool) {
		let input = egui::RawInput {
			events,
			focused: window_focused,
			screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
			..Default::default()
		};
		let ctx = root.ctx.clone();
		let mut out = ctx.run_ui(input, |ui| root.screen(ui));
		out.textures_delta.clear();
	}

	fn pin(root: &Root) -> String {
		match &root.state {
			State::Locked(s) => format!("{}|{}", s.pin, s.confirm),
			_ => "not locked".into(),
		}
	}

	#[test]
	fn typing_reaches_the_pin_fields() {
		let dir = std::env::temp_dir().join(format!("bs-lock-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let mut root = Root { ctx: egui::Context::default(), vault: Vault::in_dir(&dir), state: State::Locked(LockScreen::default()) };
		for _ in 0..3 {
			frame(&mut root, vec![]);
		}
		frame(&mut root, vec![egui::Event::Text("4829".into())]);
		frame(&mut root, vec![]);
		println!("after typing: {}", pin(&root));
		assert_eq!(pin(&root), "4829|");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	fn new_root(name: &str) -> (Root, std::path::PathBuf) {
		let dir = std::env::temp_dir().join(format!("bs-lock-{name}-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		(Root { ctx: egui::Context::default(), vault: Vault::in_dir(&dir), state: State::Locked(LockScreen::default()) }, dir)
	}

	/// The window often starts unfocused (the hint shows), then gets focus (the
	/// hint goes). The PIN field must keep its focus through that, or typing
	/// lands nowhere.
	#[test]
	fn pin_field_keeps_focus_when_the_window_gets_it() {
		let (mut root, dir) = new_root("focus");
		frame_with(&mut root, vec![], false);
		frame_with(&mut root, vec![], false);
		frame_with(&mut root, vec![egui::Event::WindowFocused(true)], true);
		frame(&mut root, vec![]);
		frame(&mut root, vec![egui::Event::Text("4829".into())]);
		frame(&mut root, vec![]);
		assert_eq!(pin(&root), "4829|");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// Asking for focus every frame reset the window's text input each time and
	/// stopped all keys and clicks; it's asked for once. So once something else
	/// has focus, the screen doesn't take it back, and the IME request stays put.
	#[test]
	fn pin_field_asks_for_focus_once() {
		let (mut root, dir) = new_root("once");
		for _ in 0..3 {
			frame(&mut root, vec![]);
		}
		let field = root.ctx.memory(|m| m.focused()).expect("the PIN field has focus at first");
		root.ctx.memory_mut(|m| m.surrender_focus(field));
		for _ in 0..5 {
			frame(&mut root, vec![]);
			assert_eq!(root.ctx.memory(|m| m.focused()), None, "focus was taken back");
		}
		std::fs::remove_dir_all(&dir).unwrap();
	}

	fn type_pins(root: &mut Root, pin: &str, again: &str) {
		let (State::Locked(s) | State::Renew(s, _)) = &mut root.state else {
			panic!("the PIN screen isn't showing");
		};
		(s.pin, s.confirm) = (pin.into(), again.into());
	}

	fn error(root: &Root) -> Option<String> {
		match &root.state {
			State::Locked(s) | State::Renew(s, _) => s.error.clone(),
			_ => None,
		}
	}

	/// Frames until the key is made.
	fn settle(root: &mut Root) {
		let start = Instant::now();
		while matches!(root.state, State::Busy { .. }) {
			assert!(start.elapsed() < Duration::from_secs(60), "still busy");
			std::thread::sleep(Duration::from_millis(10));
			frame(root, vec![]);
		}
	}

	/// A PIN being chosen needs 6 characters, and both to match.
	#[test]
	fn new_pins_need_six_characters() {
		let (mut root, dir) = new_root("six");
		type_pins(&mut root, "48291", "48291");
		root.submit();
		assert_eq!(error(&root).as_deref(), Some("Use at least 6 characters."));
		type_pins(&mut root, "482913", "482914");
		root.submit();
		assert_eq!(error(&root).as_deref(), Some("The two don't match."));
		assert!(matches!(root.state, State::Locked(_)) && !root.vault.exists(), "nothing was set up");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// A vault from when 4 characters were enough opens with its PIN, then
	/// goes no further until a long enough one replaces it.
	#[test]
	fn short_pin_opens_then_is_replaced() {
		let (mut root, dir) = new_root("renew");
		let mut store = Store::in_memory().unwrap();
		store.set_value("kept", "yes").unwrap();
		root.vault.write(&Key::for_tests(), &store.to_bytes().unwrap()).unwrap();
		type_pins(&mut root, "test", "");
		root.submit();
		settle(&mut root);
		assert!(matches!(root.state, State::Renew(..)), "a new PIN is asked for");
		type_pins(&mut root, "tests", "tests");
		root.submit();
		assert_eq!(error(&root).as_deref(), Some("Use at least 6 characters."));
		type_pins(&mut root, "tested1", "tested1");
		root.submit();
		settle(&mut root);
		let State::Open(app) = &mut root.state else {
			panic!("the app didn't open");
		};
		app.shutdown();
		assert!(matches!(root.vault.unlock("test"), Err(VaultError::WrongPin)), "the old PIN still opens it");
		let (_, bytes) = root.vault.unlock("tested1").unwrap();
		assert_eq!(Store::from_bytes(&bytes).unwrap().value("kept").unwrap().as_deref(), Some("yes"));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
