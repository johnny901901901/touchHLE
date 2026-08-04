use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_uchar};

pub struct TouchHLEIOSGameMetadata {
    display_name: CString,
    bundle_identifier: CString,
    orientation_capabilities: u32,
    icon_rgba: Vec<u8>,
    icon_width: u32,
    icon_height: u32,
}

fn c_string(value: String) -> CString {
    CString::new(value.replace('\0', "")).unwrap()
}

fn run_touchhle(args: Vec<String>) -> i32 {
    touchHLE::clear_host_exit_request();
    // The emulator runs on the main thread and this is called from
    // Objective-C. Letting a panic unwind past here is undefined behaviour at
    // the FFI boundary, and in practice it wedges the whole app: the screen
    // stays black, the exit button stops responding, and nothing says why.
    // Convert it into an error return so the host restores the library and
    // reports it.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        touchHLE::main(args.into_iter())
    }));

    match outcome {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("touchHLE failed: {error}");
            1
        }
        Err(_) => {
            eprintln!("touchHLE panicked; see the log above for the cause");
            2
        }
    }
}

#[no_mangle]
pub extern "C" fn touchhle_ios_request_exit() {
    touchHLE::request_host_exit();
}

#[no_mangle]
pub extern "C" fn touchhle_ios_current_fps() -> f32 {
    touchHLE::host_fps()
}

#[no_mangle]
pub extern "C" fn touchhle_ios_run() -> i32 {
    run_touchhle(vec![String::new()])
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_create(
    path: *const c_char,
) -> *mut TouchHLEIOSGameMetadata {
    if path.is_null() {
        return std::ptr::null_mut();
    }

    let path = match CStr::from_ptr(path).to_str() {
        Ok(path) => std::path::Path::new(path),
        Err(_) => return std::ptr::null_mut(),
    };
    let metadata = match touchHLE::inspect_host_app(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("Could not inspect game metadata: {error}");
            return std::ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(TouchHLEIOSGameMetadata {
        display_name: c_string(metadata.display_name),
        bundle_identifier: c_string(metadata.bundle_identifier),
        orientation_capabilities: metadata.orientation_capabilities,
        icon_rgba: metadata.icon_rgba,
        icon_width: metadata.icon_width,
        icon_height: metadata.icon_height,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_display_name(
    metadata: *const TouchHLEIOSGameMetadata,
) -> *const c_char {
    metadata
        .as_ref()
        .map_or(std::ptr::null(), |metadata| metadata.display_name.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_bundle_identifier(
    metadata: *const TouchHLEIOSGameMetadata,
) -> *const c_char {
    metadata.as_ref().map_or(std::ptr::null(), |metadata| {
        metadata.bundle_identifier.as_ptr()
    })
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_orientation_capabilities(
    metadata: *const TouchHLEIOSGameMetadata,
) -> u32 {
    metadata
        .as_ref()
        .map_or(1, |metadata| metadata.orientation_capabilities)
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_icon_rgba(
    metadata: *const TouchHLEIOSGameMetadata,
) -> *const c_uchar {
    metadata
        .as_ref()
        .map_or(std::ptr::null(), |metadata| metadata.icon_rgba.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_icon_width(
    metadata: *const TouchHLEIOSGameMetadata,
) -> u32 {
    metadata.as_ref().map_or(0, |metadata| metadata.icon_width)
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_icon_height(
    metadata: *const TouchHLEIOSGameMetadata,
) -> u32 {
    metadata.as_ref().map_or(0, |metadata| metadata.icon_height)
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_game_metadata_free(metadata: *mut TouchHLEIOSGameMetadata) {
    if !metadata.is_null() {
        drop(Box::from_raw(metadata));
    }
}

#[no_mangle]
pub unsafe extern "C" fn touchhle_ios_run_game(
    path: *const c_char,
    scale_hack: i32,
    orientation: i32,
    network_access: i32,
    analog_stick_tilt_controls: i32,
) -> i32 {
    if path.is_null() {
        eprintln!("touchHLE failed: game path was null");
        return 1;
    }

    let path = match CStr::from_ptr(path).to_str() {
        Ok(path) => path.to_owned(),
        Err(error) => {
            eprintln!("touchHLE failed: game path was not UTF-8: {error}");
            return 1;
        }
    };

    let mut args = vec!["touchHLE".to_owned(), path];

    if (1..=4).contains(&scale_hack) {
        args.push(format!("--scale-hack={scale_hack}"));
    }

    match orientation {
        1 => args.push("--landscape-left".to_owned()),
        2 => args.push("--landscape-right".to_owned()),
        3 => args.push("--upside-down".to_owned()),
        _ => {}
    }

    if network_access != 0 {
        args.push("--allow-network-access".to_owned());
    }
    if analog_stick_tilt_controls == 0 {
        args.push("--disable-analog-stick-tilt-controls".to_owned());
    }

    run_touchhle(args)
}
