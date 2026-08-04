/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Handling of Objective-C classes and metaclasses.
//!
//! Note that metaclasses are just a special case of classes.
//!
//! Resources:
//! - [[objc explain]: Classes and metaclasses](http://www.sealiesoftware.com/blog/archive/2009/04/14/objc_explain_Classes_and_metaclasses.html), especially [the PDF diagram](http://www.sealiesoftware.com/blog/class%20diagram.pdf)

use super::{
    id, ivar_list_t, method_list_t, nil, objc_object, AnyHostObject, HostIMP, HostObject, ObjC,
    IMP, SEL,
};
use crate::bundle;
use crate::mach_o::MachO;
use crate::mem::{
    guest_size_of, ConstPtr, ConstVoidPtr, GuestUSize, Mem, MutVoidPtr, Ptr, SafeRead,
};
use std::collections::{HashMap, VecDeque};

/// Generic pointer to an Objective-C class or metaclass.
///
/// The name is standard Objective-C.
///
/// Apple's runtime has a `objc_classes` definition similar to `objc_object`.
/// We could do the same thing here, but it doesn't seem worth it, as we can't
/// get the same unidirectional type safety.
pub type Class = id;

/// Our internal representation of a class, e.g. this is where `objc_msgSend`
/// will look up method implementations.
///
/// Note: `superclass` can be `nil`!
#[derive(Default)]
pub(super) struct ClassHostObject {
    pub(super) name: String,
    pub(super) is_metaclass: bool,
    pub(super) superclass: Class,
    pub(super) methods: HashMap<SEL, IMP>,
    pub(super) guest_method_signatures: HashMap<SEL, ConstPtr<u8>>,
    /// Maps ivar name to a tuple of an offset (as pointer) and an alignment.
    /// (Alignment is used during ivar reconciliation.)
    pub(super) ivars: HashMap<String, (ConstPtr<GuestUSize>, u32)>,
    /// Maps declared @property name to the guest-memory pointer of its
    /// `property_t` entry (as read from the binary's property list).
    /// This is what `class_getProperty` queries.
    pub(super) properties: HashMap<String, ConstVoidPtr>,
    /// Offset into the allocated memory for the object where the ivars of
    /// instances of this class or metaclass (respectively: normal objects or
    /// classes) should live. This is always >= the value in the superclass.
    pub(super) instance_start: GuestUSize,
    /// Size of the allocated memory for instances of this class or metaclass.
    /// This is always >= the value in the superclass.
    pub(super) instance_size: GuestUSize,
}
impl HostObject for ClassHostObject {}

/// Placeholder object for classes and metaclasses referenced by the app that
/// we don't have an implementation for.
///
/// This lets us delay errors about missing implementations until the first
/// time the app actually uses them (e.g. when a message is sent).
pub(super) struct UnimplementedClass {
    pub(super) name: String,
    pub(super) is_metaclass: bool,
}
impl HostObject for UnimplementedClass {}

/// Substitute object for classes and metaclasses from the guest app that we do
/// not want to support (see [substitute_classes]).
///
/// Messages sent to this class will behave as if messaging [nil].
pub(super) struct FakeClass {
    pub(super) name: String,
    pub(super) is_metaclass: bool,
}
impl HostObject for FakeClass {}

/// The layout of a class in an app binary.
///
/// The name, field names and field layout are based on what Ghidra outputs.
#[repr(C, packed)]
#[allow(dead_code)]
struct class_t {
    isa: Class, // note that this matches objc_object
    superclass: Class,
    _cache: ConstVoidPtr,
    _vtable: ConstVoidPtr,
    data: ConstPtr<class_rw_t>,
}
unsafe impl SafeRead for class_t {}

/// The layout of the main class data in an app binary.
///
/// The name, field names and field layout are based on what Ghidra's output.
#[repr(C, packed)]
#[allow(dead_code)]
struct class_rw_t {
    _flags: u32,
    instance_start: GuestUSize,
    instance_size: GuestUSize,
    _reserved: u32,
    name: ConstPtr<u8>,
    base_methods: ConstPtr<method_list_t>,
    _base_protocols: ConstVoidPtr, // protocol list (TODO)
    ivars: ConstPtr<ivar_list_t>,
    _weak_ivar_layout: u32,
    _base_properties: ConstVoidPtr, // property list (TODO)
}
unsafe impl SafeRead for class_rw_t {}

/// The layout of a category in an app binary.
///
/// The name, field names and field layout are based on what Ghidra outputs.
#[repr(C, packed)]
struct category_t {
    name: ConstPtr<u8>,
    class: Class,
    instance_methods: ConstPtr<method_list_t>,
    class_methods: ConstPtr<method_list_t>,
    _protocols: ConstVoidPtr,     // protocol list (TODO)
    _property_list: ConstVoidPtr, // property list (TODO)
}
unsafe impl SafeRead for category_t {}

/// A template for a class defined with [objc_classes].
///
/// Host implementations of libraries can use these to expose classes to the
/// application. The runtime will create the actual class ([ClassHostObject]
/// etc) from the template on-demand. See also [ClassExports].
pub struct ClassTemplate {
    pub name: &'static str,
    pub superclass: Option<&'static str>,
    pub class_methods: &'static [(&'static str, &'static dyn HostIMP)],
    pub instance_methods: &'static [(&'static str, &'static dyn HostIMP)],
}

/// Type for lists of classes exported by host implementations of frameworks.
///
/// Each module that wants to expose functions to guest code should export a
/// constant using this type. See [objc_classes] for an example.
///
/// All the constants like this can then be collected into a
/// [crate::dyld::HostDylib].
///
/// The strings are the class names.
///
/// See also [crate::dyld::ConstantExports] and [crate::dyld::FunctionExports].
pub type ClassExports = &'static [(&'static str, ClassTemplate)];

#[doc(hidden)]
#[macro_export]
macro_rules! _objc_superclass {
    (: $name:ident) => {
        Some(stringify!($name))
    };
    () => {
        None
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! _objc_method {
    (
        $env:ident,
        $this:ident,
        $_cmd:ident,
        $cmd_name:ident,
        $retty:ty,
        $block:block
        $(, $ty:ty, $arg:ident)*
        $(, ...$va_arg:ident: $va_type:ty)?
    ) => {
        // The closure must be explicitly casted because a bare closure defaults
        // to a different type than a pure fn pointer, which is the type that
        // HostIMP and CallFromGuest are implemented on.
        &((|
            #[allow(unused_variables)]
            $env: &mut $crate::Environment,
            #[allow(unused_variables)]
            $this: $crate::objc::id,
            #[allow(unused_variables)]
            $_cmd: $crate::objc::SEL,
            $($arg: $ty,)*
            $(#[allow(unused_mut)] mut $va_arg: $va_type,)?
        | -> $retty {
            const _OBJC_CURRENT_SELECTOR: &str = stringify!($cmd_name);
            $block
        }) as fn(
            &mut $crate::Environment,
            $crate::objc::id,
            $crate::objc::SEL,
            $($ty,)*
            $($va_type,)?
        ) -> $retty)
    }
}

/// Macro for creating a list of [ClassTemplate]s (i.e. [ClassExports]).
/// It imitates the Objective-C class definition syntax.
#[macro_export]
macro_rules! objc_classes {
    {
        ($env:ident, $this:ident, $_cmd:ident);
        $(
            @implementation $class_name:ident $(: $superclass_name:ident)?
            $( + ($cm_type:ty) $cm_name:ident $(:($cm_type1:ty) $cm_arg1:ident $($($cm_namen:ident)?:($cm_typen:ty) $cm_argn:ident)*)?
                              $(, ...$cm_va_arg:ident)?
                 $cm_block:block )*
            $( - ($im_type:ty) $im_name:ident $(:($im_type1:ty)
$im_arg1:ident $($($im_namen:ident)?:($im_typen:ty) $im_argn:ident)*)?
                              $(, ...$im_va_arg:ident)?
                 $im_block:block )*
            @end
        )+
    } => {
        &[
            $({
                const _OBJC_CURRENT_CLASS: &str = stringify!($class_name);
                (_OBJC_CURRENT_CLASS, $crate::objc::ClassTemplate {
                    name: _OBJC_CURRENT_CLASS,
                    superclass: $crate::_objc_superclass!($(: $superclass_name)?),
                    class_methods: &[
                        $(
                            (
                                $crate::objc::selector!(
                                    $(($cm_type1);)?
                                    $cm_name
                                    $($(, $($cm_namen)?)*)?
                                ),
                                $crate::_objc_method!(
                                    $env,
                                    $this,
                                    $_cmd,
                                    $cm_name,
                                    $cm_type,
                                    { $cm_block }
                                    $(, $cm_type1, $cm_arg1 $(, $cm_typen, $cm_argn)*)?
                                    $(, ...$cm_va_arg: $crate::abi::DotDotDot)?
                                )
                            )
                        ),*
                    ],
                    instance_methods: &[
                        $(
                            (
                                $crate::objc::selector!(
                                    $(($im_type1);)?
                                    $im_name
                                    $($(, $($im_namen)?)*)?
                                ),
                                $crate::_objc_method!(
                                    $env,
                                    $this,
                                    $_cmd,
                                    $im_name,
                                    $im_type,
                                    { $im_block }
                                    $(, $im_type1, $im_arg1 $(, $im_typen, $im_argn)*)?
                                    $(, ...$im_va_arg: $crate::abi::DotDotDot)?
                                )
                            )
                        ),*
                    ],
                })
            }),+
        ]
    }
}
pub use crate::objc_classes;
// #[macro_export] is weird...

impl ClassHostObject {
    fn from_template(
        template: &ClassTemplate,
        is_metaclass: bool,
        superclass: Class,
        objc: &ObjC,
    ) -> Self {
        // For our host implementations we store all data in host objects, so
        // there are no ivars and the size is always just the isa pointer.
        // This is true for both classes and normal objects.
        let size = guest_size_of::<objc_object>();
        ClassHostObject {
            name: template.name.to_string(),
            is_metaclass,
            superclass,
            methods: HashMap::from_iter(
                (if is_metaclass {
                    template.class_methods
                } else {
                    template.instance_methods
                })
                .iter()
                .map(|&(name, host_imp)| {
                    // The selector should already have been registered by
                    // [ObjC::register_host_selectors], so we can panic
                    // if it hasn't been.
                    (objc.selectors[name], IMP::Host(host_imp))
                }),
            ),
            guest_method_signatures: HashMap::default(),
            // maybe this should be 0 for NSObject? does it matter?
            instance_start: size,
            instance_size: size,
            ivars: HashMap::default(),
            properties: HashMap::default(),
        }
    }

    fn from_bin(class: Class, is_metaclass: bool, mem: &Mem, objc: &mut ObjC) -> Self {
        let class_t {
            superclass, data, ..
        } = mem.read(class.cast());
        let class_rw_t {
            instance_start,
            instance_size,
            name,
            base_methods,
            ivars,
            _base_properties,
            ..
        } = mem.read(data);
        // Apple's Objective-C 2.0 runtime stores `class_rw_t.name` as a
        // pointer to a NUL-terminated UTF-8 C string in `__objc_classname`
        // (see <objc/runtime.h> and `class_getName`). Some real-world
        // binaries — especially FairPlay-encrypted IPAs that were only
        // partially decrypted by tools like Clutch, or Mach-O files whose
        // `__TEXT` segment was clobbered — end up with `name` pointing at
        // bytes that aren't valid UTF-8. Previously we panicked with
        // `cstr_at_utf8().unwrap()`; mirror the real runtime instead, which
        // logs the corruption and falls back to a synthetic name derived
        // from the class pointer so the rest of the runtime stays usable.
        let name = match mem.cstr_at_utf8(name) {
            Ok(s) => s.to_string(),
            Err(_) => {
                log!(
                    "Warning: ClassHostObject::from_bin: class at {:?} has an unreadable (non-UTF-8) name at {:?}; using a synthetic placeholder.",
                    class,
                    name,
                );
                format!(
                    "_touchHLE_UnreadableClass_{:08x}",
                    class.to_bits()
                )
            }
        };

        let mut host_object = ClassHostObject {
            name,
            is_metaclass,
            superclass,
            methods: HashMap::new(),
            guest_method_signatures: HashMap::new(),
            instance_start,
            instance_size,
            ivars: HashMap::new(),
            properties: HashMap::new(),
        };
        if !base_methods.is_null() {
            host_object.add_methods_from_bin(base_methods, mem, objc);
        }

        if !ivars.is_null() {
            host_object.add_ivars_from_bin(ivars, mem);
        }

        if !_base_properties.is_null() {
            host_object.add_properties_from_bin(_base_properties, mem);
        }

        host_object
    }

    // See methods.rs for binary method parsing
}

/// Decide whether a certain class/metaclass pair from the guest app should use
/// fake class host objects and return the substitutions if so.
fn substitute_classes(
    bundle: &bundle::Bundle,
    mem: &Mem,
    class: Class,
    metaclass: Class,
) -> Option<(Box<FakeClass>, Box<FakeClass>)> {
    let class_t { data, .. } = mem.read(class.cast());
    let class_rw_t { name, .. } = mem.read(data);
    // If the class name bytes aren't valid UTF-8 the metadata is almost
    // certainly corrupt / still-encrypted (see comment in
    // `ClassHostObject::from_bin`). Don't try to fake-substitute such a
    // class — let the normal loader path log a warning and create a stub
    // host object instead of panicking here.
    let Ok(name) = mem.cstr_at_utf8(name) else {
        return None;
    };
    // Chillingo's Crystal social SDK builds its UI from theme files (`.ctd`)
    // that it used to download from Chillingo's servers into `Documents`.
    // Those servers are long gone, so the theme is never there, and
    // `CCModalViewController` puts up its launch splash as an empty
    // full-screen view that never finishes building and never dismisses. It
    // covers the game and swallows every touch - Sword of Fargoal for iPad
    // renders and plays its music underneath, completely uncontrollable.
    // Faking the class makes `alloc` return nil, so no splash is built at all.
    //
    // Matched by exact name rather than by a `CC` prefix like the SDKs below:
    // Cocos2D uses `CC` for its entire API, and faking that would break every
    // Cocos2D game.
    const CRYSTAL_SDK_CLASSES: &[&str] = &["CCModalViewController"];

    // Substitute classes that seem to be from various third-party advertising
    // or social network SDKs.
    if !(CRYSTAL_SDK_CLASSES.contains(&name)
        || name.starts_with("AdMob")
        || name.starts_with("AltAds")
        || name.starts_with("Mobclix")
        || name.starts_with("FB") // Facebook
        || name.starts_with("Flurry")
        || name.starts_with("OpenFeint")
        || name.starts_with("Tapjoy")
        || name.starts_with("UA")
        || name.starts_with("GAD")
        || name.starts_with("RevMob")
        || name.starts_with("iSimulate")
        || name.starts_with("ALSdk") // AppLovin SDK
        || name.starts_with("ALAd")  // AppLovin ads
        || name.starts_with("ALEvent") // AppLovin events
        || name.starts_with("ALIncentivized") // AppLovin incentivized ads
        || name.starts_with("ALTargeting") // AppLovin targeting
        || name.starts_with("ALPrivacy") // AppLovin privacy settings
    )
    // <-- ДОБАВЛЕНО ЗДЕСЬ
    {
        // TODO : try to remove when sqlite3 is supported.
        if (bundle.bundle_identifier() == "com.chillingo.defenderchronicles")
            && (name == "OFHighScoreService")
        {
            log!("Applying game-specific hack for Defender Chronicles: skipping OpenFeint online high score system.");
        } else {
            return None;
        }
    }

    {
        let class_t { data, .. } = mem.read(metaclass.cast());
        let class_rw_t {
            name: metaclass_name,
            ..
        } = mem.read(data);
        // Same defensive handling as the class-name read above. In Apple's
        // runtime, `class_getName(cls) == class_getName(object_getClass(cls))`,
        // but a corrupt binary can violate that invariant; an `assert!`
        // here would crash the whole emulator over what is really just
        // malformed input. Skip the substitution instead.
        let Ok(metaclass_name) = mem.cstr_at_utf8(metaclass_name) else {
            return None;
        };
        if name != metaclass_name {
            log!(
                "Warning: substitute_classes: class name {:?} != metaclass name {:?}; skipping substitution.",
                name,
                metaclass_name,
            );
            return None;
        }
    }

    log!(
        "Note: substituting fake class for {} to improve compatibility",
        name
    );
    let class_host_object = Box::new(FakeClass {
        name: name.to_string(),
        is_metaclass: false,
    });
    let metaclass_host_object = Box::new(FakeClass {
        name: name.to_string(),
        is_metaclass: true,
    });
    Some((class_host_object, metaclass_host_object))
}

impl ObjC {
    /// Iterator over all known classes and their names.
    pub fn all_classes(&self) -> impl Iterator<Item = (&String, &Class)> {
        self.classes.iter()
    }

    fn get_class(&self, name: &str, is_metaclass: bool, mem: &Mem) -> Option<Class> {
        let class = self.classes.get(name).copied()?;
        Some(if is_metaclass {
            Self::read_isa(class, mem)
        } else {
            class
        })
    }

    fn find_template(name: &str) -> Option<&'static ClassTemplate> {
        crate::dyld::search_host_dylibs(|dylib| dylib.class_exports, name)
            .map(|&(_name, ref template)| template)
    }

    /// For use by [crate::dyld]: get the class or metaclass referenced by an
    /// external relocation in the app binary. If we don't have an
    /// implementation of the class, a placeholder is used.
    pub fn link_class(&mut self, name: &str, is_metaclass: bool, mem: &mut Mem) -> Class {
        self.link_class_inner(name, is_metaclass, mem, true)
    }

    /// For use by host functions: get a particular class. If we don't have an
    /// implementation of the class, panic.
    pub fn get_known_class(&mut self, name: &str, mem: &mut Mem) -> Class {
        self.link_class_inner(name, /* is_metaclass: */ false, mem, false)
    }

    /// For use by host functions when the caller wants to handle the
    /// "class does not exist" case gracefully (e.g. UIClassSwapper falling
    /// back to the NIB's original class when the app-defined custom class is
    /// missing). Returns `Some(class)` if the class is already registered or
    /// a host-side template exists for it; otherwise returns `None` instead
    /// of panicking or installing a placeholder.
    pub fn try_get_known_class(&mut self, name: &str, mem: &mut Mem) -> Option<Class> {
        if let Some(class) = self.get_class(name, /* is_metaclass: */ false, mem) {
            return Some(class);
        }
        if Self::find_template(name).is_some() {
            return Some(self.link_class(name, /* is_metaclass: */ false, mem));
        }
        None
    }

    /// Find a registered class that *declares* the given instance method
    /// (by selector name), or `None` if there is no such class.
    ///
    /// This scans every registered class but only matches a class that
    /// declares the method *itself* — the superclass chain is intentionally
    /// not walked, so unrelated classes that merely inherit the method cannot
    /// match. Classes without a real Objective-C method table (substituted
    /// SDK classes, unimplemented-class placeholders, etc.) are skipped.
    ///
    /// This is used by `UIApplicationMain` to recover the application delegate
    /// when the app neither wires it up through its main nib nor passes a
    /// resolvable delegate class name (some binaries reach `UIApplicationMain`
    /// with a nil/empty delegate class name, which otherwise leaves the app
    /// with no delegate and frozen on its launch image, because
    /// `application:didFinishLaunchingWithOptions:` is never delivered).
    ///
    /// When several classes match, selection is deterministic and prefers
    /// names that look like an application delegate (e.g. `AppDelegate`,
    /// `AppController`), so an unrelated class that happens to implement the
    /// selector does not shadow the real delegate.
    pub fn class_declaring_instance_method(&self, sel_name: &str) -> Option<Class> {
        let sel = self.lookup_selector(sel_name)?;
        // (preference rank, class name, class); lower rank wins, ties broken by
        // name so the result does not depend on `HashMap` iteration order.
        let mut candidates: Vec<(u8, &str, Class)> = Vec::new();
        for (name, &class) in self.classes.iter() {
            let Some(host_object) = self.get_host_object(class) else {
                continue;
            };
            let Some(class_host_object) =
                host_object.as_any().downcast_ref::<ClassHostObject>()
            else {
                continue;
            };
            if class_host_object.is_metaclass || !class_host_object.methods.contains_key(&sel) {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            let rank = if lower.ends_with("appdelegate") || lower.ends_with("appcontroller") {
                0
            } else if lower.contains("delegate") {
                1
            } else {
                2
            };
            candidates.push((rank, name.as_str(), class));
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        candidates.first().map(|&(_, _, class)| class)
    }

    fn link_class_inner(
        &mut self,
        name: &str,
        is_metaclass: bool,
        mem: &mut Mem,
        use_placeholder: bool,
    ) -> Class {
        // The class and metaclass must be created together and tracked
        // together, so even though this function only returns one pointer, it
        // must create both. The function must not care whether the metaclass
        // is requested first, or if the class is requested first.
        if let Some(class) = self.get_class(name, is_metaclass, mem) {
            return class;
        };

        // An empty or otherwise garbage class name never corresponds to a real
        // class. The real Objective-C runtime returns nil for such lookups
        // (e.g. `objc_getClass("") == nil`). These names show up in touchHLE
        // when a guest reads a class name (or a `Class` pointer) from
        // uninitialised/corrupt memory — e.g. a NULL-page read returning all
        // zeroes decodes to the empty string. Registering a placeholder class
        // under such a name is actively harmful: it (a) pollutes the class
        // table so every subsequent lookup of that name returns a non-nil
        // placeholder, breaking the documented "class not loaded -> nil"
        // contract apps rely on for feature detection, and (b) leads the guest
        // to send messages (e.g. `+new`) to a bogus class, which here was
        // observed to spin in a tight loop printing
        // `Class "" ... is unimplemented. Call to class method "new".`
        // forever. Refuse to create/register such classes and return nil so
        // the guest sees the same "no such class" result a real device would.
        if name.is_empty() || name.chars().any(|c| c.is_control() || !c.is_ascii()) {
            log_dbg!(
                "Refusing to link class with empty/garbage name {:?}; returning nil.",
                name
            );
            return nil;
        }

        let class_host_object: Box<dyn AnyHostObject>;
        let metaclass_host_object: Box<dyn AnyHostObject>;
        if let Some(template) = Self::find_template(name) {
            // We have a template (host implementation) for this class, use it.
            if let Some(superclass_name) = template.superclass {
                // В реальном Objective-C рантайме классы могут загружаться
                // динамически.
                // Вместо жесткого падения (assert!), если шаблон суперкласса не
                // найден,
                // мы просто логируем это и позволяем рантайму легально создать
                // UnimplementedClass (или FakeClass) через механизм link_class.
                // Это честное поведение динамического линкера: иерархия
                // сохраняется!
                if Self::find_template(superclass_name).is_none() {
                    log!("Warning: Host class template {} inherits from missing {}, falling back to dynamic placeholder.", name, superclass_name);
                }
            }

            class_host_object = Box::new(ClassHostObject::from_template(
                template,
                /* is_metaclass: */ false,
                /* superclass: */
                template
                    .superclass
                    .map(|name| {
                        self.link_class(name, /* is_metaclass: */ false, mem)
                    })
                    .unwrap_or(nil),
                self,
            ));
            metaclass_host_object = Box::new(ClassHostObject::from_template(
                template,
                /* is_metaclass: */ true,
                /* superclass: */
                template
                    .superclass
                    .map(|name| {
                        self.link_class(name, /* is_metaclass: */ true, mem)
                    })
                    .unwrap_or(nil),
                self,
            ));
        } else {
            // ЗДЕСЬ ДОБАВЛЕНА ЛОГИКА ДЛЯ ДИНАМИЧЕСКИХ КЛАССОВ (GAD и др.)
            let is_fake = name.starts_with("AdMob")
                || name.starts_with("AltAds")
                || name.starts_with("Mobclix")
                || name.starts_with("FB")
                || name.starts_with("Flurry")
                || name.starts_with("OpenFeint")
                || name.starts_with("Tapjoy")
                || name.starts_with("UA")
                || name.starts_with("GAD")
                || name.starts_with("iSimulate")
                // SpringBoard private classes (e.g. "SBSceneFor%@" → "SBSceneFor(null)").
                // Apps probing for jailbroken-device features look these up
                // via NSClassFromString and only act if they exist; returning
                // a fake class lets the probe fail gracefully instead of
                // panicking. Restricted to the known SBScene prefix to avoid
                // accidentally swallowing real third-party SB-prefixed libs.
                || name.starts_with("SBScene")
                || name.starts_with("SBSystem"); // <-- ДОБАВЛЕНО ЗДЕСЬ

            // Note: empty/garbage class names are rejected earlier in this
            // function (they return nil), so by this point `name` is a
            // plausible, if unknown, class name.
            if !use_placeholder && !is_fake {
                // Historically this branch panicked. In the real
                // Objective-C runtime, looking up a class that doesn't
                // exist returns `nil` and the caller deals with it — it
                // never aborts the process. Apps frequently reference
                // classes that don't exist in the iOS version they
                // actually run on (e.g. UICollectionViewCell on iOS < 6),
                // or that touchHLE has no host implementation for, and
                // they expect to either fall back to a default class or
                // to detect the absence at runtime. Crashing here breaks
                // perfectly valid apps, so instead we log a loud warning
                // and install an UnimplementedClass placeholder, matching
                // the behaviour of the `link_class` (dyld) path.
                log!(
                    "Warning: get_known_class({:?}) — no host implementation; \
                     installing a placeholder. Some features depending on this \
                     class may not work.",
                    name
                );
            }

            if is_fake {
                class_host_object = Box::new(FakeClass {
                    name: name.to_string(),
                    is_metaclass: false,
                });
                metaclass_host_object = Box::new(FakeClass {
                    name: name.to_string(),
                    is_metaclass: true,
                });
            } else {
                // We don't have a real implementation for this class, use a
                // placeholder.
                class_host_object = Box::new(UnimplementedClass {
                    name: name.to_string(),
                    is_metaclass: false,
                });
                metaclass_host_object = Box::new(UnimplementedClass {
                    name: name.to_string(),
                    is_metaclass: true,
                });
            }
        }

        // NSObject's metaclass is special: it is its own metaclass, and it's
        // the superclass of all other metaclasses.
        // (FIXME: this actually should apply to any class hiearchy root.)
        // This creates a chicken-and-egg problem so it has a special path.
        let metaclass = if name == "NSObject" {
            let metaclass = mem.alloc_and_write(objc_object { isa: nil });
            mem.write(metaclass, objc_object { isa: metaclass });
            self.register_static_object(metaclass, metaclass_host_object);
            metaclass
        } else {
            let isa = self.link_class("NSObject", /* is_metaclass: */ true, mem);
            self.alloc_static_object(isa, metaclass_host_object, mem)
        };

        let class = self.alloc_static_object(metaclass, class_host_object, mem);
        if name == "NSObject" {
            // NSObject's metaclass has its class as the superclass.
            self.borrow_mut::<ClassHostObject>(metaclass).superclass = class;
        }

        self.classes.insert(name.to_string(), class);
        if is_metaclass {
            metaclass
        } else {
            class
        }
    }

    /// For use by [crate::dyld]: register all the classes from the application
    /// binary.
    pub fn register_bin_classes(&mut self, bundle: &bundle::Bundle, bin: &MachO, mem: &mut Mem) {
        let Some(list) = bin.get_section("__objc_classlist") else {
            return;
        };

        assert!(list.size % 4 == 0);
        let base: ConstPtr<Class> = Ptr::from_bits(list.addr);
        for i in 0..(list.size / 4) {
            let class = mem.read(base + i);
            let metaclass = Self::read_isa(class, mem);

            let name = if let Some(fakes) = substitute_classes(bundle, mem, class, metaclass) {
                let (class_host_object, metaclass_host_object) = fakes;
                assert!(class_host_object.name == metaclass_host_object.name);
                let name = class_host_object.name.clone();

                self.register_static_object(class, class_host_object);
                self.register_static_object(metaclass, metaclass_host_object);
                name
            } else {
                let class_host_object = Box::new(ClassHostObject::from_bin(
                    class, /* is_metaclass: */ false, mem, self,
                ));
                let mut metaclass_host_object = Box::new(ClassHostObject::from_bin(
                    metaclass, /* is_metaclass: */ true, mem, self,
                ));
                // Apple's Objective-C runtime guarantees that a class and its
                // metaclass always share the same NUL-terminated UTF-8 name:
                // `class_getName(cls)` and `class_getName(object_getClass(cls))`
                // return the identical string (see <objc/runtime.h>). In a
                // healthy Mach-O the `class_rw_t.name` pointer of the class and
                // its metaclass point at the very same C string. Real-world
                // binaries break this in two ways: (1) partially-decrypted
                // FairPlay IPAs whose `__objc_classname` bytes are unreadable,
                // so `from_bin` substitutes a per-pointer synthetic placeholder
                // that necessarily diverges between class and metaclass; and
                // (2) clobbered `__TEXT` segments where the metaclass name
                // pointer reads a *different* but still valid-UTF-8 string.
                // Apple's runtime never crashes on either; it simply uses the
                // class's name. Mirror that: treat the class side as canonical
                // and force the metaclass to match, unconditionally. This
                // removes the previous `assert!` that aborted the whole VM
                // (observed with GameStop_iOS, whose class/metaclass names were
                // both readable yet mismatched).
                if class_host_object.name != metaclass_host_object.name {
                    log!(
                        "Note: harmonizing mismatched class/metaclass names \
                         ({:?} vs {:?}) to {:?} for class at {:?}/{:?} (Apple's \
                         runtime guarantees they are identical).",
                        class_host_object.name,
                        metaclass_host_object.name,
                        class_host_object.name,
                        class,
                        metaclass,
                    );
                    metaclass_host_object.name = class_host_object.name.clone();
                }
                let name = class_host_object.name.clone();

                self.register_static_object(class, class_host_object);
                self.register_static_object(metaclass, metaclass_host_object);
                name
            };

            self.classes.insert(name.to_string(), class);
        }

        let mut queue = VecDeque::<Class>::new();
        let mut found_ns_object = false;
        // Second pass to build an inverted inheritance graph
        let mut inverted_inheritance = HashMap::<Class, Vec<Class>>::new();
        for (name, class) in self.classes.iter() {
            log_dbg!("class name {}", name);
            if name == "NSObject" {
                assert!(!found_ns_object);
                found_ns_object = true;
            }
            let class_host_object = self
                .get_host_object(*class)
                .unwrap()
                .as_any()
                .downcast_ref();
            let Some(ClassHostObject { superclass, .. }) = class_host_object else {
                // Skip FakeClass or UnimplementedClass
                continue;
            };

            if *superclass != nil {
                inverted_inheritance
                    .entry(*superclass)
                    .and_modify(|v| v.push(*class))
                    .or_insert(vec![*class]);
            } else {
                assert!(!queue.contains(class));
                queue.push_back(*class);
            }
        }
        // At least NSObject should be found as a root object
        assert!(found_ns_object);
        // Third pass to ensure no superclass has "grown into" any of its
        // subclasses.
        // (https://alwaysprocessing.blog/2023/03/12/objc-ivar-abi)

        // BFS starting from root(s) for ivar reconciliation
        //
        // It is required to be traversed in this order,
        // as one overgrown class potentially implies
        // reconciliation for _all of subclasses_
        while !queue.is_empty() {
            let next = queue.pop_front().unwrap();
            let (need, mut diff) = self.need_ivar_reconciliation(next);
            if need {
                let ClassHostObject {
                    name, superclass, ..
                } = self.borrow(next);
                log_dbg!(
                    "Class {} need ivar reconciliation with superclass {}!",
                    name,
                    &self.borrow::<ClassHostObject>(*superclass).name
                );
                let ClassHostObject {
                    ref mut instance_start,
                    ref mut instance_size,
                    ref mut ivars,
                    ..
                } = self.borrow_mut(next);

                if !ivars.is_empty() {
                    let mut max_alignment: u32 = 1;
                    for (offset, align) in ivars.values() {
                        if !offset.is_null() {
                            max_alignment = max_alignment.max(*align);
                        }
                    }

                    let align_mask = max_alignment - 1;
                    diff = (diff + align_mask) & !align_mask;

                    for (offset, _) in ivars.values_mut() {
                        if !offset.is_null() {
                            *offset = Ptr::from_bits((*offset).to_bits() + diff);
                        }
                    }
                }

                *instance_start += diff;
                *instance_size += diff;
            }
            if let Some(subclasses) = inverted_inheritance.get(&next) {
                queue.extend(subclasses);
            }
        }
    }

    fn need_ivar_reconciliation(&mut self, class: Class) -> (bool, u32) {
        let class_host_object = self.get_host_object(class).unwrap().as_any().downcast_ref();
        let Some(ClassHostObject {
            name,
            superclass,
            instance_start,
            instance_size,
            ..
        }) = class_host_object
        else {
            // The class might be a FakeClass or UnimplementedClass
            // In those cases we move on as they don't have ivars
            return (false, 0);
        };
        log_dbg!(
            "Checking need_ivar_reconciliation for {}, start {}, size {}",
            name,
            instance_start,
            instance_size
        );
        if *superclass == nil {
            return (false, 0);
        }

        let superclass_host_object = self
            .get_host_object(*superclass)
            .unwrap()
            .as_any()
            .downcast_ref();
        let Some(ClassHostObject {
            instance_size: superclass_instance_size,
            ..
        }) = superclass_host_object
        else {
            // Superclass could also be a FakeClass or UnimplementedClass
            return (false, 0);
        };

        let need = instance_start < superclass_instance_size;
        let diff = if need {
            superclass_instance_size - instance_start
        } else {
            0
        };
        (need, diff)
    }

    /// Dumps all classes available to the emulator in JSON to stdout.
    ///
    /// The JSON has the following form:
    /// ```json
    /// {
    ///     "object": "classes",
    ///     "classes": [
    ///         {
    ///             "name": ((name of class)),
    ///             "super": ((name of superclass, if available)),
    ///             "class_type": (("normal" | "unimplemented" | "fake"))
    ///         },
    ///         ...
    ///     ]
    /// }
    /// ```
    pub fn dump_classes(&self, file: &mut std::fs::File) -> Result<(), std::io::Error> {
        use std::io::Write;
        writeln!(file, "{{\n    \"object\": \"classes\",\n    \"classes\": [")?;
        for (i, (_, o)) in self.classes.iter().enumerate() {
            // Why doesn't json allow trailing commas...
            let comma = if i == self.classes.len() - 1 { "" } else { "," };
            let host_obj = self.get_host_object(*o).unwrap();

            if let Some(ClassHostObject {
                name,
                superclass: sup,
                ..
            }) = host_obj.as_any().downcast_ref()
            {
                if *sup == nil {
                    writeln!(
                        file,
                        "        {{ \"name\": \"{name}\", \"class_type\": \"normal\" }}{comma}"
                    )?;
                } else {
                    writeln!(
                        file,
                        "        {{ \"name\": \"{}\", \"super\": \"{}\", \"class_type\": \"normal\" }}{}",
                         name, self.get_class_name(*sup), comma
                    )?;
                }
            } else if let Some(UnimplementedClass { name, .. }) = host_obj.as_any().downcast_ref() {
                writeln!(
                    file,
                    "        {{ \"name\": \"{name}\", \"class_type\": \"unimplemented\" }}{comma}"
                )?;
            } else if let Some(FakeClass { name, .. }) = host_obj.as_any().downcast_ref() {
                writeln!(
                    file,
                    "        {{ \"name\": \"{name}\", \"class_type\": \"fake\" }}{comma}"
                )?;
            } else {
                log!(
                    "Warning: dump_classes: encountered unrecognized class host object; emitting placeholder entry.",
                );
                writeln!(
                    file,
                    "        {{ \"name\": \"<unknown>\", \"class_type\": \"unknown\" }}{comma}"
                )?;
            }
        }
        writeln!(file, "    ]\n}}")
    }

    /// For use by [crate::dyld]: register all the categories from the
    /// application binary.
    pub fn register_bin_categories(&mut self, bin: &MachO, mem: &mut Mem) {
        let Some(list) = bin.get_section("__objc_catlist") else {
            return;
        };

        assert!(list.size % 4 == 0);
        let base: ConstPtr<ConstPtr<category_t>> = Ptr::from_bits(list.addr);
        for i in 0..(list.size / 4) {
            let cat_ptr = mem.read(base + i);
            let data = mem.read(cat_ptr);

            // `category_t` is `#[repr(C, packed)]`, so `data.name` can't be
            // borrowed directly (unaligned). Copy out the pointer first.
            let name_ptr = data.name;
            let name = match mem.cstr_at_utf8(name_ptr) {
                Ok(s) => s,
                Err(_) => {
                    log!(
                        "Warning: register_bin_categories: category at {:?} has an unreadable (non-UTF-8) name at {:?}; skipping.",
                        cat_ptr,
                        name_ptr,
                    );
                    continue;
                }
            };
            let class = data.class;
            let metaclass = Self::read_isa(class, mem);
            for (class, methods) in [
                (class, data.instance_methods),
                (metaclass, data.class_methods),
            ] {
                if methods.is_null() {
                    continue;
                }

                let any = self.get_host_object(class).unwrap().as_any();
                if any.is::<FakeClass>() || any.is::<UnimplementedClass>() {
                    continue;
                }

                // Horrible workaround to avoid double-borrowing self:
                // temporarily replace the class object.
                let mut host_obj = std::mem::replace(
                    self.borrow_mut::<ClassHostObject>(class),
                    ClassHostObject {
                        name: Default::default(),
                        is_metaclass: Default::default(),
                        superclass: nil,
                        methods: Default::default(),
                        guest_method_signatures: Default::default(),
                        instance_start: Default::default(),
                        instance_size: Default::default(),
                        ivars: Default::default(),
                        properties: Default::default(),
                    },
                );
                log_dbg!(
                    "Adding {} methods from guest app category \"{}\" {:?} to {} \"{}\" {:?}",
                    if host_obj.is_metaclass {
                        "class"
                    } else {
                        "instance"
                    },
                    name,
                    cat_ptr,
                    if host_obj.is_metaclass {
                        "metaclass"
                    } else {
                        "class"
                    },
                    host_obj.name,
                    class,
                );
                host_obj.add_methods_from_bin(methods, mem, self);
                *self.borrow_mut::<ClassHostObject>(class) = host_obj;
            }
        }
    }

    pub fn class_is_subclass_of(&self, class: Class, superclass: Class) -> bool {
        if class == superclass {
            return true;
        }

        let mut class = class;
        loop {
            let &ClassHostObject {
                superclass: next, ..
            } = self.borrow(class);
            if next == nil {
                return false;
            } else if next == superclass {
                return true;
            } else {
                class = next;
            }
        }
    }

    /// Check whether `class` is a placeholder for a class touchHLE/HyperHLE
    /// does not implement (an `UnimplementedClass` host object).
    pub fn is_unimplemented_class(&self, class: Class) -> bool {
        if class == nil {
            return false;
        }
        let Some(host_object) = self.get_host_object(class) else {
            return false;
        };
        matches!(
            host_object.as_any().downcast_ref(),
            Some(UnimplementedClass { .. })
        )
    }

    /// Check whether `class` is a "fake" class (a `FakeClass` host object),
    /// i.e. one we tolerate the existence of without truly implementing.
    pub fn is_fake_class(&self, class: Class) -> bool {
        if class == nil {
            return false;
        }
        let Some(host_object) = self.get_host_object(class) else {
            return false;
        };
        matches!(host_object.as_any().downcast_ref(), Some(FakeClass { .. }))
    }

    pub fn get_class_name(&self, class: Class) -> &str {
        // Previously this `expect`-ed and panicked the whole emulator if the
        // class pointer didn't have a registered host object (e.g. when the
        // app sends a message to an object whose isa was clobbered, or when
        // a `borrow::<ClassHostObject>` already produced a phantom object —
        // see the "SUPER HACK! Faking borrow for missing object (null) of
        // type ClassHostObject" warnings that immediately precede the crash
        // in older logs). Recovering with a placeholder name keeps the rest
        // of the runtime running so logs/diagnostics still print useful
        // info instead of taking down the whole process.
        self.try_get_class_name(class).unwrap_or_else(|| {
            log!(
                "Warning: get_class_name: no host object for class {:?}; \
                 returning \"<unknown>\".",
                class,
            );
            "<unknown>"
        })
    }

    pub fn get_superclass(&self, class: Class) -> Class {
        let &ClassHostObject { superclass, .. } = self.borrow(class);
        superclass
    }

    pub fn try_get_class_name(&self, class: Class) -> Option<&str> {
        let host_object = self.get_host_object(class)?;
        if let Some(ClassHostObject { name, .. }) = host_object.as_any().downcast_ref() {
            Some(name)
        } else if let Some(UnimplementedClass { name, .. }) = host_object.as_any().downcast_ref() {
            Some(name)
        } else if let Some(FakeClass { name, .. }) = host_object.as_any().downcast_ref() {
            Some(name)
        } else {
            None
        }
    }
}

pub fn objc_getClass(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_begin_catch(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_end_catch(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_exception_throw(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn object_getClassName(env: &mut crate::Environment, obj: id) -> Class {
    if obj.is_null() {
        return nil;
    }

    let objc_obj: objc_object = env.mem.read(obj.cast());
    objc_obj.isa
}

pub fn object_getClass(env: &mut crate::Environment, obj: id) -> Class {
    if obj.is_null() {
        return nil;
    }

    let objc_obj: objc_object = env.mem.read(obj.cast());
    objc_obj.isa
}

pub fn objc_retainAutoreleasedReturnValue(
    env: &mut crate::Environment,
    name: ConstPtr<u8>,
) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_autoreleaseReturnValue(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_retainAutoreleaseReturnValue(
    env: &mut crate::Environment,
    name: ConstPtr<u8>,
) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_autoreleasePoolPush(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    if name.is_null() {
        return nil;
    }

    let name_str = match env.mem.cstr_at_utf8(name) {
        Ok(s) => s.to_string(),
        Err(_) => return nil,
    };
    if let Some(class) = env.objc.get_class(&name_str, false, &env.mem) {
        return class;
    }

    if ObjC::find_template(&name_str).is_some() {
        return env.objc.link_class(&name_str, false, &mut env.mem);
    }

    nil
}

pub fn objc_autoreleasePoolPop(_env: &mut crate::Environment, _context: MutVoidPtr) {
    // touchHLE manages autorelease pools through NSAutoreleasePool objects, so
    // the matching `objc_autoreleasePoolPush` is a no-op stub that returns
    // nil, and there is nothing to drain here. iPhone OS 2.x/3.x apps target
    // this path very rarely (it's primarily used by ARC).
}

pub fn class_getSuperclass(env: &mut crate::Environment, cls: Class) -> Class {
    if cls.is_null() {
        return nil;
    }
    env.objc.get_superclass(cls)
}

pub fn class_getInstanceSize(env: &mut crate::Environment, cls: Class, name: SEL) -> ConstVoidPtr {
    if cls.is_null() {
        return ConstVoidPtr::null();
    }

    let mut curr = cls;
    while !curr.is_null() {
        if let Some(host_obj) = env.objc.get_host_object(curr) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if class_obj.methods.contains_key(&name) {
                    return curr.cast_const().cast();
                }
            }
        }
        let next = env.objc.get_superclass(curr);
        if next == curr {
            break;
        }
        curr = next;
    }
    ConstVoidPtr::null()
}

// ──────────────────────────────────────────────────────────────────
// Opaque `Method` handles
//
// Apple's `Method` is a pointer to the `method_t` entry inside a class's
// method list; it identifies BOTH the class that defines the method and the
// selector. touchHLE stores methods in a `HashMap<SEL, IMP>` per class, so we
// materialise a stable 8-byte guest allocation per (defining class, selector)
// pair — `[u32 class_bits][u32 selector_bits]` — and hand its pointer to the
// guest as the opaque `Method`. All `method_*` functions decode the handle
// back into (class, selector) to operate on the real method table, which is
// what makes swizzling (`method_exchangeImplementations`,
// `method_setImplementation`) actually take effect.
// ──────────────────────────────────────────────────────────────────

/// Returns the nearest class in `cls`'s superclass chain that defines an
/// uninherited implementation of `sel`, or `nil` if none does.
fn find_defining_class(env: &crate::Environment, cls: Class, sel: SEL) -> Class {
    let mut curr = cls;
    while !curr.is_null() {
        if let Some(host_obj) = env.objc.get_host_object(curr) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if class_obj.methods.contains_key(&sel) {
                    return curr;
                }
            }
        }
        let next = env.objc.get_superclass(curr);
        if next == curr {
            break;
        }
        curr = next;
    }
    nil
}

/// Get-or-create the stable opaque `Method` handle for a (defining class,
/// selector) pair.
fn method_handle_for(env: &mut crate::Environment, defining_class: Class, sel: SEL) -> ConstVoidPtr {
    if let Some(&existing) = env.objc.method_handles.get(&(defining_class, sel)) {
        return existing.cast_const();
    }
    let handle: crate::mem::MutPtr<u32> = env.mem.alloc(8).cast();
    env.mem.write(handle, defining_class.to_bits());
    env.mem.write(handle + 1, sel.to_bits());
    let vp: MutVoidPtr = handle.cast();
    env.objc
        .method_handles
        .insert((defining_class, sel), vp);
    vp.cast_const()
}

/// Decode a `Method` handle back into its (defining class, selector) pair.
/// Returns `None` if the pointer is NULL or was not produced by
/// [method_handle_for] (guards against the guest passing an arbitrary
/// pointer).
fn method_handle_decode(env: &crate::Environment, m: ConstVoidPtr) -> Option<(Class, SEL)> {
    if m.is_null() {
        return None;
    }
    let known = env
        .objc
        .method_handles
        .values()
        .any(|p| p.to_bits() == m.to_bits());
    if !known {
        return None;
    }
    let base: ConstPtr<u32> = m.cast();
    let cls: Class = Ptr::from_bits(env.mem.read(base));
    let sel: SEL = SEL::from_bits(env.mem.read(base + 1));
    Some((cls, sel))
}

/// Read the [IMP] stored for `sel` directly on `cls` (no inheritance walk).
fn imp_at(env: &crate::Environment, cls: Class, sel: SEL) -> Option<IMP> {
    env.objc
        .get_host_object(cls)
        .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
        .and_then(|c| c.methods.get(&sel).copied())
}

/// Install `imp` for `sel` directly on `cls` (no inheritance walk).
fn set_imp_at(env: &mut crate::Environment, cls: Class, sel: SEL, imp: IMP) {
    let class_obj = env.objc.borrow_mut::<ClassHostObject>(cls);
    class_obj.methods.insert(sel, imp);
}

/// Convert an [IMP] into the guest pointer that Apple's `IMP` type is (a
/// function pointer). Guest implementations expose their real code address;
/// host (Rust-backed) implementations have no guest address, so we mint a
/// stable token pointer for the (class, selector) and remember which host
/// IMP it stands for, allowing it to be round-tripped back through
/// `method_setImplementation` / `method_exchangeImplementations`.
fn imp_to_guest_ptr(env: &mut crate::Environment, cls: Class, sel: SEL, imp: IMP) -> ConstVoidPtr {
    match imp {
        IMP::Guest(gf) => gf.to_ptr(),
        IMP::Host(_) => {
            let token = if let Some(&t) = env.objc.host_imp_tokens.get(&(cls, sel)) {
                t
            } else {
                let t = env.mem.alloc(4);
                env.objc.host_imp_tokens.insert((cls, sel), t);
                t
            };
            env.objc.imp_tokens.insert(token.to_bits(), imp);
            token.cast_const()
        }
    }
}

/// Convert a guest `IMP` pointer received from the guest back into an [IMP].
/// A pointer previously handed out for a host implementation is looked up in
/// the token registry; anything else is treated as a guest code address.
fn guest_ptr_to_imp(env: &crate::Environment, imp: ConstVoidPtr) -> Option<IMP> {
    if imp.is_null() {
        return None;
    }
    if let Some(host_imp) = env.objc.imp_tokens.get(&imp.to_bits()) {
        return Some(*host_imp);
    }
    Some(IMP::Guest(crate::abi::GuestFunction::from_addr_with_thumb_bit(
        imp.to_bits(),
    )))
}

/// `Method class_getInstanceMethod(Class cls, SEL name)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418889-class_getinstancemethod?language=objc):
///
/// > Returns a specified instance method for a given class, searching
/// > superclasses for the implementation. Returns `NULL` if `cls` is `Nil`,
/// > `name` is `NULL`, or instances of `cls` do not respond to `name`.
pub fn class_getInstanceMethod(
    env: &mut crate::Environment,
    cls: Class,
    name: SEL,
) -> ConstVoidPtr {
    if cls.is_null() || name.is_null() {
        return ConstVoidPtr::null();
    }
    let defining = find_defining_class(env, cls, name);
    if defining.is_null() {
        return ConstVoidPtr::null();
    }
    method_handle_for(env, defining, name)
}

/// `BOOL class_respondsToSelector(Class cls, SEL sel)`
///
/// Apple: "Returns a Boolean value that indicates whether instances of a
/// class respond to a particular selector." Walks the class chain and
/// reports YES if any class in that chain implements `sel`.
/// <https://developer.apple.com/documentation/objectivec/1418555-class_respondstoselector>
///
/// Per the documentation `class_respondsToSelector(Nil, …)` returns NO and
/// `class_respondsToSelector(cls, NULL)` also returns NO. Mirroring the
/// real runtime here avoids the previous return-0 stub silently breaking
/// guests that use this entry point to gate optional behaviour
/// (e.g. `class_respondsToSelector([NSString class], @selector(...))` for
/// runtime-availability checks in older SDKs).
pub fn class_respondsToSelector(
    env: &mut crate::Environment,
    cls: Class,
    sel: SEL,
) -> bool {
    if cls.is_null() || sel.is_null() {
        return false;
    }

    let mut curr = cls;
    while !curr.is_null() {
        if let Some(host_obj) = env.objc.get_host_object(curr) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if class_obj.methods.contains_key(&sel) {
                    return true;
                }
            }
        }
        let next = env.objc.get_superclass(curr);
        if next == curr {
            break;
        }
        curr = next;
    }
    false
}

/// `IMP method_getImplementation(Method m)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418551-method_getimplementation?language=objc):
///
/// > Returns the implementation of a method.
///
/// `m` is one of our opaque `Method` handles (see [method_handle_for]); we
/// decode it to a (class, selector) pair and return the guest function
/// pointer of the installed implementation.
pub fn method_getImplementation(env: &mut crate::Environment, m: ConstVoidPtr) -> ConstVoidPtr {
    let Some((cls, sel)) = method_handle_decode(env, m) else {
        return ConstVoidPtr::null();
    };
    match imp_at(env, cls, sel) {
        Some(imp) => imp_to_guest_ptr(env, cls, sel, imp),
        None => ConstVoidPtr::null(),
    }
}

/// `IMP method_setImplementation(Method m, IMP imp)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418707-method_setimplementation?language=objc):
///
/// > Sets the implementation of a method. Returns the previous
/// > implementation of the method.
///
/// This is one half of the classic swizzling idiom (the other being
/// `method_exchangeImplementations`). Decoding the `Method` handle lets us
/// mutate the real method table so the change is observed by subsequent
/// dispatch — exactly what patterns like cocos2d's `SynthesizeSingleton`
/// depend on.
pub fn method_setImplementation(
    env: &mut crate::Environment,
    m: ConstVoidPtr,
    imp: ConstVoidPtr,
) -> ConstVoidPtr {
    let Some((cls, sel)) = method_handle_decode(env, m) else {
        return ConstVoidPtr::null();
    };
    let old = imp_at(env, cls, sel);
    if let Some(new_imp) = guest_ptr_to_imp(env, imp) {
        set_imp_at(env, cls, sel, new_imp);
    }
    match old {
        Some(old_imp) => imp_to_guest_ptr(env, cls, sel, old_imp),
        None => ConstVoidPtr::null(),
    }
}

/// `const char *method_getTypeEncoding(Method m)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418488-method_gettypeencoding?language=objc):
///
/// > Returns a string describing a method's parameter and return types.
///
/// We return the guest pointer to the method's stored type-encoding string
/// (as parsed from the binary or supplied via `class_addMethod`), or NULL if
/// none is recorded.
pub fn method_getTypeEncoding(env: &mut crate::Environment, m: ConstVoidPtr) -> ConstVoidPtr {
    let Some((cls, sel)) = method_handle_decode(env, m) else {
        return ConstVoidPtr::null();
    };
    let types = env
        .objc
        .get_host_object(cls)
        .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
        .and_then(|c| c.guest_method_signatures.get(&sel).copied());
    match types {
        Some(ptr) => ptr.cast(),
        None => ConstVoidPtr::null(),
    }
}

/// `SEL method_getName(Method m)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418758-method_getname?language=objc):
///
/// > Returns the name of a method (the selector it is registered under).
pub fn method_getName(env: &mut crate::Environment, m: ConstVoidPtr) -> SEL {
    match method_handle_decode(env, m) {
        Some((_cls, sel)) => sel,
        None => SEL::null(),
    }
}

pub fn objc_getMetaClass(env: &mut crate::Environment, cls: Class, name: SEL) -> ConstVoidPtr {
    if cls.is_null() {
        return ConstVoidPtr::null();
    }

    let mut curr = cls;
    while !curr.is_null() {
        if let Some(host_obj) = env.objc.get_host_object(curr) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if class_obj.methods.contains_key(&name) {
                    return curr.cast_const().cast();
                }
            }
        }
        let next = env.objc.get_superclass(curr);
        if next == curr {
            break;
        }
        curr = next;
    }
    ConstVoidPtr::null()
}

/// `IMP class_replaceMethod(Class cls, SEL name, IMP imp, const char *types)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418677-class_replacemethod?language=objc):
///
/// > Replaces the implementation of a method for a given class.
/// >
/// > - If the method identified by `name` does not yet exist, it is added as
/// >   if `class_addMethod` were called (and the return value is `NULL`).
/// > - If the method identified by `name` does exist, its `IMP` is replaced
/// >   as if `method_setImplementation` were called (and the previous `IMP`
/// >   is returned).
pub fn class_replaceMethod(
    env: &mut crate::Environment,
    cls: Class,
    name: SEL,
    imp: ConstVoidPtr,
    types: ConstPtr<u8>,
) -> ConstVoidPtr {
    if cls.is_null() || name.is_null() {
        return ConstVoidPtr::null();
    }

    // Whether `cls` itself already defines `name` (overrides of a superclass
    // do not count, matching `class_addMethod`'s contract).
    let already_defined = imp_at(env, cls, name).is_some();
    let old = imp_at(env, cls, name);

    // A NULL IMP with an existing method removes nothing in Apple's runtime
    // (it is treated as a request to install the forwarding IMP); we simply
    // leave the existing implementation in place and return it.
    if let Some(new_imp) = guest_ptr_to_imp(env, imp) {
        set_imp_at(env, cls, name, new_imp);
        if !types.is_null() {
            let class_obj = env.objc.borrow_mut::<ClassHostObject>(cls);
            class_obj.guest_method_signatures.insert(name, types);
        }
    }

    if already_defined {
        match old {
            Some(old_imp) => imp_to_guest_ptr(env, cls, name, old_imp),
            None => ConstVoidPtr::null(),
        }
    } else {
        ConstVoidPtr::null()
    }
}

/// `BOOL class_addMethod(Class cls, SEL name, IMP imp, const char *types)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418901-class_addmethod?language=objc):
///
/// > Adds a new method to a class with a given name and implementation.
/// >
/// > Returns `YES` if the method was added successfully, otherwise `NO`
/// > (for example, the class already contains a method implementation
/// > with that name).
/// >
/// > `class_addMethod` will add an override of a superclass's
/// > implementation, but will not replace an existing implementation in
/// > this class. To change an existing implementation, use
/// > `method_setImplementation`.
///
/// We model `IMP` as a guest function pointer (the `Method` type returned
/// by `class_getInstanceMethod` is touchHLE-specific and is not the same
/// representation as `IMP`). The `types` string is captured into
/// `guest_method_signatures` so that subsequent dispatch through
/// `methodSignatureForSelector:` can honour it.
pub fn class_addMethod(
    env: &mut crate::Environment,
    cls: Class,
    name: SEL,
    imp: ConstVoidPtr,
    types: ConstPtr<u8>,
) -> bool {
    if cls.is_null() || name.is_null() {
        return false;
    }
    // Refuse to install a NULL IMP — calling it would crash the guest as
    // soon as the selector is dispatched, and Apple's runtime treats it as
    // a programmer error (the docs require `imp` to be a function pointer
    // of type `IMP`).
    if imp.is_null() {
        log!(
            "Warning: class_addMethod({:?}, {:?}, NULL imp, ...) — refusing to install NULL IMP",
            cls,
            name.as_str(&env.mem)
        );
        return false;
    }

    // Determine whether the receiver class already has an implementation
    // for this selector. Apple's contract is that `class_addMethod` only
    // succeeds when the method is NOT already defined on the receiver
    // (overrides of superclasses ARE allowed, hence we don't walk up the
    // chain here).
    let already_defined = if let Some(host_obj) = env.objc.get_host_object(cls) {
        host_obj
            .as_any()
            .downcast_ref::<ClassHostObject>()
            .is_some_and(|c| c.methods.contains_key(&name))
    } else {
        false
    };
    if already_defined {
        log_dbg!(
            "class_addMethod({:?}, {:?}, ...) — method already defined on receiver, returning NO",
            cls,
            name.as_str(&env.mem)
        );
        return false;
    }

    // Treat the IMP as a guest function pointer. `IMP` on real Apple is
    // `id (*)(id, SEL, ...)` — i.e. a C function pointer. Method names
    // ending with `:` are encoded for the Thumb bit by the linker, so we
    // pass the raw bits straight through.
    let guest_imp =
        crate::abi::GuestFunction::from_addr_with_thumb_bit(imp.to_bits());

    // Install the new method. `borrow_mut::<ClassHostObject>` walks any
    // host-object inheritance chain so this works for both classes and
    // metaclasses (whose host objects are both `ClassHostObject`).
    {
        let class_obj = env.objc.borrow_mut::<ClassHostObject>(cls);
        class_obj
            .methods
            .insert(name, super::methods::IMP::Guest(guest_imp));
        if !types.is_null() {
            class_obj.guest_method_signatures.insert(name, types);
        }
    }
    log_dbg!(
        "class_addMethod({:?}, {:?}, imp={:?}) — installed",
        cls,
        name.as_str(&env.mem),
        imp
    );
    true
}

/// `Method class_getClassMethod(Class cls, SEL name)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418680-class_getclassmethod?language=objc):
///
/// > Returns a pointer to the data structure describing the class method
/// > identified by the given selector.
/// >
/// > Note: this function searches superclasses for implementations.
///
/// In touchHLE's simplified runtime the returned "Method" value is the
/// class pointer itself (matching what `class_getInstanceMethod` returns).
/// Class methods live on the metaclass, so we resolve the metaclass first
/// and then walk its chain looking for the selector.
pub fn class_getClassMethod(
    env: &mut crate::Environment,
    cls: Class,
    name: SEL,
) -> ConstVoidPtr {
    if cls.is_null() {
        return ConstVoidPtr::null();
    }
    // The metaclass holds the class-method table. `ObjC::read_isa` of a
    // class returns its metaclass.
    let mut curr = crate::objc::ObjC::read_isa(cls, &env.mem);
    while !curr.is_null() {
        if let Some(host_obj) = env.objc.get_host_object(curr) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if class_obj.methods.contains_key(&name) {
                    return curr.cast_const().cast();
                }
            }
        }
        let next = env.objc.get_superclass(curr);
        if next == curr {
            break;
        }
        curr = next;
    }
    ConstVoidPtr::null()
}

pub fn objc_retain(env: &mut crate::Environment, obj: id) -> id {
    if !obj.is_null() {
        crate::objc::retain(env, obj);
    }
    obj
}

pub fn objc_release(env: &mut crate::Environment, obj: id) -> id {
    if !obj.is_null() {
        crate::objc::release(env, obj);
    }
    obj
}

/// `void _objc_deallocOnMainThreadHelper(void *context)` — used by libobjc
/// in conjunction with `dispatch_async_f(dispatch_get_main_queue(), …)` to
/// release a `-finalize`/`-dealloc`-ing object back on the main thread when
/// the runtime detects the object is being released on a background thread.
/// The context argument is the `id` to release. See
/// `objc4-866.9/runtime/NSObject.mm` (`_objc_deallocOnMainThreadHelper`).
///
/// Forwarding to `objc_release` is correct because touchHLE serialises the
/// guest's execution on a single host thread; "release on the main thread"
/// reduces to "release now".
#[allow(non_snake_case)]
pub fn __objc_deallocOnMainThreadHelper(env: &mut crate::Environment, context: MutVoidPtr) {
    let obj: id = Ptr::from_bits(context.to_bits());
    if !obj.is_null() {
        crate::objc::release(env, obj);
    }
}

pub fn ___objc_personality_v0(
    _env: &mut crate::Environment,
    version: i32,
    actions: i32,
    exception_class: u64,
    exception_object: MutVoidPtr,
    context: MutVoidPtr,
) -> i32 {
    log!(
        "___objc_personality_v0 called! Exception handling is not fully implemented in touchHLE.\n\
        version: {}, actions: {}, class: {:x}, object: {:?}, context: {:?}",
        version,
        actions,
        exception_class,
        exception_object,
        context
    );
    // _URC_FATAL_PHASE1_ERROR
    3
}

/// `_objc_msgForward` — the ObjC runtime's message forwarding trampoline.
///
/// On a real device, when `objc_msgSend` cannot find an IMP for a given
/// selector, it dispatches through `_objc_msgForward` which triggers the
/// full forwarding chain (`forwardingTargetForSelector:`,
/// `methodSignatureForSelector:`, `forwardInvocation:`). touchHLE does not
/// implement this machinery; instead we provide a stub that returns nil (0).
///
/// This is exported so that binaries containing external relocations to
/// `__objc_msgForward` (e.g. Cut the Rope HD) can link without errors.
/// When the stub is actually called, the return value of nil/0 matches the
/// behaviour of sending a message to nil — the least-surprising fallback
/// for games that probe forwarding.
pub fn objc_msgForward(_env: &mut crate::Environment, _receiver: id, _sel: SEL) -> id {
    log_dbg!(
        "_objc_msgForward called (receiver={:?}, sel={:?}) — returning nil",
        _receiver,
        _sel
    );
    nil
}

/// `_objc_msgForward_stret` — struct-return variant of `_objc_msgForward`.
/// Same stub behaviour: returns without writing to the stret buffer.
pub fn objc_msgForward_stret(
    _env: &mut crate::Environment,
    _stret: MutVoidPtr,
    _receiver: id,
    _sel: SEL,
) {
    log_dbg!(
        "_objc_msgForward_stret called (receiver={:?}, sel={:?}) — no-op",
        _receiver,
        _sel
    );
}
/// `void objc_storeStrong(id *location, id obj)` — ARC's strong-store
/// runtime helper. The clang ARC specification
/// (<https://clang.llvm.org/docs/AutomaticReferenceCounting.html#runtime-support>)
/// describes the canonical implementation as:
///
/// ```text
/// id prev = *location;
/// if (prev == obj) return;
/// objc_retain(obj);
/// *location = obj;
/// objc_release(prev);
/// ```
///
/// This is what the Apple objc runtime ships as `objc_storeStrong`. We
/// implement it line-by-line so the reference counts of both the old
/// and new objects stay consistent with how a non-ARC manual
/// retain/release would have managed them.
pub fn objc_storeStrong(
    env: &mut crate::Environment,
    location: crate::mem::MutPtr<id>,
    obj: id,
) {
    if location.is_null() {
        return;
    }
    let prev = env.mem.read(location);
    if prev == obj {
        return;
    }
    if !obj.is_null() {
        crate::objc::retain(env, obj);
    }
    env.mem.write(location, obj);
    if !prev.is_null() {
        crate::objc::release(env, prev);
    }
}

/// `IMP class_getMethodImplementation(Class cls, SEL name)` — Objective-C
/// runtime helper. Per Apple's reference
/// (<https://developer.apple.com/documentation/objectivec/1418811-class_getmethodimplementation>):
///
/// > Returns the function pointer that would be called if a particular
/// > message were sent to an instance of a class. The function returned
/// > might be a function internal to the runtime instead of an actual
/// > method implementation. For example, if instances of `cls` do not
/// > respond to `name`, the function returned will be part of the
/// > runtime's message forwarding machinery.
///
/// touchHLE doesn't model `_objc_msgForward`; if the selector isn't
/// implemented we return a NULL pointer, which the caller is expected
/// to compare against and skip the dispatch (Apple binaries doing
/// `IMP imp = class_getMethodImplementation(cls, sel); if (imp) imp(…)`
/// behave correctly with this).
pub fn class_getMethodImplementation(
    env: &mut crate::Environment,
    cls: Class,
    name: crate::objc::SEL,
) -> ConstVoidPtr {
    if cls.is_null() || name.is_null() {
        return ConstVoidPtr::null();
    }
    // Unlike `method_getImplementation` (which takes an opaque `Method`),
    // this call resolves the selector through the inheritance chain.
    let defining = find_defining_class(env, cls, name);
    if defining.is_null() {
        return ConstVoidPtr::null();
    }
    match imp_at(env, defining, name) {
        Some(imp) => imp_to_guest_ptr(env, defining, name, imp),
        None => ConstVoidPtr::null(),
    }
}

/// `IMP class_getMethodImplementation_stret(Class cls, SEL name)` — same
/// as above but for selectors whose return type uses `objc_msgSend_stret`
/// (large struct returns). The IMP slot is the same in touchHLE.
pub fn class_getMethodImplementation_stret(
    env: &mut crate::Environment,
    cls: Class,
    name: crate::objc::SEL,
) -> ConstVoidPtr {
    class_getMethodImplementation(env, cls, name)
}

pub fn objc_retainAutorelease(env: &mut crate::Environment, obj: id) -> id {
    if !obj.is_null() {
        crate::objc::retain(env, obj);
        crate::objc::autorelease(env, obj);
    }
    obj
}

/// `id objc_alloc(Class cls)` — ARC/libobjc fast-path allocator. Apple's
/// objc4 runtime (`NSObject.mm`, `objc_alloc`) implements this as
/// `callAlloc(cls, /*checkNil=*/true, /*allocWithZone=*/false)`, which for a
/// class that does not override `+allocWithZone:` is exactly equivalent to
/// `[cls alloc]`. Forwarding the `alloc` selector reproduces that behaviour
/// precisely, including honouring any class that provides its own `+alloc`.
/// Compilers emit calls to this helper instead of an `objc_msgSend(cls,
/// @selector(alloc))` to save a selector lookup, so it must really allocate
/// — a return-0 stub leaves the guest with a nil object and crashes later.
pub fn objc_alloc(env: &mut crate::Environment, cls: Class) -> id {
    if cls.is_null() {
        return nil;
    }
    crate::objc::msg![env; cls alloc]
}

/// `id objc_autorelease(id obj)` — ARC/libobjc fast-path autorelease. objc4
/// implements it as `obj ? obj->autorelease() : nil`: the object is added to
/// the current autorelease pool and returned unchanged. touchHLE's
/// `autorelease` helper does exactly this (with a nil fast path), so we
/// forward to it. This is the real implementation, not a stub: returning 0
/// here would hand the guest a nil where it expects its object back.
pub fn objc_autorelease(env: &mut crate::Environment, obj: id) -> id {
    crate::objc::autorelease(env, obj)
}

/// `id objc_retainBlock(id block)` — ARC runtime helper for retaining a
/// block. Apple's objc4 runtime (`NSObject.mm`) implements this as a thin
/// wrapper around the Blocks runtime:
///
/// ```text
/// id objc_retainBlock(id x) { return (id)_Block_copy(x); }
/// ```
///
/// touchHLE's `_Block_copy` (see `src/libc/blocks.rs`) does not physically
/// duplicate the block — global blocks (the common case for static literal
/// blocks) are not reference-counted, and stack-block promotion needs deeper
/// Block ABI work — so it returns the same pointer. Mirroring that here keeps
/// `objc_retainBlock` consistent with the rest of the Block runtime, while
/// still providing the correct return value the ARC-generated code expects.
pub fn objc_retainBlock(env: &mut crate::Environment, block: id) -> id {
    let _ = env;
    block
}

/// `id objc_unsafeClaimAutoreleasedReturnValue(id obj)` — ARC runtime
/// optimisation counterpart to `objc_retainAutoreleasedReturnValue`. Apple's
/// objc4 runtime uses it when a returned object is consumed by code that does
/// *not* want an owning reference (e.g. the result is immediately passed on,
/// or stored `__unsafe_unretained`): it accepts the autoreleased value and
/// claims it without adding a retain, balancing the optimised return sequence.
/// In touchHLE's serialised execution model there is no retain/autorelease
/// elision to undo, so the correct behaviour is to return the object
/// unchanged (and nil-safe). See `objc4` `NSObject.mm`,
/// `objc_unsafeClaimAutoreleasedReturnValue`.
pub fn objc_unsafeClaimAutoreleasedReturnValue(env: &mut crate::Environment, obj: id) -> id {
    let _ = env;
    obj
}

// === Additional ObjC runtime helpers used by iOS 5/6 Cocoa classes ===
//
// These are part of the public Objective-C runtime header
// (`<objc/runtime.h>`) but were not historically used by the iPhone OS
// 2.x/3.x apps that touchHLE originally targeted. iOS 6+ binaries
// reference them through Mach-O imports (often defensively, e.g. for
// dynamic class registration, KVO swizzling, or analytics SDKs probing
// for runtime features). Without host implementations the linker leaves
// the slots NULL and any first-call indirect branch crashes the
// emulator.

/// `const char *class_getName(Class cls)` — returns the C-string class
/// name. We allocate the cstring in guest memory the first time a name
/// is requested per class and cache the pointer in the side-table so
/// repeated calls return the same address (matching real ObjC's
/// guarantee that the returned pointer is stable for the lifetime of
/// the class).
pub fn class_getName(env: &mut crate::Environment, cls: Class) -> ConstPtr<u8> {
    use crate::mem::Ptr;
    if cls.is_null() {
        // Apple: returns "nil"; touchHLE returns the empty string
        // pointer to keep the call safe.
        return Ptr::null();
    }
    let name = env.objc.get_class_name(cls).to_owned();
    let bytes = name.as_bytes();
    let len: u32 = bytes.len() as u32 + 1;
    let buf: crate::mem::MutPtr<u8> = env.mem.alloc(len).cast();
    for (i, &b) in bytes.iter().enumerate() {
        env.mem.write(buf + i as u32, b);
    }
    env.mem.write(buf + bytes.len() as u32, 0);
    buf.cast_const()
}

/// `Class objc_lookUpClass(const char *name)` — like `objc_getClass`
/// but does *not* invoke the class handler if the class is missing
/// (returns nil instead of trying to load it). For our HLE runtime
/// the behaviour is identical to `objc_getClass`.
pub fn objc_lookUpClass(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    objc_getClass(env, name)
}

/// `Class objc_getRequiredClass(const char *name)` — same as
/// `objc_getClass` but documented to abort on miss. Our runtime is
/// best-effort, so we just degrade to the lenient lookup; if the
/// class is genuinely missing, returning nil keeps the guest alive
/// instead of taking the host down.
pub fn objc_getRequiredClass(env: &mut crate::Environment, name: ConstPtr<u8>) -> Class {
    let class = objc_getClass(env, name);
    if class == nil {
        if let Ok(s) = env.mem.cstr_at_utf8(name) {
            log!(
                "Warning: objc_getRequiredClass({:?}): class not found; \
                 returning nil (the real runtime would abort).",
                s
            );
        }
    }
    class
}

/// `Class objc_allocateClassPair(Class superclass, const char *name,
/// size_t extraBytes)` — used to dynamically create a new class.
/// Implementing this properly requires wiring into our `ClassHostObject`
/// machinery; until that's done, we install a placeholder
/// (UnimplementedClass) so the symbol resolves and the guest can decide
/// to fall back. Calls to `+[NewClass alloc]` etc. will go through the
/// usual UnimplementedClass path.
pub fn objc_allocateClassPair(
    env: &mut crate::Environment,
    _superclass: Class,
    name: ConstPtr<u8>,
    _extra_bytes: u32,
) -> Class {
    let Ok(s) = env.mem.cstr_at_utf8(name) else {
        return nil;
    };
    let name_str = s.to_string();
    log!(
        "Warning: objc_allocateClassPair({:?}, …): dynamic class registration is \
         not fully supported; installing an UnimplementedClass placeholder.",
        name_str
    );
    env.objc
        .link_class(&name_str, /* is_metaclass: */ false, &mut env.mem)
}

/// `Class objc_readClassPair(Class cls, const struct objc_image_info *info)` —
/// iOS 8+ Swift-related helper. Apps from iOS 6/7 may import the
/// symbol but rarely actually call it. Return the input class
/// unchanged so any best-effort caller keeps working.
pub fn objc_readClassPair(_env: &mut crate::Environment, cls: Class, _info: ConstVoidPtr) -> Class {
    cls
}

pub fn swift_getInitializedObjCClass(
    env: &mut crate::Environment,
    cls: Class,
) -> Class {
    if cls.is_null() {
        return nil;
    }
    let initialize_sel = env
        .objc
        .lookup_selector("initialize")
        .unwrap_or_else(|| env.objc.register_host_selector("initialize".to_string(), &mut env.mem));
    if env.objc.object_has_method(&env.mem, cls, initialize_sel) {
        let _: id = crate::objc::msg_send_no_initialize(env, (cls, initialize_sel));
    }
    cls
}

pub fn objc_allocWithZone(
    env: &mut crate::Environment,
    cls: Class,
    zone: crate::mem::MutVoidPtr,
) -> id {
    if cls.is_null() {
        return nil;
    }
    crate::objc::msg![env; cls allocWithZone:zone]
}

/// `void objc_registerClassPair(Class cls)` — finalise the registration
/// of a dynamically created class. touchHLE's [`objc_allocateClassPair`]
/// already inserts the class into the runtime tables; the matching
/// `register` call is therefore a no-op that just publishes the class.
/// <https://developer.apple.com/documentation/objectivec/1418414-objc_registerclasspair>
pub fn objc_registerClassPair(_env: &mut crate::Environment, _cls: Class) {}

/// `void objc_disposeClassPair(Class cls)` — destroys a class created
/// with [`objc_allocateClassPair`] that has not yet been registered.
/// touchHLE doesn't reclaim class storage, so we just drop the reference
/// on the floor (Apple's runtime is also lazy about this for very small
/// allocations).
/// <https://developer.apple.com/documentation/objectivec/1418912-objc_disposeclasspair>
pub fn objc_disposeClassPair(_env: &mut crate::Environment, _cls: Class) {}

/// `Class class_setSuperclass(Class cls, Class newSuper)` — deprecated
/// since OS X 10.5 but still exported by libobjc and used by older
/// hooking libraries. Returns the previous superclass and rewrites the
/// class's inheritance chain in-place.
/// <https://developer.apple.com/documentation/objectivec/1418687-class_setsuperclass>
pub fn class_setSuperclass(
    env: &mut crate::Environment,
    cls: Class,
    new_super: Class,
) -> Class {
    if cls.is_null() {
        return nil;
    }
    let old = env.objc.get_superclass(cls);
    env.objc.borrow_mut::<ClassHostObject>(cls).superclass = new_super;
    old
}

/// `Protocol *objc_getProtocol(const char *name)` — touchHLE doesn't
/// model protocols separately; return nil so any defensive
/// `if (proto)` check skips the protocol-specific path.
pub fn objc_getProtocol(_env: &mut crate::Environment, _name: ConstPtr<u8>) -> id {
    nil
}

/// `const char *protocol_getName(Protocol *p)` — paired with
/// `objc_getProtocol`. Returns NULL since we never hand out a real
/// Protocol pointer to the guest.
pub fn protocol_getName(_env: &mut crate::Environment, _p: id) -> ConstPtr<u8> {
    use crate::mem::Ptr;
    Ptr::null()
}

/// `BOOL protocol_conformsToProtocol(Protocol *proto, Protocol *other)`
/// per Apple's Objective-C Runtime Reference
/// (<https://developer.apple.com/documentation/objectivec/1418841-protocol_conformstoprotocol>):
///
/// > Returns a Boolean value that indicates whether one protocol
/// > conforms to another.
pub fn protocol_conformsToProtocol(_env: &mut crate::Environment, proto: id, other: id) -> bool {
    if proto.is_null() || other.is_null() {
        return false;
    }
    proto == other
}

/// `int objc_getClassList(Class *buffer, int bufferLen)` per Apple's
/// Objective-C Runtime Reference
/// (<https://developer.apple.com/documentation/objectivec/1418579-objc_getclasslist>):
///
/// > Returns the number of currently registered classes. If `buffer` is
/// > NULL or `bufferLen` is 0, it must just return the total count.
/// > Otherwise it must copy up to `bufferLen` Class pointers into
/// > `buffer`, but still return the total number registered (callers
/// > use that to detect that they need a bigger buffer).
pub fn objc_getClassList(
    env: &mut crate::Environment,
    buffer: crate::mem::MutPtr<Class>,
    buffer_len: i32,
) -> i32 {
    let total = env.objc.classes.values().count();
    if buffer.is_null() || buffer_len <= 0 {
        return i32::try_from(total).unwrap_or(i32::MAX);
    }
    let cap = u32::try_from(buffer_len).unwrap_or(0);
    let mut offset: u32 = 0;
    for class in env.objc.classes.values() {
        if offset >= cap {
            break;
        }
        env.mem.write(buffer + offset, *class);
        offset += 1;
    }
    i32::try_from(total).unwrap_or(i32::MAX)
}

/// `const char **objc_copyClassNamesForImage(const char *image,
/// unsigned int *outCount)` — enumerate class names defined by a
/// loaded image. We don't track image membership, so write 0 to
/// `outCount` and return NULL. Apps either skip enumeration or hit a
/// safe early-exit.
pub fn objc_copyClassNamesForImage(
    env: &mut crate::Environment,
    _image: ConstPtr<u8>,
    out_count: crate::mem::MutPtr<u32>,
) -> ConstPtr<u8> {
    use crate::mem::Ptr;
    if !out_count.is_null() {
        env.mem.write(out_count, 0);
    }
    Ptr::null()
}

/// `void *object_getIndexedIvars(id obj)` — returns a pointer to the
/// extra bytes allocated for a class created with
/// `objc_allocateClassPair(super, name, extraBytes)`. We don't allocate
/// extra bytes (see [objc_allocateClassPair]), so just return the
/// object pointer itself; callers that try to use it as scratch space
/// will get a no-op buffer rather than crashing on NULL.
pub fn object_getIndexedIvars(_env: &mut crate::Environment, obj: id) -> ConstVoidPtr {
    obj.cast().cast_const()
}

/// `void method_exchangeImplementations(Method m1, Method m2)`
///
/// Per Apple's [Objective-C Runtime Reference](https://developer.apple.com/documentation/objectivec/1418769-method_exchangeimplementations?language=objc):
///
/// > Exchanges the implementations of two methods. This is an atomic
/// > version of the following:
/// > ```
/// > IMP imp1 = method_getImplementation(m1);
/// > IMP imp2 = method_getImplementation(m2);
/// > method_setImplementation(m1, imp2);
/// > method_setImplementation(m2, imp1);
/// > ```
///
/// Because our `Method` handles encode the defining class and selector, we
/// can swap the real entries in each class's method table (along with their
/// type-encoding strings). This makes method swizzling take effect — e.g.
/// cocos2d's `SynthesizeSingleton` macro, whose `NSAssert` used to fail on
/// every frame because the previous implementation was a no-op.
pub fn method_exchangeImplementations(
    env: &mut crate::Environment,
    m1: ConstVoidPtr,
    m2: ConstVoidPtr,
) {
    let (Some((cls1, sel1)), Some((cls2, sel2))) = (
        method_handle_decode(env, m1),
        method_handle_decode(env, m2),
    ) else {
        log!(
            "Warning: method_exchangeImplementations({:?}, {:?}) — unknown Method handle(s); ignoring.",
            m1,
            m2
        );
        return;
    };

    let imp1 = imp_at(env, cls1, sel1);
    let imp2 = imp_at(env, cls2, sel2);

    // Swap type-encoding strings too, so `method_getTypeEncoding` keeps
    // reporting the encoding that matches each installed implementation.
    let types1 = env
        .objc
        .get_host_object(cls1)
        .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
        .and_then(|c| c.guest_method_signatures.get(&sel1).copied());
    let types2 = env
        .objc
        .get_host_object(cls2)
        .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
        .and_then(|c| c.guest_method_signatures.get(&sel2).copied());

    if let Some(imp) = imp2 {
        set_imp_at(env, cls1, sel1, imp);
    }
    if let Some(imp) = imp1 {
        set_imp_at(env, cls2, sel2, imp);
    }
    {
        let class_obj = env.objc.borrow_mut::<ClassHostObject>(cls1);
        match types2 {
            Some(t) => {
                class_obj.guest_method_signatures.insert(sel1, t);
            }
            None => {
                class_obj.guest_method_signatures.remove(&sel1);
            }
        }
    }
    {
        let class_obj = env.objc.borrow_mut::<ClassHostObject>(cls2);
        match types1 {
            Some(t) => {
                class_obj.guest_method_signatures.insert(sel2, t);
            }
            None => {
                class_obj.guest_method_signatures.remove(&sel2);
            }
        }
    }

    log_dbg!(
        "method_exchangeImplementations({:?}, {:?}) — swapped [{:?} {:?}] <-> [{:?} {:?}]",
        m1,
        m2,
        cls1,
        sel1,
        cls2,
        sel2
    );
}

/// `objc_property_t *class_copyPropertyList(Class cls, unsigned int *outCount)` —
/// returns a NULL-terminated, malloc'd array of `objc_property_t` opaque
/// pointers describing every declared `@property` in `cls` (not in its
/// superclasses). Per Apple's Objective-C Runtime Reference
/// (<https://developer.apple.com/documentation/objectivec/1418553-class_copypropertylist>):
///
/// > You should use `free()` to free the array.
/// > If the class declares no properties, the function returns `NULL` and
/// > `*outCount` is `0`.
///
/// We model "declared properties" using the `ClassHostObject::properties`
/// map populated by the runtime when a `@property` directive is processed
/// for a class. Properties inherited from superclasses are intentionally
/// excluded, matching real Apple behaviour.
pub fn class_copyPropertyList(
    env: &mut crate::Environment,
    cls: Class,
    out_count: crate::mem::MutPtr<u32>,
) -> crate::mem::MutPtr<ConstVoidPtr> {
    if !out_count.is_null() {
        env.mem.write(out_count, 0);
    }
    if cls.is_null() {
        return Ptr::null();
    }
    // Collect property pointers WITHOUT walking the hierarchy — class_getProperty
    // walks it (matching the documented `class_getProperty` semantics), but
    // class_copyPropertyList must NOT, per Apple's runtime reference.
    let entries: Vec<ConstVoidPtr> = if let Some(host_obj) = env.objc.get_host_object(cls) {
        if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
            class_obj.properties.values().copied().collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    if entries.is_empty() {
        return Ptr::null();
    }
    // Allocate a NULL-terminated array of `objc_property_t` in guest memory.
    // The Apple manpage doesn't explicitly say it's NULL-terminated, but real
    // libobjc returns a malloc'd array of exactly `*outCount` entries.
    let total = (entries.len() as u32) * guest_size_of::<ConstVoidPtr>();
    let buf: crate::mem::MutPtr<ConstVoidPtr> = env.mem.alloc(total).cast();
    for (i, e) in entries.iter().enumerate() {
        env.mem.write(buf + i as u32, *e);
    }
    if !out_count.is_null() {
        env.mem.write(out_count, entries.len() as u32);
    }
    buf
}

/// `Method *class_copyMethodList(Class cls, unsigned int *outCount)` —
/// returns a malloc'd array of `Method` pointers (one per instance method
/// declared on `cls`). Per Apple's Objective-C Runtime Reference
/// (<https://developer.apple.com/documentation/objectivec/1418490-class_copymethodlist>):
///
/// > You must free the list with `free()`.
/// > If `cls` declares no instance methods, returns `NULL` and `*outCount` is 0.
///
/// touchHLE's `class_getInstanceMethod` returns the class pointer (it
/// doesn't model real `Method` structs), so we mirror that by writing the
/// class pointer once per known selector — callers walking the array can
/// still use the pointers with `method_getImplementation`/`method_setImplementation`
/// or `class_replaceMethod`, which expect this same opaque representation.
pub fn class_copyMethodList(
    env: &mut crate::Environment,
    cls: Class,
    out_count: crate::mem::MutPtr<u32>,
) -> crate::mem::MutPtr<ConstVoidPtr> {
    if !out_count.is_null() {
        env.mem.write(out_count, 0);
    }
    if cls.is_null() {
        return Ptr::null();
    }
    // Collect the selectors defined directly on `cls` (Apple's contract:
    // this call does NOT walk superclasses).
    let sels: Vec<SEL> = if let Some(host_obj) = env.objc.get_host_object(cls) {
        if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
            class_obj.methods.keys().copied().collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    if sels.is_empty() {
        return Ptr::null();
    }
    let total = (sels.len() as u32) * guest_size_of::<ConstVoidPtr>();
    let buf: crate::mem::MutPtr<ConstVoidPtr> = env.mem.alloc(total).cast();
    // Emit a real opaque `Method` handle per selector so callers can use each
    // entry with `method_getName` / `method_getImplementation` /
    // `method_setImplementation` and observe the correct per-method state.
    for (i, sel) in sels.iter().enumerate() {
        let handle = method_handle_for(env, cls, *sel);
        env.mem.write(buf + i as u32, handle);
    }
    if !out_count.is_null() {
        env.mem.write(out_count, sels.len() as u32);
    }
    buf
}

/// `Ivar *class_copyIvarList(Class cls, unsigned int *outCount)` — Apple's
/// documented contract is the same NULL-terminated/malloc'd shape as the
/// other `class_copy*List` calls. touchHLE doesn't model ivars as opaque
/// `Ivar` structs (they're stored in a plain `host_object` map keyed by
/// name), so we return NULL with `*outCount = 0`, which is the documented
/// behaviour for a class that declares no ivars.
pub fn class_copyIvarList(
    env: &mut crate::Environment,
    _cls: Class,
    out_count: crate::mem::MutPtr<u32>,
) -> crate::mem::MutPtr<ConstVoidPtr> {
    if !out_count.is_null() {
        env.mem.write(out_count, 0);
    }
    Ptr::null()
}

/// `Protocol * __unsafe_unretained *class_copyProtocolList(Class cls, unsigned int *outCount)` —
/// same shape. We don't model adopted protocols, so return NULL with
/// `*outCount = 0`, which is also the spec-compliant answer for a class
/// adopting no protocols.
pub fn class_copyProtocolList(
    env: &mut crate::Environment,
    _cls: Class,
    out_count: crate::mem::MutPtr<u32>,
) -> crate::mem::MutPtr<ConstVoidPtr> {
    if !out_count.is_null() {
        env.mem.write(out_count, 0);
    }
    Ptr::null()
}

/// `objc_property_t class_getProperty(Class cls, const char *name)` —
/// returns an opaque pointer to the property metadata for the named
/// declared @property, walking the class hierarchy. Returns NULL if
/// no such property is declared.
///
/// Per Apple's Objective-C Runtime Reference:
/// https://developer.apple.com/documentation/objectivec/1418553-class_getproperty
pub fn class_getProperty(
    env: &mut crate::Environment,
    cls: Class,
    name: ConstPtr<u8>,
) -> ConstVoidPtr {
    if cls.is_null() || name.is_null() {
        return ConstVoidPtr::null();
    }
    let Ok(name_str) = env.mem.cstr_at_utf8(name) else {
        return ConstVoidPtr::null();
    };
    let name_string = name_str.to_string();

    // Walk the class hierarchy looking for the property.
    let mut current = cls;
    loop {
        if let Some(host_obj) = env.objc.get_host_object(current) {
            if let Some(class_obj) = host_obj.as_any().downcast_ref::<ClassHostObject>() {
                if let Some(&prop_ptr) = class_obj.properties.get(&name_string) {
                    return prop_ptr;
                }
                if class_obj.superclass == nil {
                    break;
                }
                current = class_obj.superclass;
                continue;
            }
        }
        break;
    }

    // Property not found in hierarchy — return NULL (spec-compliant).
    ConstVoidPtr::null()
}
