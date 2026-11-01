//! A sign-in window with a real web view, for sites that answer a plain client
//! with a bot check.
//!
//! The visitor passes the check (or logs in) in here, presses Done, and the
//! cookies of that very session come back together with its User-Agent — the
//! pair the site expects. Nothing is written to disk by this window: the
//! session is ephemeral, so it lives and dies with it until the caller puts the
//! cookie in the vault.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::OnceLock;
use std::time::Duration;

use gtk::prelude::*;
use webkit6::prelude::*;

use crate::cookies::CLEARANCE;

pub struct Login {
	pub host: String,
	rx: Receiver<Result<(String, String), String>>,
}

pub enum Outcome {
	Pending,
	Done(String, String),
	Failed(String),
}

impl Login {
	pub fn poll(&self) -> Outcome {
		match self.rx.try_recv() {
			Ok(Ok((cookie, ua))) => Outcome::Done(cookie, ua),
			Ok(Err(e)) => Outcome::Failed(e),
			Err(TryRecvError::Disconnected) => Outcome::Failed("the window closed".into()),
			Err(TryRecvError::Empty) => Outcome::Pending,
		}
	}
}

/// The GTK thread stays up for the life of the process: GTK can only start
/// once, and only on the thread that owns it.
struct Job {
	host: String,
	out: Sender<Result<(String, String), String>>,
}

static WORKER: OnceLock<Sender<Job>> = OnceLock::new();

fn worker() -> &'static Sender<Job> {
	WORKER.get_or_init(|| {
		let (jobs, queue) = channel::<Job>();
		let (ready, up) = channel::<bool>();
		std::thread::spawn(move || {
			if gtk::init().is_err() {
				let _ = ready.send(false);
				return;
			}
			let _ = ready.send(true);
			let main = gtk::glib::MainLoop::new(None, false);
			gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
				while let Ok(job) = queue.try_recv() {
					window(job);
				}
				gtk::glib::ControlFlow::Continue
			});
			main.run();
		});
		// Wait for GTK to be up, so the first window isn't asked for too early.
		let _ = up.recv_timeout(Duration::from_secs(5));
		jobs
	})
}

/// Opens the window. `None` when there's no host to open.
pub fn open(host: &str) -> Option<Login> {
	let host = host.trim().trim_start_matches('.').to_lowercase();
	if host.is_empty() {
		return None;
	}
	let (out, rx) = channel();
	if worker().send(Job { host: host.clone(), out }).is_err() {
		return None;
	}
	Some(Login { host, rx })
}

fn window(job: Job) {
	let Job { host, out } = job;
	let session = webkit6::NetworkSession::new_ephemeral();
	let view = webkit6::WebView::builder().network_session(&session).build();
	view.load_uri(&format!("https://{host}/"));

	let win = gtk::Window::builder()
		.title(format!("Sign in — {host}"))
		.default_width(940)
		.default_height(720)
		.build();
	let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
	let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
	bar.set_margin_top(6);
	bar.set_margin_bottom(6);
	bar.set_margin_start(6);
	bar.set_margin_end(6);
	let done = gtk::Button::with_label("Done — keep the cookies");
	let cancel = gtk::Button::with_label("Cancel");
	let note = gtk::Label::new(Some("Pass the check (or sign in), then press Done."));
	note.set_xalign(0.0);
	bar.append(&done);
	bar.append(&cancel);
	bar.append(&note);
	root.append(&view);
	root.append(&bar);
	win.set_child(Some(&root));
	win.present();

	{
		let out = out.clone();
		let win = win.clone();
		cancel.connect_clicked(move |_| {
			let _ = out.send(Err("cancelled".into()));
			win.close();
		});
	}

	let uri = format!("https://{host}/");
	done.connect_clicked(move |_| {
		let Some(manager) = session.cookie_manager() else {
			let _ = out.send(Err("the web view has no cookie store".into()));
			win.close();
			return;
		};
		let ua = <webkit6::WebView as webkit6::prelude::WebViewExt>::settings(&view)
			.and_then(|s| s.user_agent())
			.map(|u| u.to_string())
			.unwrap_or_else(|| bettersearch_core::sites::WEBKIT_UA.to_string());
		let (out, win, uri) = (out.clone(), win.clone(), uri.clone());
		// `cookies_future` is a GIO future: awaited on the GTK main context.
		gtk::glib::MainContext::default().spawn_local(async move {
			let asked = match manager.cookies_future(&uri).await {
				Err(e) => Err(e.to_string()),
				Ok(mut cookies) => {
					let mut pairs: Vec<(String, String)> =
						cookies.iter_mut().filter_map(|c| Some((c.name()?.to_string(), c.value()?.to_string()))).collect();
					if pairs.is_empty() {
						Err("no cookies yet — the check has to be passed first".into())
					} else {
						pairs.sort_by_key(|(n, _)| !CLEARANCE.contains(&n.as_str()));
						let header = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ");
						Ok((header, ua))
					}
				}
			};
			let _ = out.send(asked);
			win.close();
		});
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_hosts_are_refused() {
		assert!(open("  ").is_none());
	}
}
