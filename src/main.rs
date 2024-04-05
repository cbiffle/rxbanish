// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A simple program for hiding the X11 mosue pointer while you're typing.
//!
//! Inspired by xbanish, but using XCB, and with a lot fewer uses of
//! uninitialized stack memory.

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use xcb::{
    x::{KeyButMask, Window, self},
    xfixes,
    xinput::{self, DeviceUse, InputClass, DeviceChange},
    Connection, Event, Extension,
};

/// Basic program for hiding the X11 mouse pointer while you're typing.
#[derive(Parser)]
struct Rxbanish {
    /// Modifier keys to ignore, so that the pointer doesn't disappear as soon
    /// as you press, say, shift. You can use this flag more than once to choose
    /// multiple modifiers, or use "all" as shorthand for everything.
    #[clap(short, long, value_enum, value_name = "MOD")]
    ignore_mod: Vec<Mod>,
}

/// Convenient clap-compatible names for modifier keys
#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mod {
    Shift,
    Caps,
    Ctrl,
    Mod1,
    Mod2,
    Mod3,
    Mod4,
    All,
}

/// Translate user-facing modifier key names, including "all," to X modifier
/// masks.
impl From<Mod> for KeyButMask {
    fn from(value: Mod) -> Self {
        match value {
            Mod::Shift => KeyButMask::SHIFT,
            Mod::Caps => KeyButMask::LOCK,
            Mod::Ctrl => KeyButMask::CONTROL,
            Mod::Mod1 => KeyButMask::MOD1,
            Mod::Mod2 => KeyButMask::MOD2,
            Mod::Mod3 => KeyButMask::MOD3,
            Mod::Mod4 => KeyButMask::MOD4,

            // for ALL, we want to set all of the key related bits, and _not_
            // the mouse button bits!
            Mod::All => {
                KeyButMask::SHIFT
                    | KeyButMask::LOCK
                    | KeyButMask::CONTROL
                    | KeyButMask::MOD1
                    | KeyButMask::MOD2
                    | KeyButMask::MOD3
                    | KeyButMask::MOD4
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Rxbanish::parse();

    // Combine all user-specified ignore mods.
    let ignored_mods = args
        .ignore_mod
        .into_iter()
        .fold(KeyButMask::empty(), |a, b| a | b.into());

    // Let's go!
    let (conn, screen_num) = Connection::connect_with_extensions(
        // Display choice
        None,
        // Mandatory extensions
        &[Extension::XFixes, Extension::Input],
        // Optional extensions
        &[],
    )?;

    // Identify the root window. We'll use this for event registration and
    // cursor manipulation. Basically everything.
    let setup = conn.get_setup();
    let screen = setup.roots().nth(screen_num as usize).unwrap();
    let root = screen.root();

    // Check the version of XFixes at the server. For reasons I don't understand
    // this appears to be load-bearing; without it, the XFixes calls will return
    // an error. That's particularly strange since the C programs I'm reading
    // don't bother with this.
    let xfvresp =
        conn.wait_for_reply(conn.send_request(&xfixes::QueryVersion {
            client_major_version: 4,
            client_minor_version: 0,
        }))?;
    if xfvresp.major_version() < 4 {
        bail!("No compatible Xfixes version available");
    }

    // Alright, snoop on all input devices. It's kind of terrifying that you can
    // do this in X tbh.
    let rawmotion = snoop_xinput(&conn, root)?;

    // Avoid generating excess hide/show pointer calls using a fancy bool.
    let mut hidden = false;

    loop {
        match conn.wait_for_event()? {
            Event::Input(
                xinput::Event::RawMotion(_) | xinput::Event::RawButtonPress(_),
            ) => {
                // Any movement or button is enough to reveal the cursor.
                if hidden {
                    show_pointer(&conn, root)?;
                    hidden = false;
                }
            }
            Event::Input(xinput::Event::DeviceKeyRelease(e)) => {
                // We only hide the cursor on key _release_ because otherwise we
                // can't distinguish e.g. tapping shift using the event
                // interface that we're using.
                if e.state().intersects(ignored_mods) {
                    continue;
                }

                if !hidden {
                    hide_pointer(&conn, root)?;
                    hidden = true;
                }
            }
            Event::Input(xinput::Event::DevicePresenceNotify(e)) => {
                if e.devchange() == DeviceChange::Enabled {
                    snoop_device(&conn, root, rawmotion, e.device_id())?;
                }
            }
            Event::X(x::Event::MappingNotify(_)) => {
                // We appear to get these as a side effect of device changes. We
                // don't need them for anything.
            }
            e => {
                // This is _really_ not supposed to happen if I did the X event
                // registration correctly...
                println!("OTHER {e:?}");
            }
        }
    }
}

/// Registers to be notified of all input events on a certain window, which in
/// our case is always the root window.
fn snoop_xinput(conn: &Connection, window: Window) -> anyhow::Result<bool> {
    let mut rawmotion = false;

    // Check what XInput version we've got. We want at least 2 for raw motion
    // events, apparently.
    let xiqv_response =
        conn.wait_for_reply(conn.send_request(&xinput::XiQueryVersion {
            major_version: 2,
            minor_version: 0,
        }));
    if xiqv_response.is_ok() {
        // Register for raw pointer-related events.
        conn.send_and_check_request(&xinput::XiSelectEvents {
            window,
            masks: &[xinput::EventMaskBuf::new(
                xinput::Device::AllMaster,
                &[xinput::XiEventMask::RAW_MOTION
                    | xinput::XiEventMask::RAW_BUTTON_PRESS],
            )],
        })?;

        println!("using xinput2 raw motion events");

        rawmotion = true;
    }

    let list_reply =
        conn.wait_for_reply(conn.send_request(&xinput::ListInputDevices {}))?;
    let mut any_found = false;

    for (devinfo, name) in list_reply.devices().iter().zip(list_reply.names()) {
        if !matches!(
            devinfo.device_use(),
            DeviceUse::IsXExtensionKeyboard | DeviceUse::IsXExtensionPointer
        ) {
            continue;
        }
        any_found |= snoop_device(conn, window, rawmotion, devinfo.device_id())?;
    }

    // Apparently secret code for Device Presence class, discovered by reading C
    // headers.
    const DEVICE_PRESENCE: u32 = 0x1_0000;

    conn.send_and_check_request(&xinput::SelectExtensionEvent {
        window,
        classes: &[DEVICE_PRESENCE],
    })?;


    Ok(rawmotion)
}

/// Registers to snoop on a specific device given by ID.
fn snoop_device(
    conn: &Connection,
    window: Window,
    rawmotion: bool,
    device_id: u8,
) -> Result<bool> {
    let dev_reply =
        conn.wait_for_reply(conn.send_request(&xinput::OpenDevice {
            device_id,
        }))?;

    let mut event_list = vec![];

    for c in dev_reply.class_info() {
        match c.class_id() {
            InputClass::Key => {
                // We don't actually need key press events.
                //event_list.push(make_event_code(devinfo.device_id(), c.event_type_base()));

                // Apparently event_type_base + 1 for key inputs is release?
                // I learned this by READING C HEADERS. Not sure where
                // you're supposed to learn it.
                event_list.push(make_event_code(
                        device_id,
                        c.event_type_base() + 1,
                ));
            }
            InputClass::Valuator => {
                if rawmotion {
                    continue;
                }
                event_list.push(make_event_code(
                        device_id,
                        c.event_type_base(),
                ));
            }
            _ => (),
        }
    }

    conn.send_and_check_request(&xinput::CloseDevice {
        device_id,
    })?;

    conn.send_and_check_request(&xinput::SelectExtensionEvent {
        window,
        classes: &event_list,
    })?;

    Ok(!event_list.is_empty())
}

/// Makes an operand suitable for use with SelectExtensionEvent, which appears
/// to not be documented anywhere except C macros, hooray X11.
fn make_event_code(device_id: u8, event_type: u8) -> u32 {
    u32::from(device_id) << 8 | u32::from(event_type)
}

fn show_pointer(conn: &Connection, root: Window) -> Result<()> {
    println!("showing pointer");

    conn.send_and_check_request(&xfixes::ShowCursor { window: root })?;
    Ok(())
}

fn hide_pointer(conn: &Connection, root: Window) -> Result<()> {
    println!("hiding pointer");

    conn.send_and_check_request(&xfixes::HideCursor { window: root })?;
    Ok(())
}
