// Plays GIFs and videos inside the app, with no other program's window.
//
// GIFs (and animated WebP) are decoded here, every frame in memory. A video is
// downloaded into memory, put in a RAM-only file (memfd: it never touches the
// disk), and decoded by ffmpeg into frames this app draws; ffmpeg also plays
// the sound. ffmpeg may only read that one file (`-protocol_whitelist file`),
// so a crafted video can't make it fetch URLs or open other files, and the
// file is handed only to ffmpeg, not to anything else the app starts.

use std::io::{Cursor, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui::{self, ColorImage};
use image::{AnimationDecoder, ImageDecoder};

/// Frames are scaled down to this width at most.
const MAX_WIDTH: u32 = 1280;
/// Bytes of a video downloaded before trying to start it: enough for the
/// header of anything that can start early. (Tiny in tests, so a small test
/// video still arrives in pieces.)
#[cfg(not(test))]
const START_AFTER: usize = 2 * 1024 * 1024;
#[cfg(test)]
const START_AFTER: usize = 8 * 1024;
/// Runs in a row that may end without a frame before a video is given up on:
/// one for the graphics card failing, more for anything else.
const MAX_EMPTY_RUNS: u32 = 3;
/// Animations stop adding frames past this much memory.
const MAX_ANIMATION_BYTES: usize = 400 * 1024 * 1024;

/// An animation's frames and how long each shows, in milliseconds.
pub type Frames = Vec<(ColorImage, u32)>;

/// Animation canvases larger than this many pixels aren't decoded: every
/// frame is drawn at full canvas size before it is scaled down.
const MAX_CANVAS_PIXELS: u64 = 4096 * 4096;

/// All frames of an animated GIF or WebP. A still image gives one frame.
pub fn decode_animation(bytes: &[u8]) -> Result<Frames, String> {
	match image::guess_format(bytes).map_err(|e| e.to_string())? {
		image::ImageFormat::Gif => {
			let d = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
			check_canvas(d.dimensions())?;
			scale_frames(d.into_frames())
		}
		image::ImageFormat::WebP => {
			let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
			check_canvas(d.dimensions())?;
			if d.has_animation() {
				scale_frames(d.into_frames())
			} else {
				let still = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
				scale_frames(image::Frames::new(Box::new(std::iter::once(Ok(image::Frame::new(still.to_rgba8()))))))
			}
		}
		_ => {
			let still = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
			scale_frames(image::Frames::new(Box::new(std::iter::once(Ok(image::Frame::new(still.to_rgba8()))))))
		}
	}
}

fn check_canvas((w, h): (u32, u32)) -> Result<(), String> {
	if w as u64 * h as u64 > MAX_CANVAS_PIXELS {
		return Err(format!("animation too large to play ({w}×{h})"));
	}
	Ok(())
}

/// Decodes frames one at a time, scaling each down as it comes, so memory
/// stops at `MAX_ANIMATION_BYTES` instead of holding every full-size frame.
fn scale_frames(frames: image::Frames<'_>) -> Result<Frames, String> {
	let mut out = Vec::new();
	let mut used = 0;
	for f in frames {
		let f = match f {
			Ok(f) => f,
			// A broken frame late in the file: play what came before it.
			Err(_) if !out.is_empty() => break,
			Err(e) => return Err(e.to_string()),
		};
		let (num, den) = f.delay().numer_denom_ms();
		// Browsers show "as fast as possible" (0 or 10 ms) frames at 100 ms; so do we.
		let ms = num.checked_div(den).unwrap_or(100);
		let ms = if ms < 20 { 100 } else { ms };
		let mut buf = f.into_buffer();
		if buf.width() > MAX_WIDTH {
			let h = (buf.height() as u64 * MAX_WIDTH as u64 / buf.width() as u64).max(1) as u32;
			buf = image::imageops::thumbnail(&buf, MAX_WIDTH, h);
		}
		used += buf.as_raw().len();
		if used > MAX_ANIMATION_BYTES && !out.is_empty() {
			break;
		}
		out.push((ColorImage::from_rgba_unmultiplied([buf.width() as usize, buf.height() as usize], buf.as_raw()), ms));
	}
	if out.is_empty() {
		return Err("no frames".into());
	}
	Ok(out)
}

/// Whether ffmpeg is there to play videos with.
pub fn can_play_video() -> bool {
	Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

static HW: OnceLock<Option<String>> = OnceLock::new();

/// The graphics card to decode on: the one programs are pointed at
/// (`DRI_PRIME=pci-0000_0a_00_0`), else the one the screen started on.
fn render_node() -> Option<String> {
	if let Some(p) = std::env::var("DRI_PRIME").ok().and_then(|v| prime_node(&v)).filter(|p| std::path::Path::new(p).exists()) {
		return Some(p);
	}
	let mut nodes: Vec<_> = std::fs::read_dir("/sys/class/drm").ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
	nodes.sort();
	let first = nodes.into_iter().filter(|n| n.starts_with("renderD")).find(|n| {
		std::fs::read_to_string(format!("/sys/class/drm/{n}/device/boot_vga")).is_ok_and(|v| v.trim() == "1")
	})?;
	Some(format!("/dev/dri/{first}"))
}

/// The render node `DRI_PRIME=pci-0000_0a_00_0` names: the card at 0000:0a:00.0.
fn prime_node(prime: &str) -> Option<String> {
	let parts: Vec<&str> = prime.strip_prefix("pci-")?.split('_').collect();
	let [dom, bus, dev, func] = parts.as_slice() else { return None };
	Some(format!("/dev/dri/by-path/pci-{dom}:{bus}:{dev}.{func}-render"))
}

/// The graphics card's video decoder (VA-API), if ffmpeg can use it here.
/// Asked once, with a short ffmpeg run; without one, videos decode on the
/// processor. A codec the card can't do falls back to the processor too.
pub fn hw_decoder() -> Option<&'static str> {
	HW.get_or_init(|| {
		let node = render_node()?;
		let works = Command::new("ffmpeg")
			.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-init_hw_device", &format!("vaapi=va:{node}"), "-filter_hw_device", "va"])
			.args(["-f", "lavfi", "-i", "nullsrc=s=64x64", "-frames:v", "1", "-vf", "format=nv12,hwupload", "-f", "null", "-"])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.is_ok_and(|s| s.success());
		works.then_some(node)
	})
	.as_deref()
}

/// `hw_decoder`, without waiting: `None` until it has been asked.
pub fn hw_decoder_known() -> Option<Option<&'static str>> {
	HW.get().map(|d| d.as_deref())
}

/// A video held in memory, playing through ffmpeg.
pub struct Video {
	/// The RAM-only file holding the video.
	file: OwnedFd,
	pub width: u32,
	pub height: u32,
	/// Seconds; 0 when the file doesn't say.
	pub duration: f64,
	fps: f64,
	pub has_audio: bool,
	pub muted: bool,
	/// How loud, 0 to 1.
	pub volume: f32,
	pub paused: bool,
	child: Option<Child>,
	/// Where the current run started, and frames shown since.
	start: f64,
	frames: Arc<AtomicU64>,
	/// Which run is current. A stopped run's reader thread can still be
	/// draining its pipe; it checks this so it can't count frames for, or end,
	/// the run that replaced it.
	run: Arc<AtomicU64>,
	/// The newest frame and its number, for the window to show.
	latest: Arc<Mutex<Option<(u64, ColorImage)>>>,
	ended: Arc<AtomicBool>,
	/// Width and height frames are scaled to.
	out: (u32, u32),
	/// Whether all of it has downloaded (or stopped downloading).
	loaded: Arc<AtomicBool>,
	/// Tells the download to stop, once nothing wants it.
	cancel: Arc<AtomicBool>,
	/// Set to stop handing the current run the video, while it downloads.
	feeding: Option<Arc<AtomicBool>>,
	/// Whether the current run decodes on the graphics card, and whether one
	/// did and showed nothing (then the processor does it from there on).
	on_card: bool,
	card_failed: bool,
	/// Runs in a row that ended without a single frame. After a few, it's
	/// given up on rather than restarted forever.
	empty_runs: u32,
}

impl Video {
	/// Puts a video in a RAM-only file as it downloads, reads its size, length
	/// and streams, and returns once it can start: after the first couple of
	/// MB, or after all of it for an MP4 that keeps its index at the end. The
	/// rest comes in behind it.
	#[cfg(test)]
	pub fn stream(from: Box<dyn Read + Send>) -> Result<Video, String> {
		Self::stream_while(from, &|| true)
	}

	/// `stream`, giving up (and letting go of what came) as soon as `keep`
	/// turns false: the window moved on to another post before this one could
	/// start. After it starts, dropping the `Video` stops the download.
	pub fn stream_while(mut from: Box<dyn Read + Send>, keep: &dyn Fn() -> bool) -> Result<Video, String> {
		// Asked here, off the window's thread, so playing never waits on it.
		hw_decoder();
		let mut v = Video::empty()?;
		let mut file = v.writer()?;
		let mut buf = vec![0u8; 64 * 1024];
		let (mut have, mut done) = (0, false);
		let mut read = |file: &mut std::fs::File, done: &mut bool| -> Result<usize, String> {
			if !keep() {
				return Err("no longer wanted".into());
			}
			let n = from.read(&mut buf).map_err(|e| format!("download stopped: {e}"))?;
			file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
			*done = n == 0;
			Ok(n)
		};
		while have < START_AFTER && !done {
			have += read(&mut file, &mut done)?;
		}
		// An MP4 with its index at the end can't start until all of it is in.
		if !done && v.probe().is_err() {
			while !done {
				read(&mut file, &mut done)?;
			}
		}
		if done {
			v.loaded.store(true, Ordering::SeqCst);
			v.probe()?;
			return Ok(v);
		}
		let (loaded, cancel) = (v.loaded.clone(), v.cancel.clone());
		std::thread::spawn(move || {
			// A failed download ends here too: what came plays, then it stops.
			while !cancel.load(Ordering::SeqCst) {
				match from.read(&mut buf) {
					Ok(0) | Err(_) => break,
					Ok(n) if file.write_all(&buf[..n]).is_err() => break,
					Ok(_) => {}
				}
			}
			loaded.store(true, Ordering::SeqCst);
		});
		Ok(v)
	}

	fn empty() -> Result<Video, String> {
		// Close-on-exec: only the ffmpeg runs below get it (see `command`).
		let name = c"bettersearch-video";
		let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
		if fd < 0 {
			return Err(format!("couldn't make a memory file: {}", std::io::Error::last_os_error()));
		}
		let file = unsafe { OwnedFd::from_raw_fd(fd) };
		Ok(Video {
			file,
			width: 0,
			height: 0,
			duration: 0.0,
			fps: 30.0,
			has_audio: false,
			muted: true,
			volume: 1.0,
			paused: false,
			child: None,
			start: 0.0,
			frames: Arc::new(AtomicU64::new(0)),
			run: Arc::new(AtomicU64::new(0)),
			latest: Arc::new(Mutex::new(None)),
			ended: Arc::new(AtomicBool::new(false)),
			out: (0, 0),
			loaded: Arc::new(AtomicBool::new(false)),
			cancel: Arc::new(AtomicBool::new(false)),
			feeding: None,
			on_card: false,
			card_failed: false,
			empty_runs: 0,
		})
	}

	/// The memory file, to write the video into.
	fn writer(&self) -> Result<std::fs::File, String> {
		Ok(std::fs::File::from(self.file.try_clone().map_err(|e| e.to_string())?))
	}

	/// Whether all of it has downloaded.
	pub fn loaded(&self) -> bool {
		self.loaded.load(Ordering::SeqCst)
	}

	fn path(&self) -> String {
		format!("/proc/self/fd/{}", self.file.as_raw_fd())
	}

	/// A command for ffmpeg or ffprobe that may read only the memory file.
	fn command(&self, program: &str) -> Command {
		let mut c = Command::new(program);
		c.args(["-hide_banner", "-loglevel", "error", "-protocol_whitelist", "file"]);
		let fd = self.file.as_raw_fd();
		// Hands the memory file to this one program (it's close-on-exec otherwise).
		unsafe {
			c.pre_exec(move || {
				if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
					return Err(std::io::Error::last_os_error());
				}
				Ok(())
			});
		}
		c.stdin(Stdio::null()).stderr(Stdio::null());
		c
	}

	/// A command for an ffmpeg handed the video on its stdin while it downloads,
	/// and allowed to read nothing else (not even the memory file).
	fn fed_command(&self) -> Command {
		let mut c = Command::new("ffmpeg");
		c.args(["-hide_banner", "-loglevel", "error", "-protocol_whitelist", "pipe"]);
		c.stdin(Stdio::piped()).stderr(Stdio::null());
		c
	}

	fn probe(&mut self) -> Result<(), String> {
		let out = self
			.command("ffprobe")
			.args(["-show_entries", "stream=codec_type,width,height,avg_frame_rate:format=duration", "-of", "json"])
			.arg(self.path())
			.stdout(Stdio::piped())
			.output()
			.map_err(|e| format!("ffprobe isn't available: {e}"))?;
		let info: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "ffprobe couldn't read the video".to_string())?;
		let streams = info["streams"].as_array().cloned().unwrap_or_default();
		let video = streams.iter().find(|s| s["codec_type"] == "video").ok_or("the file has no video")?;
		self.width = video["width"].as_u64().unwrap_or(0) as u32;
		self.height = video["height"].as_u64().unwrap_or(0) as u32;
		if self.width == 0 || self.height == 0 {
			return Err("the video has no size".into());
		}
		if let Some((n, d)) = video["avg_frame_rate"].as_str().and_then(|r| r.split_once('/')) {
			let (n, d): (f64, f64) = (n.parse().unwrap_or(0.0), d.parse().unwrap_or(0.0));
			if n > 0.0 && d > 0.0 {
				self.fps = n / d;
			}
		}
		self.duration = info["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
		self.has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
		// Even sizes, at most MAX_WIDTH wide.
		let w = self.width.min(MAX_WIDTH) & !1;
		let h = ((self.height as u64 * w as u64 / self.width as u64) as u32 & !1).max(2);
		self.out = (w.max(2), h);
		Ok(())
	}

	/// Plays from `at` seconds: ffmpeg decodes at the video's own pace, sends
	/// frames here and the sound to the speakers.
	pub fn play_from(&mut self, at: f64, ctx: &egui::Context) {
		// Whether ffmpeg quit by itself, rather than being stopped for a pause or seek.
		let quit = self.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
		self.stop();
		let shown = self.frames.load(Ordering::SeqCst);
		if self.on_card && self.ended.load(Ordering::SeqCst) && shown == 0 {
			self.card_failed = true;
		}
		if quit && shown == 0 {
			self.empty_runs += 1;
		} else if shown > 0 {
			self.empty_runs = 0;
		}
		if self.broken() {
			self.ended.store(true, Ordering::SeqCst);
			return;
		}
		self.paused = false;
		self.start = at.max(0.0);
		self.frames.store(0, Ordering::SeqCst);
		self.ended.store(false, Ordering::SeqCst);
		let (w, h) = self.out;
		// Still downloading, ffmpeg reads it from a pipe as it comes. From the
		// file it would look for the index at the end (where WebMs keep it)
		// and wait there until it all arrived.
		let fed = !self.loaded();
		let mut cmd = if fed { self.fed_command() } else { self.command("ffmpeg") };
		// A thread or two, not one per core: by itself ffmpeg spreads even a
		// small video over every core, a load that makes some computers' screens
		// glitch. Two keep up with HD, four with anything bigger.
		let threads = if self.width > 1920 { "4" } else { "2" };
		cmd.args(["-nostdin", "-filter_threads", "1", "-threads", threads]);
		// The graphics card decodes, where it can; frames come back to memory for scaling.
		self.on_card = false;
		if let Some(Some(node)) = hw_decoder_known().filter(|_| !self.card_failed) {
			cmd.args(["-hwaccel", "vaapi", "-hwaccel_device", node]);
			self.on_card = true;
		}
		let at = format!("{:.3}", self.start);
		// A pipe can't seek (and ffmpeg loses frames trying), so from one it
		// reads from the top, the part before `start` in a burst, and each
		// output drops what comes before it.
		let skip: &[&str] = if fed && self.start > 0.0 { &["-ss", &at] } else { &[] };
		if fed {
			if self.start > 0.0 {
				cmd.args(["-readrate_initial_burst", &at]);
			}
			cmd.args(["-re", "-i", "pipe:0"]);
		} else {
			cmd.args(["-ss", &at, "-re", "-i"]).arg(self.path());
		}
		cmd.args(["-map", "0:v:0"]).args(skip);
		cmd.args(["-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
		if self.has_audio && !self.muted {
			// Squared, so the slider's middle sounds half as loud rather than nearly as loud.
			cmd.args(["-map", "0:a:0"]).args(skip).args(["-af", &format!("volume={:.3}", self.volume * self.volume)]);
			cmd.args(sound_output());
		}
		let Ok(mut child) = cmd.stdout(Stdio::piped()).spawn() else {
			self.ended.store(true, Ordering::SeqCst);
			return;
		};
		if fed && let (Some(to), Ok(file)) = (child.stdin.take(), self.writer()) {
			let over = Arc::new(AtomicBool::new(false));
			self.feeding = Some(over.clone());
			let loaded = self.loaded.clone();
			std::thread::spawn(move || feed(file, to, loaded, over));
		}
		let mut out = child.stdout.take().expect("piped");
		let (latest, frames, ended, ctx) = (self.latest.clone(), self.frames.clone(), self.ended.clone(), ctx.clone());
		let (run, this) = (self.run.clone(), self.run.fetch_add(1, Ordering::SeqCst) + 1);
		std::thread::spawn(move || {
			let mut buf = vec![0u8; w as usize * h as usize * 4];
			while out.read_exact(&mut buf).is_ok() && run.load(Ordering::SeqCst) == this {
				let n = frames.fetch_add(1, Ordering::SeqCst) + 1;
				*latest.lock().unwrap() = Some((n, ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &buf)));
				ctx.request_repaint();
			}
			// Only the current run ends the video; a replaced one just stops.
			if run.load(Ordering::SeqCst) == this {
				ended.store(true, Ordering::SeqCst);
			}
			ctx.request_repaint();
		});
		self.child = Some(child);
	}

	/// Seconds into the video.
	pub fn position(&self) -> f64 {
		let p = self.start + self.frames.load(Ordering::SeqCst) as f64 / self.fps;
		if self.duration > 0.0 { p.min(self.duration) } else { p }
	}

	/// Whether ffmpeg keeps stopping before showing anything (a codec it can
	/// read but not decode, say): then it isn't started again.
	pub fn broken(&self) -> bool {
		self.empty_runs >= MAX_EMPTY_RUNS
	}

	/// Whether it played to the end (to loop it).
	pub fn ended(&self) -> bool {
		self.ended.load(Ordering::SeqCst)
	}

	/// The newest frame, if newer than `seen`.
	pub fn frame_after(&self, seen: u64) -> Option<(u64, ColorImage)> {
		let l = self.latest.lock().unwrap();
		l.as_ref().filter(|(n, _)| *n > seen || (seen > 0 && *n < seen)).cloned()
	}

	/// Pausing stops ffmpeg; playing again starts it where it stopped.
	pub fn toggle_pause(&mut self, ctx: &egui::Context) {
		if self.paused {
			let at = self.start;
			self.play_from(at, ctx);
		} else {
			self.start = self.position();
			self.stop();
			self.paused = true;
		}
	}

	pub fn toggle_mute(&mut self, ctx: &egui::Context) {
		self.muted = !self.muted;
		if !self.paused {
			let at = self.position();
			self.play_from(at, ctx);
		}
	}

	/// Sets how loud (0 to 1) and turns the sound on; playing, it goes on from
	/// where it is at the new level (ffmpeg takes the level when it starts).
	pub fn set_volume(&mut self, volume: f32, ctx: &egui::Context) {
		self.volume = volume.clamp(0.0, 1.0);
		self.muted = false;
		if !self.paused {
			let at = self.position();
			self.play_from(at, ctx);
		}
	}

	fn stop(&mut self) {
		// The run's reader thread may still be draining its pipe: from here on
		// it counts for nothing.
		self.run.fetch_add(1, Ordering::SeqCst);
		if let Some(over) = self.feeding.take() {
			over.store(true, Ordering::SeqCst);
		}
		if let Some(mut c) = self.child.take() {
			let _ = c.kill();
			let _ = c.wait();
		}
	}
}

impl Drop for Video {
	fn drop(&mut self) {
		self.cancel.store(true, Ordering::SeqCst);
		self.stop();
	}
}

/// Hands ffmpeg the video as it downloads: what's in the memory file, then
/// what arrives, then the end once it's all there. Reads by position, so the
/// download's own place in the file is left alone.
fn feed(file: std::fs::File, mut to: std::process::ChildStdin, loaded: Arc<AtomicBool>, over: Arc<AtomicBool>) {
	use std::os::unix::fs::FileExt;
	let mut buf = vec![0u8; 64 * 1024];
	let mut at = 0;
	while !over.load(Ordering::SeqCst) {
		// Checked before reading: then nothing read means nothing more coming.
		let all = loaded.load(Ordering::SeqCst);
		match file.read_at(&mut buf, at) {
			Ok(0) if all => break,
			Ok(0) => std::thread::sleep(std::time::Duration::from_millis(20)),
			Ok(n) if to.write_all(&buf[..n]).is_ok() => at += n as u64,
			// ffmpeg was stopped, or the file can't be read.
			_ => break,
		}
	}
}

/// Where ffmpeg plays sound: PulseAudio (or PipeWire's Pulse) if it's running, else ALSA.
fn sound_output() -> Vec<String> {
	let pulse = std::env::var_os("PULSE_SERVER").is_some()
		|| std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("pulse/native").exists());
	if pulse { vec!["-f".into(), "pulse".into(), "BetterSearch".into()] } else { vec!["-f".into(), "alsa".into(), "default".into()] }
}

/// mm:ss
pub fn clock(seconds: f64) -> String {
	let s = seconds.max(0.0) as u64;
	format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A two-frame GIF made here: both frames come back, with their timing.
	#[test]
	fn decodes_every_gif_frame() {
		let mut bytes = Vec::new();
		{
			let mut enc = image::codecs::gif::GifEncoder::new(&mut bytes);
			for (shade, ms) in [(0u8, 50u32), (255, 0)] {
				let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([shade, shade, shade, 255]));
				enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(ms, 1))).unwrap();
			}
		}
		let frames = decode_animation(&bytes).unwrap();
		assert_eq!(frames.len(), 2);
		assert_eq!(frames[0].0.size, [4, 3]);
		assert_eq!((frames[0].1, frames[1].1), (50, 100));
		assert!(decode_animation(b"not an image").is_err());
	}

	/// A GIF of a few bytes can claim a huge canvas, and every frame would be
	/// drawn at that size: it's refused before any frame is decoded.
	#[test]
	fn refuses_a_huge_gif_canvas() {
		let mut gif = b"GIF89a".to_vec();
		gif.extend(20000u16.to_le_bytes());
		gif.extend(20000u16.to_le_bytes());
		gif.extend([0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
		gif.extend([b',', 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0, b';']);
		assert!(decode_animation(&gif).unwrap_err().contains("too large"));
	}

	#[test]
	fn finds_the_card_programs_are_pointed_at() {
		assert_eq!(prime_node("pci-0000_0a_00_0").as_deref(), Some("/dev/dri/by-path/pci-0000:0a:00.0-render"));
		assert_eq!(prime_node("1"), None);
		assert_eq!(prime_node("pci-0000_0a_00"), None);
	}

	/// A slow download: a few KB at a time, 20 ms apart.
	struct Trickle {
		bytes: Vec<u8>,
		at: usize,
		finished: Arc<AtomicBool>,
	}

	impl Read for Trickle {
		fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
			if self.at == self.bytes.len() {
				self.finished.store(true, Ordering::SeqCst);
				return Ok(0);
			}
			std::thread::sleep(std::time::Duration::from_millis(20));
			let n = buf.len().min(4096).min(self.bytes.len() - self.at);
			buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
			self.at += n;
			Ok(n)
		}
	}

	/// ffmpeg that quits before the first frame every time (here: an empty
	/// file) is started a few times, then not again, instead of on every repaint.
	#[test]
	fn gives_up_on_a_video_that_never_shows_a_frame() {
		if !can_play_video() {
			return;
		}
		let mut v = Video::empty().unwrap();
		v.loaded.store(true, Ordering::SeqCst);
		(v.width, v.height, v.out) = (16, 16, (16, 16));
		let ctx = egui::Context::default();
		let mut runs = 0;
		while !v.broken() && runs < 10 {
			v.play_from(0.0, &ctx);
			runs += 1;
			let t = std::time::Instant::now();
			while !v.ended() && t.elapsed().as_secs() < 10 {
				std::thread::sleep(std::time::Duration::from_millis(10));
			}
			// Let the process finish exiting, so it reads as having quit.
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
		assert!(v.broken(), "still restarting after {runs} runs");
		assert_eq!(runs, MAX_EMPTY_RUNS + 1);
		v.play_from(0.0, &ctx);
		assert!(v.child.is_none() && v.ended());
	}

	/// The window moved on before the video could start: the download stops.
	#[test]
	fn stops_downloading_a_video_nobody_wants() {
		let reads = Arc::new(AtomicU64::new(0));
		struct Endless(Arc<AtomicU64>);
		impl Read for Endless {
			fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
				self.0.fetch_add(1, Ordering::SeqCst);
				buf.fill(0);
				Ok(buf.len())
			}
		}
		let seen = reads.clone();
		let got = Video::stream_while(Box::new(Endless(reads.clone())), &move || seen.load(Ordering::SeqCst) < 3);
		assert_eq!(got.err().as_deref(), Some("no longer wanted"));
		assert_eq!(reads.load(Ordering::SeqCst), 3);
	}

	/// A video starts before it has all arrived, plays on as the rest comes in,
	/// and ends at its end. Skipped where ffmpeg isn't installed.
	#[test]
	fn plays_a_video_while_it_downloads() {
		if !can_play_video() {
			return;
		}
		// Written to a file, so it says how long it is, as site videos do.
		let dir = std::env::temp_dir().join(format!("bs-stream-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("v.webm");
		let made = Command::new("ffmpeg")
			.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=3"])
			.args(["-vf", "noise=alls=40:allf=t", "-c:v", "libvpx", "-b:v", "800k", "-threads", "2"])
			.arg(&path)
			.stdin(Stdio::null())
			.status()
			.unwrap();
		if !made.success() {
			return;
		}
		let bytes = std::fs::read(&path).unwrap();
		std::fs::remove_dir_all(&dir).unwrap();
		assert!(bytes.len() > 4 * START_AFTER, "too small to arrive in pieces: {}", bytes.len());
		let finished = Arc::new(AtomicBool::new(false));
		let mut v = Video::stream(Box::new(Trickle { bytes, at: 0, finished: finished.clone() })).unwrap();
		assert!(!v.loaded() && !finished.load(Ordering::SeqCst), "it waited for the whole file");
		assert_eq!((v.width, v.height), (320, 240));
		v.play_from(0.0, &egui::Context::default());
		let t = std::time::Instant::now();
		let (mut first, mut resumed) = (None, None);
		while !v.ended() && t.elapsed().as_secs() < 15 {
			std::thread::sleep(std::time::Duration::from_millis(20));
			if first.is_none() && v.frame_after(0).is_some() {
				first = Some((t.elapsed(), v.loaded()));
			}
			// Paused and played again mid-download, it goes on from where it was.
			if resumed.is_none() && !v.loaded() && v.position() >= 0.5 {
				let ctx = egui::Context::default();
				v.toggle_pause(&ctx);
				v.toggle_pause(&ctx);
				resumed = Some(v.start);
			}
		}
		assert!(v.loaded() && v.ended(), "loaded {} ended {} after {:?}, first {first:?}, resumed {resumed:?}, broken {}", v.loaded(), v.ended(), t.elapsed(), v.broken());
		// The first frame came while it was still downloading, and the end came at the end.
		assert!(first.is_some_and(|(_, loaded)| !loaded), "first frame at {first:?}");
		assert!(t.elapsed().as_secs_f64() < 5.0, "{:?}", t.elapsed());
		let from = resumed.expect("it was still downloading half a second in");
		let (n, _) = v.frame_after(0).expect("frames came");
		assert!(from >= 0.5 && from + n as f64 / 10.0 >= 2.8, "went on from {from}, for {n} frames");
	}

	/// A one-second test video made by ffmpeg plays here: it's read from memory,
	/// and frames of the right size come back. Skipped where ffmpeg isn't installed.
	#[test]
	fn plays_a_video_from_memory() {
		if !can_play_video() {
			return;
		}
		let made = Command::new("ffmpeg")
			.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=1"])
			.args(["-f", "webm", "-c:v", "libvpx", "pipe:1"])
			.stdin(Stdio::null())
			.stderr(Stdio::null())
			.output()
			.unwrap();
		if !made.status.success() {
			return;
		}
		let mut v = Video::stream(Box::new(Cursor::new(made.stdout))).unwrap();
		assert_eq!((v.width, v.height, v.has_audio), (320, 240, false));
		// A webm streamed out of ffmpeg has no length written in it; site videos do.
		assert!(v.duration == 0.0 || (v.duration - 1.0).abs() < 0.2, "{}", v.duration);
		v.play_from(0.0, &egui::Context::default());
		let t = std::time::Instant::now();
		while !v.ended() && t.elapsed().as_secs() < 10 {
			std::thread::sleep(std::time::Duration::from_millis(50));
		}
		let (n, frame) = v.frame_after(0).expect("frames came");
		assert!(n >= 5, "{n}");
		assert_eq!(frame.size, [320, 240]);
	}
}
