use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{Device, EventType, InputEventKind, Key};
use std::io::{BufRead, BufReader as StdBufReader};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// Configuration
const TOUCHPAD_PATH: &str = "/dev/input/by-path/platform-AMDI0010:03-event-mouse";
const KEYBOARD_PATH: &str = "/dev/input/by-path/platform-i8042-serio-0-event-kbd";
const DEBOUNCE_MS: u64 = 0;
const VIRTUAL_KEYBOARD_NAME: &str = "my-virtual-keyboard";

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("🚀 Starting touchpad-remap");
    eprintln!("📁 Touchpad: {}", TOUCHPAD_PATH);
    eprintln!("⌨️  Keyboard: {}", KEYBOARD_PATH);
    eprintln!("🖱️  Virtual keyboard: {}", VIRTUAL_KEYBOARD_NAME);
    eprintln!();

    let touchpad_active = Arc::new(AtomicBool::new(false));

    // Spawn libinput monitor in blocking thread
    let touchpad_active_clone = touchpad_active.clone();
    tokio::task::spawn_blocking(move || monitor_libinput(touchpad_active_clone));

    // Spawn touchpad release monitor in blocking thread
    let touchpad_active_clone = touchpad_active.clone();
    tokio::task::spawn_blocking(move || monitor_evdev_release(touchpad_active_clone));

    // Run keyboard monitor in blocking thread
    tokio::task::spawn_blocking(move || monitor_keyboard(touchpad_active)).await??;

    Ok(())
}

/// Monitor libinput for POINTER_MOTION
fn monitor_libinput(touchpad_active: Arc<AtomicBool>) -> Result<()> {
    eprintln!("📡 Starting libinput monitor...");

    let mut child = StdCommand::new("libinput")
        .arg("debug-events")
        .arg("--device")
        .arg(TOUCHPAD_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn libinput")?;

    let stdout = child.stdout.take().context("Failed to get stdout")?;
    let reader = StdBufReader::new(stdout);

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        if trimmed.contains("POINTER_MOTION") {
            if !touchpad_active.load(Ordering::Relaxed) {
                touchpad_active.store(true, Ordering::Relaxed);
                eprintln!("✓ POINTER_MOTION detected - mode ACTIVE");
            }
        }
    }

    Ok(())
}

/// Monitor evdev for BTN_TOOL_FINGER release
fn monitor_evdev_release(touchpad_active: Arc<AtomicBool>) -> Result<()> {
    eprintln!("👆 Starting touchpad release monitor...");

    let mut touchpad = Device::open(TOUCHPAD_PATH).context("Failed to open touchpad")?;

    loop {
        let events = touchpad.fetch_events().context("Failed to fetch events")?;

        for event in events {
            if let InputEventKind::Key(Key::BTN_TOOL_FINGER) = event.kind() {
                if event.value() == 0 {
                    if touchpad_active.load(Ordering::Relaxed) {
                        if DEBOUNCE_MS > 0 {
                            eprintln!("⏱  BTN_TOOL_FINGER released - waiting {}ms", DEBOUNCE_MS);
                            thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                        }

                        touchpad_active.store(false, Ordering::Relaxed);
                        eprintln!("✗ Mode DEACTIVATED");
                    }
                }
            }
        }
    }
}

/// Create a virtual keyboard with custom name
fn create_virtual_keyboard(keyboard: &Device, name: &str) -> Result<VirtualDevice> {
    let mut builder = VirtualDeviceBuilder::new()
        .context("Failed to create keyboard builder")?
        .name(name);

    if let Some(keys) = keyboard.supported_keys() {
        builder = builder.with_keys(keys)?;
    }

    if let Some(rel_axes) = keyboard.supported_relative_axes() {
        builder = builder.with_relative_axes(rel_axes)?;
    }

    if let Some(switches) = keyboard.supported_switches() {
        builder = builder.with_switches(switches)?;
    }

    builder.build().context("Failed to build virtual keyboard")
}

/// Handle keyboard input
fn monitor_keyboard(touchpad_active: Arc<AtomicBool>) -> Result<()> {
    eprintln!("⌨️  Opening keyboard device...");

    let mut keyboard = Device::open(KEYBOARD_PATH).context("Failed to open keyboard")?;

    eprintln!("🔒 Grabbing keyboard...");
    keyboard.grab().context("Failed to grab keyboard")?;

    eprintln!("🖱️  Creating virtual mouse...");
    let mut mouse_keys = evdev::AttributeSet::new();
    mouse_keys.insert(Key::BTN_LEFT);
    mouse_keys.insert(Key::BTN_RIGHT);
    mouse_keys.insert(Key::BTN_MIDDLE);

    let mut mouse_axes = evdev::AttributeSet::new();
    mouse_axes.insert(evdev::RelativeAxisType::REL_X);
    mouse_axes.insert(evdev::RelativeAxisType::REL_Y);
    mouse_axes.insert(evdev::RelativeAxisType::REL_WHEEL);

    let mut mouse = VirtualDeviceBuilder::new()
        .context("Failed to create mouse builder")?
        .name("rust-virtual-mouse")
        .with_keys(&mouse_keys)?
        .with_relative_axes(&mouse_axes)?
        .build()
        .context("Failed to build virtual mouse")?;

    eprintln!(
        "⌨️  Creating virtual keyboard '{}'...",
        VIRTUAL_KEYBOARD_NAME
    );
    let mut virtual_kbd = create_virtual_keyboard(&keyboard, VIRTUAL_KEYBOARD_NAME)?;

    eprintln!("✅ Ready! Monitoring keyboard events...");

    // Notify systemd that we're ready (virtual keyboard is created)
    let _ = systemd::daemon::notify(false, [(systemd::daemon::STATE_READY, "1")].iter());

    eprintln!();

    loop {
        let events = keyboard
            .fetch_events()
            .context("Failed to fetch keyboard events")?;

        for event in events {
            let mut handled = false;
            let is_active = touchpad_active.load(Ordering::Relaxed);

            if event.event_type() == EventType::KEY && is_active {
                match event.kind() {
                    InputEventKind::Key(Key::KEY_F) => {
                        if event.value() == 1 {
                            eprintln!("F → LEFT CLICK");
                        }
                        mouse.emit(&[evdev::InputEvent::new(
                            EventType::KEY,
                            Key::BTN_LEFT.code(),
                            event.value(),
                        )])?;
                        handled = true;
                    }
                    InputEventKind::Key(Key::KEY_D) => {
                        if event.value() == 1 {
                            eprintln!("D → RIGHT CLICK");
                        }
                        mouse.emit(&[evdev::InputEvent::new(
                            EventType::KEY,
                            Key::BTN_RIGHT.code(),
                            event.value(),
                        )])?;
                        handled = true;
                    }
                    _ => {}
                }
            }

            if !handled {
                virtual_kbd.emit(&[event])?;
            }
        }
    }
}
