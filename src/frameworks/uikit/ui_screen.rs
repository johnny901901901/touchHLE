/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UIScreen`.

use crate::frameworks::core_graphics::{CGFloat, CGPoint, CGRect, CGSize};
use crate::objc::{id, msg, msg_class, nil, objc_classes, ClassExports, TrivialHostObject, SEL};

#[derive(Default)]
pub struct State {
    main_screen: Option<id>,
    current_mode: Option<id>,
}

fn screen_size_for_current_orientation(env: &mut crate::Environment) -> (u32, u32) {
    let (portrait_width, portrait_height) = env.window().device_family().portrait_size();

    if env.options.landscape_uiscreen_bounds
        || std::env::var_os("TOUCHHLE_LANDSCAPE_UISCREEN_BOUNDS").is_some()
    {
        let is_landscape = !matches!(
            env.window().current_rotation(),
            crate::window::DeviceOrientation::Portrait
        );

        if is_landscape {
            return (portrait_height, portrait_width);
        }
    }

    (portrait_width, portrait_height)
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UIScreen: NSObject

// MARK: - Singleton

+ (id)mainScreen {
    if let Some(screen) = env.framework_state.uikit.ui_screen.main_screen {
        screen
    } else {
        let new = env.objc.alloc_static_object(
            this,
            Box::new(TrivialHostObject),
            &mut env.mem,
        );
        env.framework_state.uikit.ui_screen.main_screen = Some(new);
        new
    }
}

+ (id)screens {
    // Only one screen is ever available.
    let main: id = msg![env; this mainScreen];
    msg_class![env; NSArray arrayWithObject:main]
}

// MARK: - Retain / release (singleton — no-ops)

- (id)retain      { this }
- (())release     {}
- (id)autorelease { this }

// MARK: - Geometry

- (CGRect)bounds {
    let (width, height) = screen_size_for_current_orientation(env);
    if std::env::var_os("TOUCHHLE_LANDSCAPE_UISCREEN_BOUNDS").is_some() {
        log!(
            "TOUCHHLE_LANDSCAPE_UISCREEN_BOUNDS=1: UIScreen bounds reporting {}x{}",
            width,
            height
        );
    }
    CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width:  width  as CGFloat,
            height: height as CGFloat,
        },
    }
}

- (CGRect)nativeBounds {
    // Same as bounds at scale 1 — we don't model the physical pixel grid.
    msg![env; this bounds]
}

- (CGRect)applicationFrame {
    let mut bounds: CGRect = msg![env; this bounds];
    const STATUS_BAR_HEIGHT: CGFloat = 20.0;
    if !env.framework_state.uikit.ui_application.status_bar_hidden {
        bounds.origin.y    += STATUS_BAR_HEIGHT;
        bounds.size.height -= STATUS_BAR_HEIGHT;
    }
    bounds
}

// MARK: - Scale

- (CGFloat)scale {
    env.window().screen_scale() as CGFloat
}

- (CGFloat)nativeScale {
    // Physical pixels == points for our purposes.
    1.0
}

// MARK: - Brightness

- (CGFloat)brightness {
    1.0
}

- (())setBrightness:(CGFloat)_brightness {
    log!("TODO: [UIScreen setBrightness:] (not implemented)");
}

- (bool)wantsSoftwareDimming {
    false
}

- (())setWantsSoftwareDimming:(bool)_value {
    log!("TODO: [UIScreen setWantsSoftwareDimming:] (not implemented)");
}

// MARK: - Display mode / overscan

- (id)currentMode {
    let (width, height) = screen_size_for_current_orientation(env);
    let size = CGSize {
        width:  width  as CGFloat,
        height: height as CGFloat,
    };

    crate::frameworks::uikit::ui_screen_mode::from_size(env, size, 1.0)
}

- (id)preferredMode {
    nil
}

- (id)availableModes {
    let current_mode: id = msg![env; this currentMode];
    msg_class![env; NSArray arrayWithObject:current_mode]
}

// Apple's
// <https://developer.apple.com/documentation/uikit/uiscreen/1617815-setcurrentmode>
// (now deprecated, was originally on UIScreen but in iOS 5+ Apple moved
// resolution control to UIScreenMode itself). The setter is documented as
// a no-op on screens that have only a single supported mode, which is
// the case for every device we emulate (a single fixed framebuffer).
// Accept the call so apps that probe / try to set the mode at launch
// don't get a "doesNotRecognizeSelector" log spam.
- (())setCurrentMode:(id)_mode {
    log_dbg!("[UIScreen setCurrentMode:] ignored; we emulate a single fixed display mode.");
}

- (CGFloat)overscanCompensationInsets {
    // UIEdgeInsetsZero as four floats would need a custom return type.
    // Return 0.0 as a stand-in; the real return type is UIEdgeInsets.
    0.0
}

// MARK: - Mirroring (iOS 4.3+)

- (id)mirroredScreen {
    nil
}

- (bool)isCaptured {
    false
}

// MARK: - Coordinate conversion helpers

- (CGRect)convertRect:(CGRect)rect toScreen:(id)_other_screen {
    // Single-screen device — coordinates are always the same.
    rect
}

- (CGRect)convertRect:(CGRect)rect fromScreen:(id)_other_screen {
    rect
}

- (CGPoint)convertPoint:(CGPoint)point toScreen:(id)_other_screen {
    point
}

- (CGPoint)convertPoint:(CGPoint)point fromScreen:(id)_other_screen {
    point
}
// MARK: - Display Link

- (id)displayLinkWithTarget:(id)target selector:(SEL)selector {
    msg_class![env; CADisplayLink displayLinkWithTarget:target selector:selector]
}

@end

};
