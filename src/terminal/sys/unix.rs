//! UNIX related logic for terminal manipulation.

#[cfg(feature = "events")]
use crate::event::{KeyboardEnhancementFlags, TerminalFeatures, ThemeMode};
use crate::terminal::{
    sys::file_descriptor::{tty_fd, FileDesc},
    WindowSize,
};
#[cfg(feature = "libc")]
use libc::{
    cfmakeraw, ioctl, tcgetattr, tcsetattr, termios as Termios, winsize, STDOUT_FILENO, TCSANOW,
    TIOCGWINSZ,
};
use parking_lot::Mutex;
#[cfg(not(feature = "libc"))]
use rustix::{
    fd::AsFd,
    termios::{Termios, Winsize},
};

use std::{fs::File, io, process};
#[cfg(feature = "libc")]
use std::{
    mem,
    os::unix::io::{IntoRawFd, RawFd},
};

// Some(Termios) -> we're in the raw mode and this is the previous mode
// None -> we're not in the raw mode
static TERMINAL_MODE_PRIOR_RAW_MODE: Mutex<Option<Termios>> = parking_lot::const_mutex(None);

pub(crate) fn is_raw_mode_enabled() -> bool {
    TERMINAL_MODE_PRIOR_RAW_MODE.lock().is_some()
}

#[cfg(feature = "libc")]
impl From<winsize> for WindowSize {
    fn from(size: winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}
#[cfg(not(feature = "libc"))]
impl From<Winsize> for WindowSize {
    fn from(size: Winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}

#[allow(clippy::useless_conversion)]
#[cfg(feature = "libc")]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    // http://rosettacode.org/wiki/Terminal_control/Dimensions#Library:_BSD_libc
    let mut size = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let file = File::open("/dev/tty").map(|file| (FileDesc::new(file.into_raw_fd(), true)));
    let fd = if let Ok(file) = &file {
        file.raw_fd()
    } else {
        // Fallback to libc::STDOUT_FILENO if /dev/tty is missing
        STDOUT_FILENO
    };

    if wrap_with_result(unsafe { ioctl(fd, TIOCGWINSZ.into(), &mut size) }).is_ok() {
        return Ok(size.into());
    }

    Err(std::io::Error::last_os_error().into())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    let file = File::open("/dev/tty").map(|file| (FileDesc::Owned(file.into())));
    let fd = if let Ok(file) = &file {
        file.as_fd()
    } else {
        // Fallback to libc::STDOUT_FILENO if /dev/tty is missing
        rustix::stdio::stdout()
    };
    let size = rustix::termios::tcgetwinsize(fd)?;
    Ok(size.into())
}

#[allow(clippy::useless_conversion)]
pub(crate) fn size() -> io::Result<(u16, u16)> {
    if let Ok(window_size) = window_size() {
        return Ok((window_size.columns, window_size.rows));
    }

    tput_size().ok_or_else(|| std::io::Error::last_os_error().into())
}

#[cfg(feature = "libc")]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if original_mode.is_some() {
        return Ok(());
    }

    let tty = tty_fd()?;
    let fd = tty.raw_fd();
    let mut ios = get_terminal_attr(fd)?;
    let original_mode_ios = ios;
    raw_terminal_attr(&mut ios);
    set_terminal_attr(fd, &ios)?;
    // Keep it last - set the original mode only if we were able to switch to the raw mode
    *original_mode = Some(original_mode_ios);
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if original_mode.is_some() {
        return Ok(());
    }

    let tty = tty_fd()?;
    let mut ios = get_terminal_attr(&tty)?;
    let original_mode_ios = ios.clone();
    ios.make_raw();
    set_terminal_attr(&tty, &ios)?;
    // Keep it last - set the original mode only if we were able to switch to the raw mode
    *original_mode = Some(original_mode_ios);
    Ok(())
}

/// Reset the raw mode.
///
/// More precisely, reset the whole termios mode to what it was before the first call
/// to [enable_raw_mode]. If you don't mess with termios outside of crossterm, it's
/// effectively disabling the raw mode and doing nothing else.
#[cfg(feature = "libc")]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if let Some(original_mode_ios) = original_mode.as_ref() {
        let tty = tty_fd()?;
        set_terminal_attr(tty.raw_fd(), original_mode_ios)?;
        // Keep it last - remove the original mode only if we were able to switch back
        *original_mode = None;
    }
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if let Some(original_mode_ios) = original_mode.as_ref() {
        let tty = tty_fd()?;
        set_terminal_attr(&tty, original_mode_ios)?;
        // Keep it last - remove the original mode only if we were able to switch back
        *original_mode = None;
    }
    Ok(())
}

#[cfg(not(feature = "libc"))]
fn get_terminal_attr(fd: impl AsFd) -> io::Result<Termios> {
    let result = rustix::termios::tcgetattr(fd)?;
    Ok(result)
}

#[cfg(not(feature = "libc"))]
fn set_terminal_attr(fd: impl AsFd, termios: &Termios) -> io::Result<()> {
    rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, termios)?;
    Ok(())
}

/// Queries the currently selected theme mode (dark/light) from the terminal.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn query_terminal_theme_mode() -> io::Result<Option<ThemeMode>> {
    if is_raw_mode_enabled() {
        query_terminal_theme_mode_raw()
    } else {
        query_terminal_theme_mode_nonraw()
    }
}

#[cfg(feature = "events")]
fn query_terminal_theme_mode_nonraw() -> io::Result<Option<ThemeMode>> {
    enable_raw_mode()?;
    let theme_mode = query_terminal_theme_mode_raw();
    disable_raw_mode()?;
    theme_mode
}

#[cfg(feature = "events")]
fn query_terminal_theme_mode_raw() -> io::Result<Option<ThemeMode>> {
    use crate::event::{
        filter::{PrimaryDeviceAttributesFilter, ThemeModeFilter},
        poll_internal, read_internal, Event, InternalEvent,
    };
    use std::io::Write;
    use std::time::Duration;

    // ESC [ ? 996 n         Query current terminal theme mode
    // ESC [ c               Query primary device attributes (widely supported)
    const QUERY: &[u8] = b"\x1B[?996n\x1B[c";

    let result = File::open("/dev/tty").and_then(|mut file| {
        file.write_all(QUERY)?;
        file.flush()
    });
    if result.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
    }

    loop {
        match poll_internal(Some(Duration::from_millis(2000)), &ThemeModeFilter) {
            Ok(true) => match read_internal(&ThemeModeFilter) {
                Ok(InternalEvent::Event(Event::ThemeModeChanged(theme_mode))) => {
                    // Flush the PrimaryDeviceAttributes out of the event queue.
                    read_internal(&PrimaryDeviceAttributesFilter).ok();
                    return Ok(Some(theme_mode));
                }
                _ => return Ok(None),
            },
            Ok(false) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "The theme mode could not be read in a normal duration",
                ));
            }
            Err(_) => {}
        }
    }
}

#[cfg(feature = "events")]
fn supports_synchronized_output_raw() -> io::Result<bool> {
    use crate::event::{
        filter::{PrimaryDeviceAttributesFilter, SynchronizedOutputModeFilter},
        poll_internal, read_internal, InternalEvent, SynchronizedOutputMode,
    };
    use std::io::Write;
    use std::time::Duration;

    // ESC [ ? 2026 $ p      DECRQM request for synchronized output state
    // ESC [ c               Query primary device attributes (widely supported)
    const QUERY: &[u8] = b"\x1B[?2026$p\x1B[c";

    let result = File::open("/dev/tty").and_then(|mut file| {
        file.write_all(QUERY)?;
        file.flush()
    });
    if result.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
    }

    loop {
        match poll_internal(
            Some(Duration::from_millis(2000)),
            &SynchronizedOutputModeFilter,
        ) {
            Ok(true) => match read_internal(&SynchronizedOutputModeFilter) {
                Ok(InternalEvent::SynchronizedOutputMode(
                    SynchronizedOutputMode::Set | SynchronizedOutputMode::Reset,
                )) => {
                    // Flush the PrimaryDeviceAttributes out of the event queue.
                    read_internal(&PrimaryDeviceAttributesFilter).ok();
                    return Ok(true);
                }
                _ => return Ok(false),
            },
            Ok(false) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Synchronized output mode could not be read in a normal duration",
                ));
            }
            Err(_) => {}
        }
    }
}

#[cfg(feature = "events")]
fn supports_synchronized_output_nonraw() -> io::Result<bool> {
    enable_raw_mode()?;
    let is_supported = supports_synchronized_output_raw();
    disable_raw_mode()?;
    is_supported
}

/// Queries the terminal's support for synchronized output sequences.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn supports_synchronized_output() -> io::Result<bool> {
    if is_raw_mode_enabled() {
        supports_synchronized_output_raw()
    } else {
        supports_synchronized_output_nonraw()
    }
}

/// Queries the terminal's support for progressive keyboard enhancement.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    query_keyboard_enhancement_flags().map(|flags| flags.is_some())
}

/// Queries the terminal's currently active keyboard enhancement flags.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn query_keyboard_enhancement_flags() -> io::Result<Option<KeyboardEnhancementFlags>> {
    if is_raw_mode_enabled() {
        query_keyboard_enhancement_flags_raw()
    } else {
        query_keyboard_enhancement_flags_nonraw()
    }
}

#[cfg(feature = "events")]
fn query_keyboard_enhancement_flags_nonraw() -> io::Result<Option<KeyboardEnhancementFlags>> {
    enable_raw_mode()?;
    let flags = query_keyboard_enhancement_flags_raw();
    disable_raw_mode()?;
    flags
}

#[cfg(feature = "events")]
fn query_keyboard_enhancement_flags_raw() -> io::Result<Option<KeyboardEnhancementFlags>> {
    use crate::event::{
        filter::{KeyboardEnhancementFlagsFilter, PrimaryDeviceAttributesFilter},
        poll_internal, read_internal, InternalEvent,
    };
    use std::io::Write;
    use std::time::Duration;

    // This is the recommended method for testing support for the keyboard enhancement protocol.
    // We send a query for the flags supported by the terminal and then the primary device attributes
    // query. If we receive the primary device attributes response but not the keyboard enhancement
    // flags, none of the flags are supported.
    //
    // See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>

    // ESC [ ? u        Query progressive keyboard enhancement flags (kitty protocol).
    // ESC [ c          Query primary device attributes.
    const QUERY: &[u8] = b"\x1B[?u\x1B[c";

    let result = File::open("/dev/tty").and_then(|mut file| {
        file.write_all(QUERY)?;
        file.flush()
    });
    if result.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
    }

    loop {
        match poll_internal(
            Some(Duration::from_millis(2000)),
            &KeyboardEnhancementFlagsFilter,
        ) {
            Ok(true) => {
                match read_internal(&KeyboardEnhancementFlagsFilter) {
                    Ok(InternalEvent::KeyboardEnhancementFlags(current_flags)) => {
                        // Flush the PrimaryDeviceAttributes out of the event queue.
                        read_internal(&PrimaryDeviceAttributesFilter).ok();
                        return Ok(Some(current_flags));
                    }
                    _ => return Ok(None),
                }
            }
            Ok(false) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "The keyboard enhancement status could not be read within a normal duration",
                ));
            }
            Err(_) => {}
        }
    }
}

/// Queries information about features that the terminal supports.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn terminal_features() -> io::Result<TerminalFeatures> {
    if is_raw_mode_enabled() {
        terminal_features_raw()
    } else {
        terminal_features_nonraw()
    }
}

#[cfg(feature = "events")]
fn terminal_features_nonraw() -> io::Result<TerminalFeatures> {
    enable_raw_mode()?;
    let features = terminal_features_raw();
    disable_raw_mode()?;
    features
}

#[cfg(feature = "events")]
fn terminal_features_raw() -> io::Result<TerminalFeatures> {
    use crate::event::{
        filter::TerminalFeaturesFilter, poll_internal, read_internal, Event, InternalEvent,
    };
    use std::io::Write;
    use std::time::Duration;

    // ESC [ ? u             Query progressive keyboard enhancement flags (kitty protocol).
    // ESC [ ? 2026 $ p      DECRQM request for synchronized output state
    // ESC [ ? 996 n         Query current terminal theme mode
    // ESC [ c               Query primary device attributes.
    const QUERY: &[u8] = b"\x1B[?u\x1B[?2026$p\x1B[?996n\x1B[c";

    let result = File::open("/dev/tty").and_then(|mut file| {
        file.write_all(QUERY)?;
        file.flush()
    });
    if result.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
    }

    let mut features = TerminalFeatures::default();
    loop {
        match poll_internal(Some(Duration::from_millis(2000)), &TerminalFeaturesFilter) {
            Ok(true) => match read_internal(&TerminalFeaturesFilter) {
                Ok(InternalEvent::KeyboardEnhancementFlags(flags)) => {
                    features.keyboard_enhancement_flags = Some(flags);
                }
                Ok(InternalEvent::SynchronizedOutputMode(mode)) => {
                    features.synchronized_output_mode = mode;
                }
                Ok(InternalEvent::Event(Event::ThemeModeChanged(theme_mode))) => {
                    features.theme_mode = Some(theme_mode);
                }
                _ => return Ok(features),
            },
            Ok(false) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Terminal information could not be read within a normal duration",
                ));
            }
            Err(_) => {}
        }
    }
}

/// execute tput with the given argument and parse
/// the output as a u16.
///
/// The arg should be "cols" or "lines"
fn tput_value(arg: &str) -> Option<u16> {
    let output = process::Command::new("tput").arg(arg).output().ok()?;
    let value = output
        .stdout
        .into_iter()
        .filter_map(|b| char::from(b).to_digit(10))
        .fold(0, |v, n| v * 10 + n as u16);

    if value > 0 {
        Some(value)
    } else {
        None
    }
}

/// Returns the size of the screen as determined by tput.
///
/// This alternate way of computing the size is useful
/// when in a subshell.
fn tput_size() -> Option<(u16, u16)> {
    match (tput_value("cols"), tput_value("lines")) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

#[cfg(feature = "libc")]
// Transform the given mode into an raw mode (non-canonical) mode.
fn raw_terminal_attr(termios: &mut Termios) {
    unsafe { cfmakeraw(termios) }
}

#[cfg(feature = "libc")]
fn get_terminal_attr(fd: RawFd) -> io::Result<Termios> {
    unsafe {
        let mut termios = mem::zeroed();
        wrap_with_result(tcgetattr(fd, &mut termios))?;
        Ok(termios)
    }
}

#[cfg(feature = "libc")]
fn set_terminal_attr(fd: RawFd, termios: &Termios) -> io::Result<()> {
    wrap_with_result(unsafe { tcsetattr(fd, TCSANOW, termios) })
}

#[cfg(feature = "libc")]
fn wrap_with_result(result: i32) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
