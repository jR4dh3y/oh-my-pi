#[cfg(feature = "wayland-pipewire")]
mod capture;
mod libei;
mod portal;

use std::os::unix::fs::PermissionsExt;

use image::RgbaImage;

use crate::desktop::{
	backend::{AxBackend, Backend, DeliveryMode, PointerEvent},
	error::{CoreResult, DesktopError},
	frame::FrameGeometry,
	keys::KeyName,
	linux::ax::AtSpiAx,
	types::{
		CaptureCaps, DesktopCapabilities, DesktopDisplay, DesktopWindow, DisplaySelector, Target,
	},
};

pub struct WaylandBackend {
	display:     DisplaySelector,
	ax:          Option<AtSpiAx>,
	ax_error:    Option<DesktopError>,
	input:       Option<libei::Libei>,
	input_error: Option<DesktopError>,
	displays:    Vec<DesktopDisplay>,
}

impl WaylandBackend {
	pub fn new(display: DisplaySelector) -> Self {
		// Remove the world-readable RemoteDesktop restore token that pre-#7884
		// builds wrote during read-only calls; nothing reads it anymore (#7884).
		portal::remove_orphaned_remote_desktop_token();
		let (ax, ax_error) = match AtSpiAx::new() {
			Ok(ax) => (Some(ax), None),
			Err(err) => (None, Some(err)),
		};
		Self { display, ax, ax_error, input: None, input_error: None, displays: Vec::new() }
	}

	fn window_input_error(target: &Target, kind: &str) -> CoreResult<()> {
		if let Target::Window(id) = target {
			return Err(DesktopError::background_unavailable(format!(
				"window {id} wayland-compositor-focus-only: Wayland cannot programmatically activate \
				 a non-focused window for {kind}; only the currently focused surface is reachable; \
				 use ax actions or desktop input"
			)));
		}
		Ok(())
	}

	fn prepare_input(&mut self, target: &Target, kind: &str) -> CoreResult<&mut libei::Libei> {
		Self::window_input_error(target, kind)?;
		if self.input.is_none() && self.input_error.is_none() {
			match libei::Libei::new() {
				Ok(input) => self.input = Some(input),
				Err(err) => self.input_error = Some(err),
			}
		}
		if let Some(input) = self.input.as_mut() {
			return Ok(input);
		}
		Err(self.input_error.clone().unwrap_or_else(|| {
			DesktopError::permission_denied(
				"RemoteDesktop portal or LIBEI_SOCKET is required for Wayland input",
			)
		}))
	}

	fn synthetic_display(image: &RgbaImage) -> DesktopDisplay {
		DesktopDisplay {
			id:           "wayland-portal-0".to_string(),
			name:         "Wayland portal monitor".to_string(),
			x:            0,
			y:            0,
			width:        image.width(),
			height:       image.height(),
			scale:        1.0,
			pixel_x:      0,
			pixel_y:      0,
			pixel_width:  image.width(),
			pixel_height: image.height(),
			is_primary:   true,
		}
	}

	fn selected_display_allowed(&self) -> CoreResult<()> {
		match &self.display {
			DisplaySelector::All => Ok(()),
			DisplaySelector::Id(id) if id == "wayland-portal-0" || id == "wayland-display-0" => Ok(()),
			DisplaySelector::Id(id) => Err(DesktopError::invalid_target(format!(
				"Wayland display '{id}' is unavailable; use 'all'"
			))),
		}
	}

	fn has_tool(name: &str) -> bool {
		// In-process PATH lookup: no `which` subprocess, no dependency on it.
		std::env::var_os("PATH").is_some_and(|paths| {
			std::env::split_paths(&paths).any(|dir| {
				std::fs::metadata(dir.join(name))
					.is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
			})
		})
	}

	fn has_grim() -> bool {
		Self::has_tool("grim")
	}

	fn has_wtype() -> bool {
		Self::has_tool("wtype")
	}

	fn capture_grim() -> CoreResult<RgbaImage> {
		let output = std::process::Command::new("grim")
			.args(["-l", "0", "-t", "png", "-"])
			.output()
			.map_err(|err| DesktopError::capture_failed(format!("failed to run grim: {err}")))?;
		if !output.status.success() {
			let stderr = String::from_utf8_lossy(&output.stderr);
			return Err(DesktopError::capture_failed(format!("grim capture failed: {stderr}")));
		}
		let img = image::load_from_memory(&output.stdout).map_err(|err| {
			DesktopError::capture_failed(format!("failed to decode grim PNG: {err}"))
		})?;
		Ok(img.to_rgba8())
	}

	fn niri_windows() -> Option<Vec<DesktopWindow>> {
		let output = std::process::Command::new("niri")
			.args(["msg", "--json", "windows"])
			.output()
			.ok()?;
		if !output.status.success() {
			return None;
		}
		let val: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
		let arr = val.as_array()?;
		let mut wins = Vec::new();
		for item in arr {
			let Some(id) = item.get("id").map(ToString::to_string) else {
				continue;
			};
			let title = item
				.get("title")
				.and_then(|v| v.as_str())
				.unwrap_or("")
				.to_string();
			let app = item
				.get("app_id")
				.and_then(|v| v.as_str())
				.unwrap_or("")
				.to_string();
			let pid = item.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
			let focused = item
				.get("is_focused")
				.and_then(|v| v.as_bool())
				.unwrap_or(false);
			let (width, height) = if let Some(ws) = item
				.get("layout")
				.and_then(|l| l.get("window_size"))
				.and_then(|v| v.as_array())
			{
				(
					ws.first().and_then(|v| v.as_u64()).unwrap_or(0) as u32,
					ws.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
				)
			} else {
				(0, 0)
			};
			wins.push(DesktopWindow { id, title, app, pid, x: 0, y: 0, width, height, focused });
		}
		Some(wins)
	}

	#[cfg(feature = "wayland-pipewire")]
	fn capture_pipewire_image(&self) -> CoreResult<RgbaImage> {
		capture::capture()
	}

	#[cfg(not(feature = "wayland-pipewire"))]
	fn capture_pipewire_image(&self) -> CoreResult<RgbaImage> {
		Err(DesktopError::capture_failed(
			"Wayland capture requires the wayland-pipewire feature or the grim tool",
		))
	}
}

impl Backend for WaylandBackend {
	fn capabilities(&mut self) -> DesktopCapabilities {
		let has_grim = Self::has_grim();
		let can_capture = cfg!(feature = "wayland-pipewire") || has_grim;
		let input_permission = if self.input.is_some() {
			"granted"
		} else if self.input_error.is_some() {
			if Self::has_wtype() {
				"granted"
			} else {
				"unavailable"
			}
		} else {
			"prompt-or-granted"
		};
		DesktopCapabilities {
			backend: "wayland".to_string(),
			display_server: Some("wayland".to_string()),
			capture: can_capture,
			input: self.input_error.is_none() || Self::has_wtype(),
			ax: self.ax.is_some(),
			background_window_input: false,
			delivery_modes: vec!["background".to_string()],
			capture_permission: if can_capture {
				"prompt-or-granted".to_string()
			} else {
				"unavailable".to_string()
			},
			input_permission: input_permission.to_string(),
			ax_permission: if self.ax.is_some() {
				"granted".to_string()
			} else {
				"unavailable".to_string()
			},
			display_count: self.displays.len() as u32,
		}
	}

	fn displays(&mut self) -> CoreResult<Vec<DesktopDisplay>> {
		Ok(self.displays.clone())
	}

	fn windows(&mut self) -> CoreResult<Vec<DesktopWindow>> {
		let mut wins = self
			.ax
			.as_mut()
			.and_then(|ax| ax.windows().ok())
			.unwrap_or_default();
		if let Some(niri_wins) = Self::niri_windows() {
			for nw in niri_wins {
				let duplicate = wins.iter().any(|w| w.pid == nw.pid && w.title == nw.title);
				if !duplicate {
					wins.push(nw);
				}
			}
		}
		if wins.is_empty() {
			if let Some(ax) = self.ax.as_mut() {
				return ax.windows();
			}
			return Err(
				self
					.ax_error
					.clone()
					.unwrap_or_else(DesktopError::ax_unsupported),
			);
		}
		Ok(wins)
	}

	fn capture(
		&mut self,
		target: &Target,
		_caps: &CaptureCaps,
	) -> CoreResult<(RgbaImage, FrameGeometry)> {
		self.selected_display_allowed()?;

		let image = if Self::has_grim() {
			// grim serves the compositor-native screencopy protocol directly: no
			// portal dialog, no PipeWire round-trip. Fall back to the portal path
			// only when grim itself fails (e.g. compositors without wlr-screencopy).
			match Self::capture_grim() {
				Ok(image) => image,
				Err(grim_err) => self.capture_pipewire_image().map_err(|_| grim_err)?,
			}
		} else {
			self.capture_pipewire_image()?
		};
		let display = Self::synthetic_display(&image);
		self.displays = vec![display.clone()];
		match target {
			Target::Desktop => {
				let geometry = FrameGeometry::for_displays(&self.displays);
				Ok((image, geometry))
			},
			Target::Window(id) => {
				let window = self
					.windows()?
					.into_iter()
					.find(|window| &window.id == id)
					.ok_or_else(|| {
						DesktopError::window_not_found(format!("Wayland window {id} not found"))
					})?;
				if window.x < 0 || window.y < 0 {
					return Err(DesktopError::capture_failed(
						"Wayland portal monitor stream cannot crop a window outside the selected monitor",
					));
				}
				let x = window.x as u32;
				let y = window.y as u32;
				let width = window.width.min(image.width().saturating_sub(x));
				let height = window.height.min(image.height().saturating_sub(y));
				if width == 0 || height == 0 {
					return Err(DesktopError::capture_failed(format!(
						"Wayland window {id} is outside the selected portal monitor"
					)));
				}
				let cropped = image::imageops::crop_imm(&image, x, y, width, height).to_image();
				let geometry = FrameGeometry::for_window(&window, cropped.width(), cropped.height());
				Ok((cropped, geometry))
			},
		}
	}

	fn pointer(
		&mut self,
		target: &Target,
		ev: PointerEvent,
		_frame: &FrameGeometry,
		_mode: DeliveryMode,
	) -> CoreResult<()> {
		self.prepare_input(target, "pointer input")?.pointer(ev)
	}

	fn type_text(&mut self, target: &Target, text: &str, _mode: DeliveryMode) -> CoreResult<()> {
		Self::window_input_error(target, "keyboard input")?;
		match self.prepare_input(target, "keyboard input") {
			Ok(input) => input.type_text(text),
			Err(err) => {
				if matches!(target, Target::Desktop) && Self::has_wtype() {
					let status = std::process::Command::new("wtype")
						.args(["--", text])
						.status()
						.map_err(|e| DesktopError::permission_denied(format!("wtype failed: {e}")))?;
					if status.success() {
						return Ok(());
					}
				}
				Err(err)
			},
		}
	}

	fn key_chord(
		&mut self,
		target: &Target,
		keys: &[KeyName],
		_mode: DeliveryMode,
	) -> CoreResult<()> {
		Self::window_input_error(target, "keyboard input")?;
		match self.prepare_input(target, "keyboard input") {
			Ok(input) => input.key_chord(keys),
			Err(err) => {
				if matches!(target, Target::Desktop) && Self::has_wtype() {
					let mut cmd = std::process::Command::new("wtype");
					for key in keys {
						match key {
							KeyName::Ctrl => {
								cmd.arg("-M").arg("ctrl");
							},
							KeyName::Alt => {
								cmd.arg("-M").arg("alt");
							},
							KeyName::Shift => {
								cmd.arg("-M").arg("shift");
							},
							KeyName::Meta => {
								cmd.arg("-M").arg("logo");
							},
							KeyName::Enter => {
								cmd.arg("-k").arg("Return");
							},
							KeyName::Escape => {
								cmd.arg("-k").arg("Escape");
							},
							KeyName::Tab => {
								cmd.arg("-k").arg("Tab");
							},
							KeyName::Space => {
								cmd.arg("-k").arg("space");
							},
							KeyName::Backspace => {
								cmd.arg("-k").arg("BackSpace");
							},
							KeyName::Delete => {
								cmd.arg("-k").arg("Delete");
							},
							KeyName::Home => {
								cmd.arg("-k").arg("Home");
							},
							KeyName::End => {
								cmd.arg("-k").arg("End");
							},
							KeyName::PageUp => {
								cmd.arg("-k").arg("Page_Up");
							},
							KeyName::PageDown => {
								cmd.arg("-k").arg("Page_Down");
							},
							KeyName::Up => {
								cmd.arg("-k").arg("Up");
							},
							KeyName::Down => {
								cmd.arg("-k").arg("Down");
							},
							KeyName::Left => {
								cmd.arg("-k").arg("Left");
							},
							KeyName::Right => {
								cmd.arg("-k").arg("Right");
							},
							KeyName::F1 => {
								cmd.arg("-k").arg("F1");
							},
							KeyName::F2 => {
								cmd.arg("-k").arg("F2");
							},
							KeyName::F3 => {
								cmd.arg("-k").arg("F3");
							},
							KeyName::F4 => {
								cmd.arg("-k").arg("F4");
							},
							KeyName::F5 => {
								cmd.arg("-k").arg("F5");
							},
							KeyName::F6 => {
								cmd.arg("-k").arg("F6");
							},
							KeyName::F7 => {
								cmd.arg("-k").arg("F7");
							},
							KeyName::F8 => {
								cmd.arg("-k").arg("F8");
							},
							KeyName::F9 => {
								cmd.arg("-k").arg("F9");
							},
							KeyName::F10 => {
								cmd.arg("-k").arg("F10");
							},
							KeyName::F11 => {
								cmd.arg("-k").arg("F11");
							},
							KeyName::F12 => {
								cmd.arg("-k").arg("F12");
							},
							KeyName::Char(c) => {
								cmd.arg(c.to_string());
							},
							key => {
								return Err(DesktopError::invalid_key(format!(
									"wtype fallback has no mapping for key `{key:?}`"
								)));
							},
						}
					}
					let status = cmd
						.status()
						.map_err(|e| DesktopError::permission_denied(format!("wtype failed: {e}")))?;
					if status.success() {
						return Ok(());
					}
				}
				Err(err)
			},
		}
	}

	fn raise_window(&mut self, id: &str) -> CoreResult<()> {
		if let Ok(num_id) = id.parse::<u64>() {
			if let Ok(status) = std::process::Command::new("niri")
				.args(["msg", "action", "focus-window", "--id", &num_id.to_string()])
				.status()
			{
				if status.success() {
					return Ok(());
				}
			}
		}
		Err(DesktopError::background_unavailable(format!(
			"window {id} wayland-compositor-focus-only: Wayland cannot programmatically activate a \
			 non-focused window; only the currently focused surface is reachable"
		)))
	}

	fn ax(&mut self) -> Option<&mut dyn AxBackend> {
		self.ax.as_mut().map(|ax| ax as &mut dyn AxBackend)
	}
}

#[cfg(test)]
mod tests {
	use std::{
		io::ErrorKind,
		os::unix::net::UnixListener,
		sync::{Mutex, mpsc},
		thread,
	};

	use super::*;

	static LIBEI_ENV_LOCK: Mutex<()> = Mutex::new(());

	fn backend_without_services() -> WaylandBackend {
		WaylandBackend {
			display:     DisplaySelector::All,
			ax:          None,
			ax_error:    None,
			input:       None,
			input_error: None,
			displays:    Vec::new(),
		}
	}
	fn with_fake_libei(action: impl FnOnce(&mut WaylandBackend)) -> bool {
		let _guard = LIBEI_ENV_LOCK.lock().expect("lock LIBEI_SOCKET test");
		let socket = std::env::temp_dir().join(format!("omp-libei-test-{}", std::process::id()));
		let _ = std::fs::remove_file(&socket);
		let listener = UnixListener::bind(&socket).expect("bind fake libei socket");
		listener
			.set_nonblocking(true)
			.expect("make fake libei socket nonblocking");
		let (stop_tx, stop_rx) = mpsc::channel();
		let accepted = thread::spawn(move || {
			loop {
				match listener.accept() {
					Ok(_) => return true,
					Err(err) if err.kind() == ErrorKind::WouldBlock => {
						if !matches!(
							stop_rx.recv_timeout(std::time::Duration::from_millis(10)),
							Err(mpsc::RecvTimeoutError::Timeout)
						) {
							return false;
						}
					},
					Err(err) => panic!("fake libei listener: {err}"),
				}
			}
		});
		let previous = std::env::var_os("LIBEI_SOCKET");
		unsafe { std::env::set_var("LIBEI_SOCKET", &socket) };
		let mut backend = WaylandBackend::new(DisplaySelector::All);
		action(&mut backend);
		let _ = stop_tx.send(());
		if let Some(previous) = previous {
			unsafe { std::env::set_var("LIBEI_SOCKET", previous) };
		} else {
			unsafe { std::env::remove_var("LIBEI_SOCKET") };
		}
		let connected = accepted.join().expect("fake libei listener");
		let _ = std::fs::remove_file(socket);
		connected
	}

	#[test]
	fn readonly_backend_creation_does_not_connect_to_libei() {
		let mut capabilities = None;
		let connected = with_fake_libei(|backend| capabilities = Some(backend.capabilities()));
		assert!(!connected, "read-only backend construction connected to libei");
		let capabilities = capabilities.expect("Wayland capabilities");
		assert!(capabilities.input);
		assert_eq!(capabilities.input_permission, "prompt-or-granted");
	}

	#[test]
	fn desktop_input_connects_to_libei_lazily() {
		let connected = with_fake_libei(|backend| {
			let _ = backend.type_text(&Target::Desktop, "hello", DeliveryMode::Foreground);
		});
		assert!(connected, "desktop input did not connect to libei");
	}

	#[test]
	fn window_foreground_delivery_reports_compositor_constraint() {
		let mut backend = backend_without_services();
		let target = Target::Window("w1".to_string());
		let err = backend
			.type_text(&target, "hello", DeliveryMode::Foreground)
			.expect_err("window foreground input must fail");
		assert_eq!(err.code.as_str(), "BackgroundUnavailable");
		assert_eq!(
			err.message,
			"window w1 wayland-compositor-focus-only: Wayland cannot programmatically activate a \
			 non-focused window for keyboard input; only the currently focused surface is reachable; \
			 use ax actions or desktop input"
		);
	}

	#[test]
	fn window_raise_reports_compositor_constraint() {
		let mut backend = backend_without_services();
		let err = backend
			.raise_window("w1")
			.expect_err("Wayland window raise must fail");
		assert_eq!(err.code.as_str(), "BackgroundUnavailable");
		assert_eq!(
			err.message,
			"window w1 wayland-compositor-focus-only: Wayland cannot programmatically activate a \
			 non-focused window; only the currently focused surface is reachable"
		);
	}

	#[test]
	fn capabilities_do_not_advertise_foreground_delivery() {
		let mut backend = backend_without_services();
		assert_eq!(backend.capabilities().delivery_modes, ["background"]);
	}

	#[test]
	#[cfg(not(feature = "wayland-pipewire"))]
	fn capabilities_report_no_capture_without_pipewire_feature() {
		let mut backend = WaylandBackend {
			display:     DisplaySelector::All,
			ax:          None,
			ax_error:    None,
			input:       None,
			input_error: None,
			displays:    Vec::new(),
		};
		let caps = backend.capabilities();
		// Without the pipewire feature, capture is available only through the
		// grim fallback, so capabilities() must agree with capture() in both cases.
		assert_eq!(caps.capture, WaylandBackend::has_grim());
		if WaylandBackend::has_grim() {
			assert_eq!(caps.capture_permission, "prompt-or-granted");
		} else {
			assert_eq!(caps.capture_permission, "unavailable");
			let err = backend
				.capture(&Target::Desktop, &CaptureCaps::default())
				.expect_err("capture must fail without the pipewire feature or grim");
			assert_eq!(err.code.as_str(), "CaptureFailed");
		}
	}
}
