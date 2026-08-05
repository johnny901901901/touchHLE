/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UITouch`.

use super::ui_event;
use super::ui_gesture_recognizer::{
    UIGestureRecognizerHostObject, UIGestureRecognizerStatePossible,
    UIGestureRecognizerStateRecognized, UISwipeGestureRecognizerDirectionDown,
    UISwipeGestureRecognizerDirectionLeft, UISwipeGestureRecognizerDirectionRight,
    UISwipeGestureRecognizerDirectionUp,
};
use crate::frameworks::core_graphics::{CGPoint, CGRect};
use crate::frameworks::foundation::{NSInteger, NSTimeInterval, NSUInteger};
use crate::mem::MutVoidPtr;
use crate::objc::{
    autorelease, id, msg, msg_class, msg_send_no_type_checking, nil, objc_classes, release, retain, ClassExports, HostObject,
    NSZonePtr,
};
use crate::window::{Coords, Event, FingerId};
use crate::Environment;
use std::collections::hash_map::{Entry, HashMap};
use std::collections::HashSet;

pub type UITouchPhase = NSInteger;
pub const UITouchPhaseBegan: UITouchPhase = 0;
pub const UITouchPhaseMoved: UITouchPhase = 1;
pub const UITouchPhaseStationary: UITouchPhase = 2;
pub const UITouchPhaseEnded: UITouchPhase = 3;

#[derive(Default)]
pub struct State {
    pub current_touches: HashMap<FingerId, id>,
}

#[derive(Default)]
pub(super) struct UITouchHostObject {
    pub(super) view: id,
    pub(super) window: id,
    location: CGPoint,
    previous_location: CGPoint,
    /// Where this touch first landed (in window/screen coordinates). Used to
    /// compute the total displacement for swipe gesture recognition.
    start_location: CGPoint,
    timestamp: NSTimeInterval,
    phase: UITouchPhase,
}
impl HostObject for UITouchHostObject {}

fn touchhle_cocos_view_class_name(env: &mut Environment, view: id) -> String {
    if view == nil { return String::new(); }
    let view_class: crate::objc::Class = msg![env; view class];
    env.objc.get_class_name(view_class).to_owned()
}

fn touchhle_cocos_is_gl_or_game_view_name(class_name: &str) -> bool {
    matches!(
        class_name,
        "CCGLView" | "EAGLView" | "CCEAGLView" | "GLKView" |
        "Cocos2dxGLView" | "Cocos2dView" | "DirectorView" | "CCUIViewWrapper"
    ) || class_name.contains("EAGL")
        || class_name.contains("GLView")
        || class_name.contains("Cocos")
        || class_name.contains("CCGL")
        || class_name.contains("Unity")
        || class_name.contains("UnityView")
        || class_name.contains("UnityGLView")
        || class_name.contains("UnityRenderingView")
        || class_name.contains("RenderView")
        || class_name.contains("GameView")
        || class_name.contains("RootView")
}

fn touchhle_should_use_landscape_touch_remap(env: &Environment) -> bool {
    match env.bundle.bundle_identifier() {
        // Confirmed landscape Source/Cocos games.
        "at.source.veggie1" | "at.source.potato3D" | "at.source.potpan" | "com.robtop.geometryjump" => true,

        // TomatoZombie is native portrait.
        "at.source.tomzom" => false,

        // Manual override for testing.
        _ => std::env::var_os("TOUCHHLE_TOUCH_LOCATION_PORTRAIT_TO_LANDSCAPE").is_some()
            || std::env::var_os("TOUCHHLE_COCOS_TOUCH_REMAP").is_some()
            || std::env::var_os("TOUCHHLE_UNITY_TOUCH_REMAP").is_some()
            || std::env::var_os("TOUCHHLE_ENGINE_TOUCH_REMAP").is_some(),
    }
}

fn should_remap_touch_location_for_view(env: &mut Environment, view: id) -> bool {
    if !touchhle_should_use_landscape_touch_remap(env) {
        return false;
    }

    if view == nil { return false; }
    let class_name = touchhle_cocos_view_class_name(env, view);
    touchhle_cocos_is_gl_or_game_view_name(&class_name)
}

fn touchhle_cocos_target_size() -> (f32, f32) {
    std::env::var("TOUCHHLE_COCOS_TOUCH_SIZE").or_else(|_| std::env::var("TOUCHHLE_UNITY_TOUCH_SIZE")).or_else(|_| std::env::var("TOUCHHLE_ENGINE_TOUCH_SIZE"))
        .ok()
        .and_then(|v| {
            let mut parts = v.split(|c| c == 'x' || c == 'X' || c == ',');
            let w = parts.next()?.trim().parse::<f32>().ok()?;
            let h = parts.next()?.trim().parse::<f32>().ok()?;
            Some((w, h))
        })
        .unwrap_or((480.0, 320.0))
}

fn touchhle_cocos_remap_point(env: &mut Environment, view: id, point: CGPoint) -> CGPoint {
    let old_x = point.x;
    let old_y = point.y;
    let (target_w, target_h) = touchhle_cocos_target_size();
    let mode = std::env::var("TOUCHHLE_TOUCH_MODE").unwrap_or_else(|_| {
        match env.bundle.bundle_identifier() {
            "at.source.veggie1" | "at.source.potato3D" | "at.source.potpan" | "com.robtop.geometryjump" => "scale".to_string(),
            _ => std::env::var("TOUCHHLE_COCOS_TOUCH_MODE")
                .or_else(|_| std::env::var("TOUCHHLE_UNITY_TOUCH_MODE"))
                .or_else(|_| std::env::var("TOUCHHLE_ENGINE_TOUCH_MODE"))
                .unwrap_or_else(|_| "scale".to_string()),
        }
    });

    let source_bounds: CGRect = if view != nil {
        msg![env; view bounds]
    } else {
        CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: crate::frameworks::core_graphics::CGSize { width: 320.0, height: 480.0 } }
    };
    let source_w = if source_bounds.size.width.abs() > 0.001 { source_bounds.size.width.abs() } else { 320.0 };
    let source_h = if source_bounds.size.height.abs() > 0.001 { source_bounds.size.height.abs() } else { 480.0 };

    let (mut new_x, mut new_y) = match mode.as_str() {
        "identity" | "none" => (old_x, old_y),
        "right" => (old_y * (target_w / source_h), target_h - old_x * (target_h / source_w)),
        "right-flip-x" => (target_w - old_y * (target_w / source_h), target_h - old_x * (target_h / source_w)),
        "left" => (target_w - old_y * (target_w / source_h), old_x * (target_h / source_w)),
        "left-flip-x" => (old_y * (target_w / source_h), old_x * (target_h / source_w)),
        "flip-x" => (target_w - old_x * (target_w / source_w), old_y * (target_h / source_h)),
        "flip-y" => (old_x * (target_w / source_w), target_h - old_y * (target_h / source_h)),
        "scale-1024x768" => (old_x * (1024.0 / source_w), old_y * (768.0 / source_h)),
        "scale-480x320" | "scale" | _ => (old_x * (target_w / source_w), old_y * (target_h / source_h)),
    };

    if let Ok(offset) = std::env::var("TOUCHHLE_TOUCH_LOCATION_X_OFFSET") {
        if let Ok(offset) = offset.parse::<f32>() { new_x += offset; }
    }
    if let Ok(offset) = std::env::var("TOUCHHLE_TOUCH_LOCATION_Y_OFFSET") {
        if let Ok(offset) = offset.parse::<f32>() { new_y += offset; }
    }

    if std::env::var_os("TOUCHHLE_COCOS_NO_TOUCH_CLAMP").is_none() {
        new_x = new_x.clamp(0.0, (target_w - 1.0).max(0.0));
        new_y = new_y.clamp(0.0, (target_h - 1.0).max(0.0));
    }

    log_dbg!(
        "UITouch Cocos remap mode={} source=({:.1}x{:.1}) target=({:.1}x{:.1}): ({:.1}, {:.1}) -> ({:.1}, {:.1})",
        mode, source_w, source_h, target_w, target_h, old_x, old_y, new_x, new_y
    );

    CGPoint { x: new_x, y: new_y }
}

fn touchhle_cocos_should_allow_multitouch(env: &mut Environment, view: id) -> bool {
    if std::env::var_os("TOUCHHLE_COCOS_FORCE_SINGLE_TOUCH").is_some()
        || std::env::var_os("TOUCHHLE_UNITY_FORCE_SINGLE_TOUCH").is_some()
        || std::env::var_os("TOUCHHLE_ENGINE_FORCE_SINGLE_TOUCH").is_some()
    {
        return false;
    }
    if std::env::var_os("TOUCHHLE_COCOS_FORCE_MULTITOUCH").is_some()
        || std::env::var_os("TOUCHHLE_UNITY_FORCE_MULTITOUCH").is_some()
        || std::env::var_os("TOUCHHLE_ENGINE_FORCE_MULTITOUCH").is_some()
    {
        return true;
    }
    let class_name = touchhle_cocos_view_class_name(env, view);
    touchhle_cocos_is_gl_or_game_view_name(&class_name)
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UITouch: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(UITouchHostObject {
        view: nil,
        window: nil,
        location: CGPoint { x: 0.0, y: 0.0 },
        previous_location: CGPoint { x: 0.0, y: 0.0 },
        start_location: CGPoint { x: 0.0, y: 0.0 },
        timestamp: 0.0,
        phase: UITouchPhaseBegan,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let &mut UITouchHostObject { view, window, .. } = env.objc.borrow_mut(this);
    release(env, view);
    release(env, window);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (CGPoint)locationInView:(id)that_view {
    let &UITouchHostObject { location, window, view, .. } = env.objc.borrow(this);
    let location_in_window: CGPoint = msg![env; window
        convertPoint:location fromWindow:nil];
    let mut result: CGPoint = if that_view == nil {
        location_in_window
    } else {
        msg![env;
        that_view convertPoint:location_in_window fromView:window]
    };

    let remap_view = if that_view != nil { that_view } else { view };
    if touchhle_should_use_landscape_touch_remap(env) || should_remap_touch_location_for_view(env, remap_view) {
        // Important: this happens AFTER UIKit hit-testing. The touch can still
        // hit a portrait-sized EAGL/CCGL view, but the game can receive the
        // Cocos/OpenGL coordinate system it expects.
        result = touchhle_cocos_remap_point(env, remap_view, result);
    }

    result
}

- (CGPoint)previousLocationInView:(id)that_view {
    let &UITouchHostObject { previous_location, window, view, .. } = env.objc.borrow(this);
    let location_in_window: CGPoint = msg![env; window
        convertPoint:previous_location fromWindow:nil];
    let mut result: CGPoint = if that_view == nil {
        location_in_window
    } else {
        msg![env;
        that_view convertPoint:location_in_window fromView:window]
    };

    let remap_view = if that_view != nil { that_view } else { view };
    if touchhle_should_use_landscape_touch_remap(env) || should_remap_touch_location_for_view(env, remap_view) {
        result = touchhle_cocos_remap_point(env, remap_view, result);
    }

    result
}

- (id)view {
    env.objc.borrow::<UITouchHostObject>(this).view
}

- (id)window {
    env.objc.borrow::<UITouchHostObject>(this).window
}

- (NSTimeInterval)timestamp {
    env.objc.borrow::<UITouchHostObject>(this).timestamp
}

- (NSUInteger)tapCount {
    1
}

- (UITouchPhase)phase {
    env.objc.borrow::<UITouchHostObject>(this).phase
}

@end

};

pub fn handle_event(env: &mut Environment, event: Event) {
    let touch_ids: Vec<id> = env
        .framework_state
        .uikit
        .ui_touch
        .current_touches
        .values()
        .cloned()
        .collect();
    for touch in touch_ids {
        env.objc.borrow_mut::<UITouchHostObject>(touch).phase = UITouchPhaseStationary;
    }
    match event {
        Event::TouchesDown(map) => handle_touches_down(env, map),
        Event::TouchesMove(map) => handle_touches_move(env, map),
        Event::TouchesUp(map) => handle_touches_up(env, map),
        other => {
            // ui_touch::handle_event only ever wants touch events; non-touch
            // events are filtered out before getting here. Log instead of
            // panicking the host if that contract is ever violated.
            log!(
                "Warning: ui_touch::handle_event: unsupported event {:?}; ignored.",
                other
            );
        }
    }
}


fn touchhle_cocos_touch_aliases_enabled(env: &mut Environment, view: id) -> bool {
    if std::env::var_os("TOUCHHLE_DISABLE_COCOS_TOUCH_ALIASES").is_some() { return false; }
    if std::env::var_os("TOUCHHLE_COCOS_TOUCH_ALIASES").is_some() { return true; }
    let class_name = touchhle_cocos_view_class_name(env, view);
    touchhle_cocos_is_gl_or_game_view_name(&class_name)
        || class_name.starts_with("CC")
        || class_name.contains("Layer")
        || class_name.contains("Scene")
        || class_name.contains("Menu")
}

fn touchhle_send_cocos_touch_aliases(env: &mut Environment, view: id, phase: &str, touches: id, event: id) {
    if view == nil || !touchhle_cocos_touch_aliases_enabled(env, view) { return; }

    let all_sel_name = match phase {
        "began" => "ccTouchesBegan:withEvent:",
        "moved" => "ccTouchesMoved:withEvent:",
        "ended" => "ccTouchesEnded:withEvent:",
        "cancelled" => "ccTouchesCancelled:withEvent:",
        _ => return,
    };
    let one_sel_name = match phase {
        "began" => "ccTouchBegan:withEvent:",
        "moved" => "ccTouchMoved:withEvent:",
        "ended" => "ccTouchEnded:withEvent:",
        "cancelled" => "ccTouchCancelled:withEvent:",
        _ => return,
    };

    let all_sel = env.objc.register_host_selector(all_sel_name.to_string(), &mut env.mem);
    let responds_all: bool = msg![env; view respondsToSelector:all_sel];
    if responds_all {
        let _: () = msg_send_no_type_checking(env, (view, all_sel, touches, event));
    }

    let one_sel = env.objc.register_host_selector(one_sel_name.to_string(), &mut env.mem);
    let responds_one: bool = msg![env; view respondsToSelector:one_sel];
    if responds_one {
        let arr: id = msg![env; touches allObjects];
        let count: NSUInteger = msg![env; arr count];
        if count > 0 {
            let touch: id = msg![env; arr objectAtIndex:0];
            if phase == "began" {
                let _: u32 = msg_send_no_type_checking(env, (view, one_sel, touch, event));
            } else {
                let _: () = msg_send_no_type_checking(env, (view, one_sel, touch, event));
            }
        }
    }
}

fn touchhle_send_cocos_touch_aliases_to_chain(env: &mut Environment, view: id, phase: &str, touches: id, event: id) {
    let mut current = view;
    let mut depth = 0;
    while current != nil && depth < 32 {
        touchhle_send_cocos_touch_aliases(env, current, phase, touches, event);
        current = msg![env; current superview];
        depth += 1;
    }
}

fn touchhle_find_cocos_touch_target(env: &mut Environment, root: id) -> id {
    if root == nil { return nil; }
    let subviews: id = msg![env; root subviews];
    if subviews != nil {
        let count: NSUInteger = msg![env; subviews count];
        for i in (0..count).rev() {
            let child: id = msg![env; subviews objectAtIndex:i];
            let found = touchhle_find_cocos_touch_target(env, child);
            if found != nil { return found; }
        }
    }
    let class_name = touchhle_cocos_view_class_name(env, root);
    if touchhle_cocos_is_gl_or_game_view_name(&class_name) { root } else { nil }
}

fn handle_touches_down(env: &mut Environment, map: HashMap<FingerId, Coords>) {
    let pool: id = msg_class![env;
        NSAutoreleasePool new];

    let timestamp: NSTimeInterval = {
        let process_info = msg_class![env; NSProcessInfo processInfo];
        msg![env; process_info systemUptime]
    };

    let touches: id = msg_class![env;
        NSMutableSet
        allocWithZone:(MutVoidPtr::null())];

    for (finger_id, coords) in map {
        if env
            .framework_state
            .uikit
            .ui_touch
            .current_touches
            .contains_key(&finger_id)
        {
            log!(
                "Warning: New touch {:?} initiated but old one exists.",
                finger_id
            );
            return handle_touches_move(env, HashMap::from([(finger_id, coords)]));
        }

        let location = CGPoint {
            x: coords.0,
            y: coords.1,
        };
        let new_touch: id = msg_class![env; UITouch alloc];
        *env.objc.borrow_mut(new_touch) = UITouchHostObject {
            view: nil,
            window: nil,
            location,
            previous_location: location,
            start_location: location,
            timestamp,
            phase: UITouchPhaseBegan,
        };
        autorelease(env, new_touch);

        let _: () = msg![env; touches addObject:new_touch];
        env.framework_state
            .uikit
            .ui_touch
            .current_touches
            .insert(finger_id, new_touch);
        retain(env, new_touch);
    }

    let all_touches_set: id = msg_class![env; NSMutableSet
        allocWithZone:(MutVoidPtr::null())];
    let existing_touches: Vec<id> = env
        .framework_state
        .uikit
        .ui_touch
        .current_touches
        .values()
        .cloned()
        .collect();
    for touch in existing_touches {
        let _: () = msg![env; all_touches_set addObject:touch];
    }

    let event = ui_event::new_event(env, all_touches_set);
    autorelease(env, event);
    let views_with_existing_touches: HashSet<id> = env
        .framework_state
        .uikit
        .ui_touch
        .current_touches
        .values()
        .map(|&touch| env.objc.borrow::<UITouchHostObject>(touch).view)
        .collect();
    let mut view_touches: HashMap<id, id> = HashMap::new();
    let touches_arr: id = msg![env; touches allObjects];
    let touches_count: NSUInteger = msg![env;
        touches_arr count];

    for i in 0..touches_count {
        let touch: id = msg![env;
            touches_arr objectAtIndex:i];
        let &UITouchHostObject { location, .. } = env.objc.borrow(touch);

        let windows = env.framework_state.uikit.ui_view.ui_window.windows.clone();
        let found_window = windows.iter().rev().find_map(|&window| {
            let location_in_window: CGPoint = msg![env; window
                convertPoint:location fromWindow:nil];
            if msg![env; window pointInside:location_in_window withEvent:event] {
                Some((window, location_in_window))
            } else {
                None
            }
        });
        // SUPER HACK: Если окно отвергло касание, силой отправляем его в
        // главное окно!
        let Some((window, location_in_window)) = found_window.or_else(|| {
            windows.last().map(|&window| {
                let lx = location.x;
                let ly = location.y;
                log_dbg!(
                    "SUPER HACK: Forcing rejected touch at ({}, {}) into window",
                    lx,
                    ly
                );
                let loc: CGPoint = msg![env; window convertPoint:location fromWindow:nil];
                (window, loc)
            })
        }) else {
            let lx = location.x;
            let ly = location.y;
            log!(
                "Couldn't find ANY window for touch at ({}, {}), discarding",
                lx,
                ly
            );
            continue;
        };
        let mut view: id = msg![env; window hitTest:location_in_window withEvent:event];
        let hit_view = view;

        if view != nil {
            let view_class: crate::objc::Class = msg![env; view class];
            let class_name = env.objc.get_class_name(view_class).to_owned();

            if class_name == "MBProgressHUD" {
                log!("Touch hit MBProgressHUD; keeping HUD as touch target");
            }
        }

        if view == nil {
            log_dbg!("SUPER HACK: hitTest failed, looking for Cocos/GL target before using window");
            let cocos_target = touchhle_find_cocos_touch_target(env, window);
            view = if cocos_target != nil { cocos_target } else { window };
        } else if view == window {
            let cocos_target = touchhle_find_cocos_touch_target(env, window);
            if cocos_target != nil { view = cocos_target; }
        } else {
            let f: CGRect = msg![env;
                view frame];
            let view_class: crate::objc::Class = msg![env; view class];
            let class_name = env.objc.get_class_name(view_class).to_owned();
            let lx = location_in_window.x;
            let ly = location_in_window.y;
            log_dbg!(
                "Touch at ({}, {}) hit {} {:?} with frame {:?}",
                lx,
                ly,
                class_name,
                view,
                f,
            );
        }

        // Report where the first handful of touches actually land. When an app
        // stops responding to touch, the question is always "which view is
        // swallowing them" — a Crystal/OpenFeint style overlay on top of the
        // game will show up here as an unexpected class covering the screen.
        // This used to be `log_dbg!`-only, i.e. absent from release builds.
        {
            use std::sync::atomic::{AtomicUsize, Ordering};
            const LIMIT: usize = 12;
            static LOGGED: AtomicUsize = AtomicUsize::new(0);
            let count = LOGGED.fetch_add(1, Ordering::Relaxed);
            if count < LIMIT {
                let describe = |env: &mut Environment, v: id| -> String {
                    if v == nil {
                        return "nil".to_string();
                    }
                    let class: crate::objc::Class = msg![env; v class];
                    let name = env.objc.get_class_name(class).to_owned();
                    let frame: CGRect = msg![env; v frame];
                    let bounds: CGRect = msg![env; v bounds];
                    let hidden: bool = msg![env; v isHidden];
                    let interaction: bool = msg![env; v isUserInteractionEnabled];
                    // The window's hitTest skips subviews with alpha < 0.01 and
                    // tests the point against `bounds`, so both matter here.
                    let alpha: crate::frameworks::core_graphics::CGFloat =
                        msg![env; v alpha];
                    format!(
                        "{} {:?} frame={:?} bounds={:?} hidden={} userInteraction={} alpha={}",
                        name, v, frame, bounds, hidden, interaction, alpha
                    )
                };
                let hit_desc = describe(env, hit_view);
                let target_desc = if view == hit_view {
                    "(same)".to_string()
                } else {
                    describe(env, view)
                };
                // CGPoint is `repr(packed)`, so its fields cannot be borrowed
                // by `format_args!` — copy them out first.
                let (lx, ly) = (location_in_window.x, location_in_window.y);
                log!(
                    "Touch {}/{} at ({}, {}) in window: hitTest gave {} -> delivering to {}",
                    count + 1,
                    LIMIT,
                    lx,
                    ly,
                    hit_desc,
                    target_desc,
                );
            }
        }

        let is_multi_touch_enabled: bool = msg![env; view isMultipleTouchEnabled];
        if !is_multi_touch_enabled && !touchhle_cocos_should_allow_multitouch(env, view)
            && (view_touches.contains_key(&view) || views_with_existing_touches.contains(&view))
        {
            let stuck: Vec<FingerId> = env
                .framework_state
                .uikit
                .ui_touch
                .current_touches
                .iter()
                .filter(|(_, &t)| {
                    env.objc.borrow::<UITouchHostObject>(t).view == view && t != touch
                })
                .map(|(&fid, _)| fid)
                .collect();
            if !stuck.is_empty() {
                for fid in stuck {
                    if let Some(t) = env
                        .framework_state
                        .uikit
                        .ui_touch
                        .current_touches
                        .remove(&fid)
                    {
                        release(env, t);
                    }
                }
            } else {
                continue;
            }
        }

        if let Entry::Vacant(e) = view_touches.entry(view) {
            let s: id = msg_class![env;
                NSMutableSet
                allocWithZone:(MutVoidPtr::null())];
            e.insert(s);
        }
        let v_set: id = *view_touches.get(&view).unwrap();
        let _: () = msg![env; v_set addObject:touch];
        retain(env, view);
        retain(env, window);
        {
            let t_obj = env.objc.borrow_mut::<UITouchHostObject>(touch);
            t_obj.view = view;
            t_obj.window = window;
            t_obj.location = location;
        }
    }

    for (view, v_set) in view_touches {
        let _: () = msg![env;
            view touchesBegan:v_set withEvent:event];
        super::ui_gesture_recognizer::touches_began(env, view, v_set);
        touchhle_send_cocos_touch_aliases_to_chain(env, view, "began", v_set, event);
    }
    release(env, pool);
}

fn handle_touches_move(env: &mut Environment, map: HashMap<FingerId, Coords>) {
    let pool: id = msg_class![env;
        NSAutoreleasePool new];
    let timestamp: NSTimeInterval = {
        let pi = msg_class![env; NSProcessInfo processInfo];
        msg![env; pi systemUptime]
    };

    let mut view_touches: HashMap<id, id> = HashMap::new();
    for (finger_id, coords) in map {
        let Some(&touch) = env
            .framework_state
            .uikit
            .ui_touch
            .current_touches
            .get(&finger_id)
        else {
            continue;
        };
        let location = CGPoint {
            x: coords.0,
            y: coords.1,
        };
        let view = env.objc.borrow::<UITouchHostObject>(touch).view;
        let host = env.objc.borrow_mut::<UITouchHostObject>(touch);
        if host.location == location {
            continue;
        }
        host.previous_location = host.location;
        host.location = location;
        host.timestamp = timestamp;
        host.phase = UITouchPhaseMoved;

        if let Entry::Vacant(e) = view_touches.entry(view) {
            let s: id = msg_class![env;
                NSMutableSet
                allocWithZone:(MutVoidPtr::null())];
            e.insert(s);
        }
        let v_set: id = *view_touches.get(&view).unwrap();
        let _: () = msg![env; v_set addObject:touch];
    }

    let all_touches_set: id = msg_class![env; NSMutableSet
        allocWithZone:(MutVoidPtr::null())];
    let existing: Vec<id> = env
        .framework_state
        .uikit
        .ui_touch
        .current_touches
        .values()
        .cloned()
        .collect();
    for t in existing {
        let _: () = msg![env; all_touches_set addObject:t];
    }

    let event = ui_event::new_event(env, all_touches_set);
    autorelease(env, event);
    for (view, v_set) in view_touches {
        let _: () = msg![env;
            view touchesMoved:v_set withEvent:event];
        super::ui_gesture_recognizer::touches_moved(env, view, v_set);
        touchhle_send_cocos_touch_aliases_to_chain(env, view, "moved", v_set, event);
    }
    release(env, pool);
}


fn ultrahle_minionjump_drain_pending_callback(env: &mut Environment, select_only: bool) {
    if !matches!(
        env.bundle.bundle_identifier(),
        "com.apprisetec9.minionjump" | "com.risinghighapps.kingdomprincepro"
    ) {
        return;
    }

    let Some(target_raw) = std::env::var("ULTRAHLE_MINIONJUMP_PENDING_TARGET")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };

    let Some(sel_raw) = std::env::var("ULTRAHLE_MINIONJUMP_PENDING_SEL")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };

    let sender_raw = std::env::var("ULTRAHLE_MINIONJUMP_PENDING_SENDER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);

    let callback_name = std::env::var("ULTRAHLE_MINIONJUMP_PENDING_CALLBACK")
        .unwrap_or_else(|_| "<unknown>".to_string());

    let stage =
        std::env::var("ULTRAHLE_MINIONJUMP_PENDING_STAGE").unwrap_or_else(|_| "0".to_string());

    let is_level_select = callback_name == "selectLVAction:";
    if select_only != is_level_select {
        return;
    }

    std::env::remove_var("ULTRAHLE_MINIONJUMP_PENDING_TARGET");
    std::env::remove_var("ULTRAHLE_MINIONJUMP_PENDING_SEL");
    std::env::remove_var("ULTRAHLE_MINIONJUMP_PENDING_SENDER");
    std::env::remove_var("ULTRAHLE_MINIONJUMP_PENDING_CALLBACK");
    std::env::remove_var("ULTRAHLE_MINIONJUMP_PENDING_STAGE");

    if target_raw == 0 || sel_raw == 0 {
        return;
    }

    let target_id = id::from_bits(target_raw);
    let sender = id::from_bits(sender_raw);
    let callback_sel_ptr = crate::mem::ConstPtr::<u8>::from_bits(sel_raw);
    let callback_sel: crate::objc::SEL = unsafe { std::mem::transmute(callback_sel_ptr) };

    log!(
        "UltraHLE MinionJump: draining pending callback selector={} target={:?} sender={:?} stage={} select_only={}",
        callback_name,
        target_id,
        sender,
        stage,
        select_only
    );

    if callback_name.ends_with(':') {
        let _: () = msg_send_no_type_checking(env, (target_id, callback_sel, sender));
    } else {
        let _: () = msg_send_no_type_checking(env, (target_id, callback_sel));
    }
}

fn handle_touches_up(env: &mut Environment, map: HashMap<FingerId, Coords>) {
    let pool: id = msg_class![env;
        NSAutoreleasePool new];
    let timestamp: NSTimeInterval = {
        let pi = msg_class![env; NSProcessInfo processInfo];
        msg![env; pi systemUptime]
    };

    let touches: id = msg_class![env;
        NSMutableSet
        allocWithZone:(MutVoidPtr::null())];
    let all_touches_set: id = msg_class![env;
        NSMutableSet
        allocWithZone:(MutVoidPtr::null())];
    let existing: Vec<id> = env
        .framework_state
        .uikit
        .ui_touch
        .current_touches
        .values()
        .cloned()
        .collect();
    for t in existing {
        let _: () = msg![env; all_touches_set addObject:t];
    }

    let mut view_touches: HashMap<id, id> = HashMap::new();
    for (finger_id, coords) in map {
        let Some(&touch) = env
            .framework_state
            .uikit
            .ui_touch
            .current_touches
            .get(&finger_id)
        else {
            continue;
        };
        let location = CGPoint {
            x: coords.0,
            y: coords.1,
        };
        let view = env.objc.borrow::<UITouchHostObject>(touch).view;
        let start_location = env.objc.borrow::<UITouchHostObject>(touch).start_location;
        {
            let host = env.objc.borrow_mut::<UITouchHostObject>(touch);
            host.previous_location = host.location;
            host.location = location;
            host.timestamp = timestamp;
            host.phase = UITouchPhaseEnded;
        }

        let _: () = msg![env;
            touches addObject:touch];

        if let Entry::Vacant(e) = view_touches.entry(view) {
            let s: id = msg_class![env;
                NSMutableSet
                allocWithZone:(MutVoidPtr::null())];
            e.insert(s);
        }
        let v_set: id = *view_touches.get(&view).unwrap();
        let _: () = msg![env; v_set addObject:touch];
        env.framework_state
            .uikit
            .ui_touch
            .current_touches
            .remove(&finger_id);
        release(env, touch);
    }

    let event = ui_event::new_event(env, all_touches_set);
    autorelease(env, event);
    for (view, v_set) in view_touches {
        let _: () = msg![env;
            view touchesEnded:v_set withEvent:event];
        super::ui_gesture_recognizer::touches_ended(env, view, v_set);
        touchhle_send_cocos_touch_aliases_to_chain(env, view, "ended", v_set, event);
    }

    // ULTRAHLE_MINIONJUMP_DRAIN_SELECT_BEGIN
    ultrahle_minionjump_drain_pending_callback(env, true);
    // ULTRAHLE_MINIONJUMP_DRAIN_SELECT_END
    // ULTRAHLE_MINIONJUMP_DRAIN_POST_BEGIN
    ultrahle_minionjump_drain_pending_callback(env, false);
    // ULTRAHLE_MINIONJUMP_DRAIN_POST_END

    release(env, pool);
}
