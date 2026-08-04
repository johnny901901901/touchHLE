/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! OpenGL ES and EAGL.
//!
//! This module is specific to OpenGL ES's role as a part of the iPhone OS API
//! surface. See [crate::gles] for other uses and a discussion of the broader
//! topic.

mod eagl;
mod gles_guest;

pub(crate) use eagl::present_composited_frame;

use touchHLE_gl_bindings::gles11::types::{GLenum, GLint, GLsizei, GLuint};

use crate::mem::ConstPtr;

pub const DYLIB: crate::dyld::HostDylib = crate::dyld::HostDylib {
    path: "/System/Library/Frameworks/OpenGLES.framework/OpenGLES",
    aliases: &[],
    class_exports: &[eagl::CLASSES],
    constant_exports: &[eagl::CONSTANTS],
    function_exports: &[gles_guest::FUNCTIONS, eagl::FUNCTIONS],
};

#[derive(Default)]
pub struct State {
    /// Current EAGLContext for each thread
    current_ctxs: std::collections::HashMap<crate::ThreadId, Option<crate::objc::id>>,
    /// `glGetString()` results cache, keyed by `(is_es2, name)` so that an
    /// ES 2.0 context does not return the ES 1.1 `OpenGL ES-CM 1.1` version
    /// string (Bad Piggies / Unity 3.5 checks the version string to decide
    /// which renderer code path to use — mismatching the API leaves Unity in
    /// a half-initialised ES 2.0 path that emits torn / overlapping frames).
    strings_cache: std::collections::HashMap<(bool, GLenum), ConstPtr<u8>>,
    /// The framebuffer object the guest currently has bound, per thread.
    bound_framebuffers: std::collections::HashMap<crate::ThreadId, GLuint>,
    /// Framebuffer objects whose colour attachment is a *texture*.
    ///
    /// The scale hack enlarges renderbuffers (both the EAGL drawable's and any
    /// the app allocates via `glRenderbufferStorage`), so scaling a viewport
    /// aimed at those is correct. Textures are never enlarged, so a viewport
    /// aimed at a texture-backed framebuffer must be left alone — otherwise the
    /// app draws `scale_hack` times too large into an unscaled target and you
    /// see a magnified bottom-left corner (Flappy Bird renders through a
    /// 288x512 texture FBO and did exactly that).
    texture_backed_framebuffers: std::collections::HashSet<GLuint>,
    /// The last viewport the guest asked for, in guest (unscaled) co-ordinates.
    ///
    /// The GL viewport is global state rather than per-framebuffer, and apps
    /// are free to call `glViewport` *before* binding the framebuffer they mean
    /// it for (Flappy Bird does). Whether the scale hack applies therefore
    /// cannot be decided when `glViewport` is called — we remember the request
    /// and re-apply it with the right scaling whenever the binding changes.
    guest_viewports: std::collections::HashMap<crate::ThreadId, (GLint, GLint, GLsizei, GLsizei)>,
}
impl State {
    fn current_ctx_for_thread(&mut self, thread: crate::ThreadId) -> &mut Option<crate::objc::id> {
        self.current_ctxs.entry(thread).or_insert(None);
        self.current_ctxs.get_mut(&thread).unwrap()
    }

    /// Whether this thread has a current `EAGLContext`, i.e. whether the guest
    /// has any GL context of its own that could present a frame.
    pub fn thread_has_current_ctx(&mut self, thread: crate::ThreadId) -> bool {
        self.current_ctx_for_thread(thread).is_some()
    }

    pub fn set_bound_framebuffer(&mut self, thread: crate::ThreadId, framebuffer: GLuint) {
        self.bound_framebuffers.insert(thread, framebuffer);
    }

    /// Record what kind of colour attachment a framebuffer was given, so
    /// `glViewport` can tell whether the scale hack applies to it.
    pub fn set_colour_attachment_is_texture(&mut self, thread: crate::ThreadId, is_texture: bool) {
        let framebuffer = self
            .bound_framebuffers
            .get(&thread)
            .copied()
            .unwrap_or_default();
        // Framebuffer 0 is the window/drawable itself and is always scaled.
        if framebuffer == 0 {
            return;
        }
        if is_texture {
            self.texture_backed_framebuffers.insert(framebuffer);
        } else {
            self.texture_backed_framebuffers.remove(&framebuffer);
        }
    }

    /// Whether the scale hack should be applied to `glViewport` right now.
    ///
    /// Defaults to `true` (the historical behaviour) unless we positively know
    /// the bound framebuffer is texture-backed, so anything we failed to track
    /// keeps working exactly as before.
    pub fn set_guest_viewport(
        &mut self,
        thread: crate::ThreadId,
        viewport: (GLint, GLint, GLsizei, GLsizei),
    ) {
        self.guest_viewports.insert(thread, viewport);
    }

    pub fn guest_viewport(
        &self,
        thread: crate::ThreadId,
    ) -> Option<(GLint, GLint, GLsizei, GLsizei)> {
        self.guest_viewports.get(&thread).copied()
    }

    pub fn scale_hack_applies_to_viewport(&self, thread: crate::ThreadId) -> bool {
        let framebuffer = self
            .bound_framebuffers
            .get(&thread)
            .copied()
            .unwrap_or_default();
        !self.texture_backed_framebuffers.contains(&framebuffer)
    }
}

/// Bind the calling thread's current EAGL context to the host window and
/// return a [`crate::gles::GLES`] wrapper for it.
///
/// Returns `None` when the calling thread has no current EAGL context (i.e.
/// `[EAGLContext setCurrentContext:]` was never called with a non-`nil`
/// argument on this thread, or it was reset to `nil`). Apple's
/// implementation simply ignores GL calls in that state instead of crashing
/// the process — see Apple's "OpenGL ES Programming Guide for iOS",
/// section "Context Lifecycle Management": *"If the current context is
/// `nil`, OpenGL ES commands silently fail"*.
///
/// Previously this used `.unwrap()` on the thread-local context, which
/// turned a perfectly recoverable guest-side state error into an emulator
/// panic. HyperHLE log #5 reproduced this when a worker thread issued GL
/// calls between `[EAGLContext setCurrentContext:nil]` and a later
/// `setCurrentContext:` on the main thread.
fn sync_context<'objc, 'win: 'objc>(
    state: &mut State,
    objc: &'objc mut crate::objc::ObjC,
    window: &'win mut crate::window::Window,
    current_thread: crate::ThreadId,
) -> Option<Box<dyn crate::gles::GLES + 'objc>> {
    let gles_ctx = get_thread_context(state, objc, current_thread)?;
    Some(gles_ctx.make_current(window))
}

/// Look up the current [`crate::gles::GLESContext`] for the given thread.
///
/// Returns `None` when the thread has no current EAGL context bound, or
/// when that context was created without a backing GLES driver (e.g.
/// headless / unsupported `EAGLRenderingAPI`). Both are recoverable —
/// callers should skip the GL operation rather than abort the emulator.
fn get_thread_context<'objc>(
    state: &mut State,
    objc: &'objc mut crate::objc::ObjC,
    current_thread: crate::ThreadId,
) -> Option<&'objc mut dyn crate::gles::GLESContext> {
    // Note: `Box<dyn GLESContext>` carries the default `'static` trait-object
    // bound, so the returned mutable reference re-borrows from the
    // (`'objc`) ObjC store without imposing a tighter lifetime.
    let current_ctx = (*state.current_ctx_for_thread(current_thread))?;
    let host_obj = objc.borrow_mut::<eagl::EAGLContextHostObject>(current_ctx);
    let gles_ctx: &mut (dyn crate::gles::GLESContext + 'static) =
        host_obj.gles_ctx.as_deref_mut()?;
    Some(gles_ctx as &mut dyn crate::gles::GLESContext)
}
