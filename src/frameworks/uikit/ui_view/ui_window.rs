/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UIWindow`.
//!
//! Useful resources:
//! - [Technical Q&A QA1588: Automatic orientation support for iPhone and iPad apps](https://developer.apple.com/library/archive/qa/qa1588/_index.html)
//! - [Technical Q&A QA1688: Why won't my UIViewController rotate with the device?](https://developer.apple.com/library/archive/qa/qa1688/_index.html)

use super::UIViewHostObject;
use crate::dyld::{ConstantExports, HostConstant};
use crate::frameworks::core_graphics::cg_affine_transform::{
    CGAffineTransform, CGAffineTransformIdentity,
};
use crate::frameworks::core_graphics::{CGPoint, CGRect, CGSize};
use crate::frameworks::foundation::NSUInteger;
use crate::frameworks::foundation::ns_string;
use crate::frameworks::uikit::ui_application::{
    UIInterfaceOrientationLandscapeLeft, UIInterfaceOrientationLandscapeRight,
    UIInterfaceOrientationPortraitUpsideDown,
};
use crate::frameworks::uikit::ui_device::{
    UIDeviceOrientationLandscapeLeft, UIDeviceOrientationLandscapeRight,
    UIDeviceOrientationPortraitUpsideDown,
};
use crate::objc::{
    id, msg, msg_class, msg_super, nil, objc_classes, release, retain, Class, ClassExports,
};
use crate::Environment;
use std::collections::HashMap;

#[derive(Default)]
pub struct State {
    /// List of visible windows for internal purposes. Non-retaining!
    ///
    /// This is public because Core Animation also uses it.
    pub windows: Vec<id>,
    /// The most recent window which received `makeKeyAndVisible` message.
    /// Non-retaining!
    pub key_window: Option<id>,
    /// Per-window `rootViewController` association (iOS 4+).
    ///
    /// `UIWindow` itself derives from `UIView` whose host object is shared
    /// with every plain view; we therefore keep the root-VC association out
    /// of the per-view struct and indexed by window pointer instead. The VC
    /// is retained while the window holds it, mirroring Apple's documented
    /// strong-property semantics.
    pub root_view_controllers: HashMap<id, id>,
}

/// Whether `view` is an empty, invisible container that merely happens to lie on
/// top of something the user could actually be aiming at.
///
/// SDKs bolt their overlays straight onto the app's window: Crystal 1.2 (Sword
/// of Fargoal) adds a plain `UIView`, rotates it a quarter turn and sizes it to
/// the whole window so it can host a 353x59 banner in one corner. UIKit's
/// hit-testing hands that container every touch that misses the banner, so the
/// game's `EAGLView` underneath — the whole interface — stops responding.
///
/// Deliberately narrow, because intercepting touches *is* normally correct:
/// - only a plain `UIView`, never a subclass, which might implement `-drawRect:`
///   and draw content we can't see from here;
/// - never a view backed by a `CAEAGLLayer`, which is a render surface and
///   always a legitimate target;
/// - only when the layer has no background colour, i.e. nothing was ever drawn.
///
/// Anything with content, or any custom class, keeps its touch as before.
fn touchhle_is_invisible_container(env: &mut Environment, view: id) -> bool {
    let class: Class = msg![env; view class];
    if env.objc.get_class_name(class) != "UIView" {
        return false;
    }
    let layer = env.objc.borrow::<UIViewHostObject>(view).layer;
    if layer == nil {
        return true;
    }
    let eagl_layer_class: Class = msg_class![env; CAEAGLLayer class];
    let is_eagl: bool = msg![env; layer isKindOfClass:eagl_layer_class];
    if is_eagl {
        return false;
    }
    let background: id = msg![env; layer backgroundColor];
    background == nil
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UIWindow: UIView

// TODO: more?

// UIWindow levels are only relevant when multiple windows overlap. The
// compositor already draws windows in insertion order, so retain the value
// for API compatibility without changing the single-screen ordering.
- (())setWindowLevel:(f32)_level { }
- (f32)windowLevel { 0.0 }

- (id)initWithFrame:(CGRect)frame {
    let this = msg_super![env; this initWithFrame:frame];
    // Undocumented: windows seem to be hidden by default on iOS, unlike views.
    // Super call to bypass the overriden setter on this class, which would post
    // a notification.
    () = msg_super![env; this setHidden:true];

    let list = &mut env.framework_state.uikit.ui_view.ui_window.windows;
    list.push(this);
    log_dbg!(
        "New window: {:?}. New list of all windows: {:?}",
        this,
        list,
    );

    this
}

// NSCoding implementation
- (id)initWithCoder:(id)coder {
    let this = msg_super![env; this initWithCoder:coder];
    // Undocumented: windows seem to be hidden by default on iOS, unlike views.
    // Super call to bypass the overriden setter on this class, which would post
    // a notification.
    () = msg_super![env; this setHidden:true];

    // Real iOS forces UIWindow.frame to match UIScreen.bounds regardless of
    // whatever frame was encoded in the NIB. Interface Builder by default
    // uses a 320x460 canvas (status bar visible) for iPhone XIBs, so a
    // window loaded from a NIB will normally come out at 320x460 and miss
    // the bottom 20px of the screen. Apps that hide the status bar via
    // Info.plist (e.g. Minecraft PE 0.6.x) then end up with their EAGLView
    // sized to the truncated window, producing a visible offset/shift.
    //
    // Mirror Apple's behaviour by re-setting the frame to the full screen
    // bounds after the super decoder runs.
    let screen: id = msg_class![env; UIScreen mainScreen];
    let screen_bounds: CGRect = msg![env; screen bounds];
    let current_bounds: CGRect = msg![env; this bounds];
    if current_bounds.size != screen_bounds.size {
        log_dbg!(
            "UIWindow {:?}: overriding NIB-encoded size {:?} with UIScreen.bounds size {:?}",
            this,
            current_bounds.size,
            screen_bounds.size,
        );
        () = msg![env; this setFrame:screen_bounds];
    }

    let list = &mut env.framework_state.uikit.ui_view.ui_window.windows;
    list.push(this);
    log_dbg!(
        "New window: {:?}. New list of all windows: {:?}",
        this,
        list,
    );

    this
}

- (())dealloc {
    if let Some(key_window) = env.framework_state.uikit.ui_view.ui_window.key_window {
        if key_window == this {
            env.framework_state.uikit.ui_view.ui_window.key_window = None;
        }
    }
    // Release the strong `rootViewController` reference, if any.
    if let Some(root_vc) = env
        .framework_state
        .uikit
        .ui_view
        .ui_window
        .root_view_controllers
        .remove(&this)
    {
        release(env, root_vc);
    }
    let list = &mut env.framework_state.uikit.ui_view.ui_window.windows;
    let idx = list.iter().position(|&w| w == this).unwrap();
    list.remove(idx);
    log_dbg!(
        "Deallocating window {:?}. New list of all windows: {:?}",
        this,
        list,
    );
    msg_super![env; this dealloc]
}

- (())layoutIfNeeded {
    log_dbg!("[(UIWindow*){:?} layoutIfNeeded]", this);
    // Честная реализация: немедленно форсируем пересчет layout'а,
    // отправляя сообщение layoutSubviews самому себе (наследуется от UIView)
    () = msg![env; this layoutSubviews];
}

// Permissive hit-testing for the application window.
//
// On real iOS, UIWindow's frame matches UIScreen.bounds and touches always
// land inside it, so the standard UIView hitTest implementation is fine.
// In touchHLE, however, the host window can be larger than the iPhone's
// virtual screen (Android phones, large desktop windows). Even with the
// touch-coordinate clamp in `transform_input_coords`, edge cases such as
// the rootViewController's view being rotated for landscape orientation
// can leave individual modal/overlay subviews positioned in such a way
// that a touch landing on them produces a window-coordinate point just
// outside of UIWindow's own bounds (off-by-one at the bottom row, status
// bar overlap, etc.). The default UIView.hitTest then early-exits via
// pointInside, returning nil for the entire touch — which is what
// produced the `SUPER HACK: Forcing rejected touch ...` log spam and
// stopped the in-game chat / Create World text fields from ever becoming
// first responder.
//
// Override hitTest to ALWAYS recurse into subviews. If any subview claims
// the touch we return it; otherwise we fall back to the window itself so
// touch dispatch is never lost. This matches Apple's documented intent:
// "Windows don't actively participate in event handling. They merely
// pass events on to their subviews."
- (id)hitTest:(CGPoint)point withEvent:(id)event {
    let subviews = env.objc.borrow::<super::UIViewHostObject>(this).subviews.clone();
    // An empty overlay that claimed the point only because it covers the screen
    // is kept as a last resort: we prefer anything below it that actually has
    // something there, and only fall back to the overlay if nothing does.
    let mut invisible_overlay: id = nil;
    for subview in subviews.into_iter().rev() {
        let hidden: bool = msg![env; subview isHidden];
        let alpha: crate::frameworks::core_graphics::CGFloat = msg![env; subview alpha];
        let interactible: bool = msg![env; subview isUserInteractionEnabled];
        if hidden || alpha < 0.01 || !interactible { continue; }
        let sub_point: CGPoint = msg![env; subview convertPoint:point fromView:this];
        let hit: id = msg![env; subview hitTest:sub_point withEvent:event];
        if hit == nil {
            continue;
        }
        // A descendant claimed the point, so the touch was certainly meant for
        // this branch of the hierarchy.
        if hit != subview {
            return hit;
        }
        if touchhle_is_invisible_container(env, subview) {
            if invisible_overlay == nil {
                log_once!(
                    "Note: a plain UIView with no content covers the window and is \
                     claiming touches; looking underneath it first so the app's own \
                     views still receive input."
                );
                invisible_overlay = subview;
            }
            continue;
        }
        return hit;
    }
    if invisible_overlay != nil {
        return invisible_overlay;
    }
    this
}

- (())setHidden:(bool)is_hidden {
    () = msg_super![env; this setHidden:is_hidden];

    // TODO: post UIWindowDidBecomeVisibleNotification,
    //            UIWindowDidBecomeHiddenNotification
    log_dbg!("[(UIWindow*){:?} setHidden:{:?}]", this, is_hidden);
}

- (())makeKeyWindow {
    // TODO: post UIWindowDidResignKeyNotification for previous key window
    env.framework_state.uikit.ui_view.ui_window.key_window = Some(this);

    let center: id = msg_class![env; NSNotificationCenter defaultCenter];
    let notif_name = ns_string::get_static_str(env, UIWindowDidBecomeKeyNotification);
    () = msg![env; center postNotificationName:notif_name object:this userInfo:nil];
}

- (bool)isKeyWindow {
    env.framework_state.uikit.ui_view.ui_window.key_window == Some(this)
}

- (())makeKeyAndVisible {
    // TODO: We don't currently have send any non-touch events to windows,
    // so there's no meaning in it yet.

    // FIXME: This should also bump the window to the top of the list.

    () = msg![env; this makeKeyWindow];

    // TODO: post UIWindowDidBecomeVisibleNotification
    () = msg![env; this setHidden:false];
}

// Legacy iOS 2/3 pattern: [window setContentView:someView]
// Semantically equivalent to addSubview: for UIWindow.
- (())setContentView:(id)view {
    log_dbg!("[(UIWindow*){:?} setContentView:{:?}]", this, view);
    () = msg![env; this addSubview:view];
}

// Returns the first subview as the content view (legacy behaviour).
- (id)contentView {
    let subviews = &env.objc.borrow::<UIViewHostObject>(this).subviews;
    subviews.first().copied().unwrap_or(nil)
}

// Support for rootViewController (iOS 4+)
// Per Apple's [UIWindow Reference](https://developer.apple.com/documentation/uikit/uiwindow/1621581-rootviewcontroller):
// `rootViewController` is a strong reference. Setting a new root view
// controller installs its `-view` as a full-bounds subview of the window,
// optionally replacing the previously installed root view. We mirror that
// contract exactly: retain the new VC, release the old one, swap the
// associated view, and notify the existing appearance lifecycle through
// `-addSubview:`.
- (())setRootViewController:(id)view_controller {
    log_dbg!("[(UIWindow*){:?} setRootViewController:{:?}]", this, view_controller);

    let previous = env
        .framework_state
        .uikit
        .ui_view
        .ui_window
        .root_view_controllers
        .get(&this)
        .copied()
        .unwrap_or(nil);

    if previous == view_controller {
        return;
    }

    // Retain BEFORE releasing the old one, so that if the new VC was the
    // last strong holder of the old VC we don't temporarily drop it.
    if view_controller != nil {
        retain(env, view_controller);
    }

    if previous != nil {
        let previous_view: id = msg![env; previous view];
        if previous_view != nil {
            () = msg![env; previous_view removeFromSuperview];
        }
        release(env, previous);
    }

    if view_controller != nil {
        let view: id = msg![env; view_controller view];
        // Apple's docs: the root view controller's view is resized to fill
        // the window's bounds. Match the window bounds before adding so
        // -layoutSubviews observes the right geometry.
        let bounds: CGRect = msg![env; this bounds];
        () = msg![env; view setFrame:bounds];
        () = msg![env; this addSubview:view];
        env.framework_state
            .uikit
            .ui_view
            .ui_window
            .root_view_controllers
            .insert(this, view_controller);
    } else {
        env.framework_state
            .uikit
            .ui_view
            .ui_window
            .root_view_controllers
            .remove(&this);
    }
}

- (id)rootViewController {
    env.framework_state
        .uikit
        .ui_view
        .ui_window
        .root_view_controllers
        .get(&this)
        .copied()
        .unwrap_or(nil)
}

// UIResponder implementation
// From the Apple UIView docs regarding [UIResponder nextResponder]:
// "UIWindow returns the application object."
- (id)nextResponder {
    msg_class![env; UIApplication sharedApplication]
}

- (())addSubview:(id)view {
    log_dbg!("[(UIWindow*){:?} addSubview:{:?}] => ()", this, view);

    // Name every view put directly into the window. A game that stops
    // responding to touch usually has something layered over it (a Crystal /
    // OpenFeint / ad-SDK overlay), and this is where that arrives.
    if view != nil {
        let view_class: Class = msg![env; view class];
        let view_class_name = env.objc.get_class_name(view_class).to_owned();
        let view_frame: CGRect = msg![env; view frame];
        let interaction: bool = msg![env; view isUserInteractionEnabled];
        log!(
            "UIWindow addSubview: {} {:?} frame={:?} userInteraction={}",
            view_class_name,
            view,
            view_frame,
            interaction,
        );
    }

    // A direct child of the window with *both* dimensions flexible is asking to
    // fill the window - that is what UIViewAutoresizingFlexibleWidth |
    // FlexibleHeight means when the parent is the screen. Real UIKit applies
    // that during the layout pass which follows the window being sized to the
    // screen; touchHLE forces the window to `UIScreen.bounds` back in
    // `-initWithCoder:`, before any subviews exist, so that pass never had a
    // size change to propagate and the subview kept the size the nib gave it.
    //
    // The Sims Medieval for iPad lays its `EAGLView` out on Interface Builder's
    // 320x460 iPhone canvas with mask 0x12 (flexible width and height) and
    // never touches its frame in code, so the guest's render surface stayed
    // 320x460 inside a 768x1024 window: the game drew into a corner of the
    // screen and the rest of it stayed blank.
    //
    // Deliberately conservative - it only ever grows a view, and only when the
    // view asked to be flexible on both axes, so anything laid out smaller than
    // the window on purpose is left alone.
    if view != nil {
        const FLEXIBLE_WIDTH: NSUInteger = 1 << 1;
        const FLEXIBLE_HEIGHT: NSUInteger = 1 << 4;
        let mask: NSUInteger = msg![env; view autoresizingMask];
        let autoresizes: bool = msg![env; this autoresizesSubviews];
        // A view carrying a transform has already had its geometry decided by
        // whoever rotated it - The Sims Medieval for iPad sets its EAGLView to
        // `(128, -128) 768x1024`, which is a landscape frame seen through a
        // quarter-turn rotation. Stretching that to the window's bounds throws
        // the rotation compensation away.
        let transform: CGAffineTransform = msg![env; view transform];
        if autoresizes
            && transform.is_identity()
            && (mask & FLEXIBLE_WIDTH) != 0
            && (mask & FLEXIBLE_HEIGHT) != 0
        {
            let bounds: CGRect = msg![env; this bounds];
            let frame: CGRect = msg![env; view frame];
            let width = (bounds.size.width - frame.origin.x).max(frame.size.width);
            let height = (bounds.size.height - frame.origin.y).max(frame.size.height);
            if width != frame.size.width || height != frame.size.height {
                log!(
                    "[(UIWindow*){:?} addSubview:{:?}] autoresizingMask={:#04x} asks to fill \
                     the window: stretching {:?} to {:?}",
                    this,
                    view,
                    mask,
                    frame.size,
                    CGSize { width, height },
                );
                () = msg![env; view setFrame:(CGRect {
                    origin: frame.origin,
                    size: CGSize { width, height },
                })];
            }
        }
    }

    if view == nil || env.objc.borrow::<UIViewHostObject>(view).view_controller == nil {
        () = msg_super![env; this addSubview:view];
        return;
    }

    // Below we treat a special case of adding a view controller's view
    // to a window, in order to generate the appropriate display-related
    // notifications. Apple's UIViewController contract:
    //   - viewWillAppear: is sent before the view is added to the
    //     view hierarchy AND will become visible.
    //   - viewDidAppear:  is sent after that.
    // When the view is already a subview, [UIView addSubview:] still
    // moves it to the top of the subviews array (Apple docs:
    // "Calls to this method with a view that is already a subview
    //  result in the view being moved to the end of the subview list.")
    // After moving to the top there are no obstructing siblings, so the
    // appearance lifecycle is fired iff the view is not explicitly
    // hidden via -setHidden:YES. The previous TODO existed because
    // touchHLE had no way to detect this case; we now defer to
    // -isHidden, which mirrors UIKit's actual visibility test.
    let was_subview = env
        .objc
        .borrow::<UIViewHostObject>(this)
        .subviews
        .contains(&view);
    let view_hidden: bool = msg![env; view isHidden];
    let should_fire_appearance = !view_hidden;
    if was_subview {
        log_dbg!(
            "[(UIWindow*){:?} addSubview:{:?}]: re-adding existing subview; \
             moving to top, appearance lifecycle will{} fire (hidden={}).",
            this,
            view,
            if should_fire_appearance { "" } else { " not" },
            view_hidden
        );
    }

    let vc = env.objc.borrow::<UIViewHostObject>(view).view_controller;
    if should_fire_appearance {
        () = msg![env; vc viewWillAppear:false];
    }
    () = msg_super![env; this addSubview:view];
    if should_fire_appearance {
        () = msg![env; vc viewDidAppear:false];
    }

    // Support auto-rotation. This is currently only for apps that request a
    // non-portrait interface orientation via Info.plist, as we do not yet
    // support changes of orientation caused by device rotation (TODO).
    // FIXME: It's unclear when and where this auto-rotation is supposed to
    //        happen. It must have something to do with mounting the view
    //        controller to a window, so we do it here. QA1688 (see top of file)
    //        mentions a breaking behaviour change in iOS 6 that makes
    //        auto-rotation rely on rootViewController (a property only found in
    //        iOS 6), so the current implementation is specific to iOS <= 5.
    // FIXME: Are we supposed to notify the view somehow of the rotation?
    // FIXME: What do we do if shouldAutorotateToInterfaceOrientation:
    //        returns false? The status bar has already been rotated…
    // FIXME: The device orientation stored on env.window can come from one of
    //        three places (user/default options, setStatusBarOrientation: etc,
    //        Info.plist UIInterfaceOrientation etc). It's not clear if these
    //        are really equivalent and should all trigger autorotation.
    if let Some(orientation) = match env.window.as_ref().unwrap().current_rotation() {
        crate::window::DeviceOrientation::LandscapeLeft => Some(UIDeviceOrientationLandscapeLeft),
        crate::window::DeviceOrientation::LandscapeRight => Some(UIDeviceOrientationLandscapeRight),
        crate::window::DeviceOrientation::PortraitUpsideDown => Some(UIDeviceOrientationPortraitUpsideDown),
        // Portrait is the default so we don't do anything here.
        crate::window::DeviceOrientation::Portrait => None,
    } {
        // (UIInterfaceOrientation and UIDeviceOrientation are compatible enums,
        //  here we use whichever is clearer contextually.)
        let should = msg![env; vc shouldAutorotateToInterfaceOrientation:orientation];
        log_dbg!("[{:?} shouldAutorotateToInterfaceOrientation:{:?}] => {:?}", vc, orientation, should);
        if should {
            log_dbg!("App requested autorotation; applying orientation transform to view {:?}.", view);
            let transform = match orientation {
                UIInterfaceOrientationLandscapeLeft => CGAffineTransform::make_rotation(-std::f32::consts::FRAC_PI_2),
                UIInterfaceOrientationLandscapeRight => CGAffineTransform::make_rotation(std::f32::consts::FRAC_PI_2),
                UIInterfaceOrientationPortraitUpsideDown => CGAffineTransform::make_rotation(std::f32::consts::PI),
                other => {
                    // UIInterfaceOrientation has Portrait/PortraitUpsideDown/
                    // LandscapeLeft/LandscapeRight; the first two are filtered
                    // out earlier (Portrait => None and PortraitUpsideDown
                    // isn't reachable from window::DeviceOrientation today).
                    // Fall back to the identity transform so an unexpected
                    // orientation can't take down the host.
                    log!(
                        "Warning: UIWindow autorotation: unsupported interface orientation {}; using identity transform.",
                        other
                    );
                    CGAffineTransformIdentity
                }
            };

            let window_frame: CGRect = msg![env; this frame];
            log_dbg!("Window frame: {window_frame:?}");
            let view_frame: CGRect = msg![env; view frame];
            log_dbg!("Old view frame: {view_frame:?}");

            () = msg![env; view setTransform:transform];

            // Re-apply the view's old frame to compensate for the rotation
            // effectively offseting its center position and changing the size.
            // FIXME: I have no idea if this is how this should be solved, but
            //        it works for DMC4 Refrain at least.

            let view_frame: CGRect = msg![env; view frame];
            log_dbg!("Old view frame after transform: {view_frame:?}");

            () = msg![env; view setFrame:window_frame];

            let view_frame: CGRect = msg![env; view frame];
            log_dbg!("New view frame after re-applying old view frame: {view_frame:?}");
        }
    }
}

- (CGPoint)convertPoint:(CGPoint)point
             fromWindow:(id)other { // UIWindow*
    let this_layer: id = msg![env; this layer];
    // Resolves to nil if other is nil.
    let other_layer: id = msg![env; other layer];
    msg![env; this_layer convertPoint:point fromLayer:other_layer]
}
- (CGPoint)convertPoint:(CGPoint)point
               toWindow:(id)other { // UIWindow*
    let this_layer: id = msg![env; this layer];
    // Resolves to nil if other is nil.
    let other_layer: id = msg![env; other layer];
    msg![env; this_layer convertPoint:point toLayer:other_layer]
}

// Apple's
// <https://developer.apple.com/documentation/uikit/uiwindow/1621597-screen>:
// `screen` returns the `UIScreen` the window is currently displayed on,
// and the documented setter (`setScreen:`) is used by apps that wish to
// move a window to an external display. We emulate a single main screen
// only, so the getter always returns `+[UIScreen mainScreen]` and the
// setter is a no-op that just acknowledges the call to keep the
// "doesNotRecognizeSelector" warning from firing on apps that
// unconditionally call `window.screen = UIScreen.mainScreen` during
// `application:didFinishLaunchingWithOptions:`.
- (id)screen {
    msg_class![env; UIScreen mainScreen]
}
- (())setScreen:(id)_screen {
    log_dbg!(
        "[UIWindow setScreen:] ignored; touchHLE only emulates the main screen."
    );
}

@end

};

/// Window life-cycle notifications.
///
/// Apple `UIWindow.h` declares each name as
/// `UIKIT_EXTERN NSNotificationName const ...`. The literal value
/// posted to `NSNotificationCenter` is the symbol's own name, which is
/// what `-[NSNotificationCenter addObserver:selector:name:object:]`
/// compares against with `-[NSString isEqualToString:]`.
///
/// References:
/// * Apple [UIWindow notifications](https://developer.apple.com/documentation/uikit/uiwindow)
const UIWindowDidBecomeKeyNotification: &str = "UIWindowDidBecomeKeyNotification";
const UIWindowDidResignKeyNotification: &str = "UIWindowDidResignKeyNotification";
const UIWindowDidBecomeHiddenNotification: &str = "UIWindowDidBecomeHiddenNotification";
const UIWindowDidBecomeVisibleNotification: &str = "UIWindowDidBecomeVisibleNotification";
/// Keyboard notifications
/// TODO: more keyboard notifications
pub const UIKeyboardWillShowNotification: &str = "UIKeyboardWillShowNotification";
pub const UIKeyboardDidShowNotification: &str = "UIKeyboardDidShowNotification";
pub const UIKeyboardWillHideNotification: &str = "UIKeyboardWillHideNotification";
pub const UIKeyboardDidHideNotification: &str = "UIKeyboardDidHideNotification";
pub const UIKeyboardBoundsUserInfoKey: &str = "UIKeyboardBoundsUserInfoKey";

pub const CONSTANTS: ConstantExports = &[
    (
        "_UIWindowDidBecomeKeyNotification",
        HostConstant::NSString(UIWindowDidBecomeKeyNotification),
    ),
    (
        "_UIWindowDidResignKeyNotification",
        HostConstant::NSString(UIWindowDidResignKeyNotification),
    ),
    (
        "_UIWindowDidBecomeHiddenNotification",
        HostConstant::NSString(UIWindowDidBecomeHiddenNotification),
    ),
    (
        "_UIWindowDidBecomeVisibleNotification",
        HostConstant::NSString(UIWindowDidBecomeVisibleNotification),
    ),
    // _UIKeyboardWillShowNotification, _UIKeyboardDidShowNotification,
    // _UIKeyboardWillHideNotification, _UIKeyboardDidHideNotification and
    // _UIKeyboardBoundsUserInfoKey are exported from
    // uikit::ui_keyboard::CONSTANTS; not duplicated here.
];
