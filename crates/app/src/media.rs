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
use std::sync::{Arc, Mutex};

use eframe::egui::{self, ColorImage};
use image::AnimationDecoder;

/// Frames are scaled down to this width at most.
const MAX_WIDTH: u32 = 1280;
/// Animations stop adding frames past this much memory.
const MAX_ANIMATION_BYTES: usize = 400 * 1024 * 1024;

/// An animation's frames and how long each shows, in milliseconds.
pub type Frames = Vec<(ColorImage, u32)>;

/// All frames of an animated GIF or WebP. A still image gives one frame.
pub fn decode_animation(bytes: &[u8]) -> Result<Frames, String> {
	let frames: Vec<image::Frame> = match image::guess_format(bytes).map_err(|e| e.to_string())? {
		image::ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
			.and_then(|d| d.into_frames().collect_frames())
			.map_err(|e| e.to_string())?,
		image::ImageFormat::WebP => {
			let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
			if d.has_animation() {
				d.into_frames().collect_frames().map_err(|e| e.to_string())?
			} else {
				let still = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
				vec![image::Frame::new(still.to_rgba8())]
			}
		}
		_ => vec![image::Frame::new(image::load_from_memory(bytes).map_err(|e| e.to_string())?.to_rgba8())],
	};
	let mut out = Vec::new();
	let mut used = 0;
	for f in frames {
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
	pub paused: bool,
	child: Option<Child>,
	/// Where the current run started, and frames shown since.
	start: f64,
	frames: Arc<AtomicU64>,
	/// The newest frame and its number, for the window to show.
	latest: Arc<Mutex<Option<(u64, ColorImage)>>>,
	ended: Arc<AtomicBool>,
	/// Width and height frames are scaled to.
	out: (u32, u32),
}

impl Video {
	/// Puts the video in a RAM-only file and reads its size, length and streams.
	pub fn open(bytes: &[u8]) -> Result<Video, String> {
		// Close-on-exec: only the ffmpeg runs below get it (see `hand_over`).
		let name = c"bettersearch-video";
		let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
		if fd < 0 {
			return Err(format!("couldn't make a memory file: {}", std::io::Error::last_os_error()));
		}
		let file = unsafe { OwnedFd::from_raw_fd(fd) };
		std::fs::File::from(file.try_clone().map_err(|e| e.to_string())?).write_all(bytes).map_err(|e| e.to_string())?;
		let mut v = Video {
			file,
			width: 0,
			height: 0,
			duration: 0.0,
			fps: 30.0,
			has_audio: false,
			muted: true,
			paused: false,
			child: None,
			start: 0.0,
			frames: Arc::new(AtomicU64::new(0)),
			latest: Arc::new(Mutex::new(None)),
			ended: Arc::new(AtomicBool::new(false)),
			out: (0, 0),
		};
		v.probe()?;
		Ok(v)
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
		self.stop();
		self.paused = false;
		self.start = at.max(0.0);
		self.frames.store(0, Ordering::SeqCst);
		self.ended.store(false, Ordering::SeqCst);
		let (w, h) = self.out;
		let mut cmd = self.command("ffmpeg");
		cmd.args(["-nostdin", "-ss", &format!("{:.3}", self.start), "-re", "-i"]).arg(self.path());
		cmd.args(["-map", "0:v:0", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
		if self.has_audio && !self.muted {
			cmd.args(["-map", "0:a:0"]);
			cmd.args(sound_output());
		}
		let Ok(mut child) = cmd.stdout(Stdio::piped()).spawn() else {
			self.ended.store(true, Ordering::SeqCst);
			return;
		};
		let mut out = child.stdout.take().expect("piped");
		let (latest, frames, ended, ctx) = (self.latest.clone(), self.frames.clone(), self.ended.clone(), ctx.clone());
		std::thread::spawn(move || {
			let mut buf = vec![0u8; w as usize * h as usize * 4];
			while out.read_exact(&mut buf).is_ok() {
				let n = frames.fetch_add(1, Ordering::SeqCst) + 1;
				*latest.lock().unwrap() = Some((n, ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &buf)));
				ctx.request_repaint();
			}
			ended.store(true, Ordering::SeqCst);
			ctx.request_repaint();
		});
		self.child = Some(child);
	}

	/// Seconds into the video.
	pub fn position(&self) -> f64 {
		let p = self.start + self.frames.load(Ordering::SeqCst) as f64 / self.fps;
		if self.duration > 0.0 { p.min(self.duration) } else { p }
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

	fn stop(&mut self) {
		if let Some(mut c) = self.child.take() {
			let _ = c.kill();
			let _ = c.wait();
		}
	}
}

impl Drop for Video {
	fn drop(&mut self) {
		self.stop();
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
		let mut v = Video::open(&made.stdout).unwrap();
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
