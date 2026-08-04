/*
* This Source Code Form is subject to the terms of the Mozilla Public
* License, v. 2.0.
* If a copy of the MPL was not distributed with this
* file, You can obtain one at https://mozilla.org/MPL/2.0/.
*/
//!
//! `NSBundle`.
use super::{ns_string, NSNotFound, NSRange, NSUInteger};
use crate::bundle::Bundle;
use crate::frameworks::core_foundation::cf_bundle::{
    CFBundleCopyBundleLocalizations, CFBundleCopyPreferredLocalizationsFromArray,
};
use crate::frameworks::foundation::ns_string::{from_rust_string, NSUTF8StringEncoding};
use crate::mem::{ConstVoidPtr, MutPtr, Ptr};
use crate::objc::{
    autorelease, id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject,
    NSZonePtr,
};
use crate::Environment;
use std::collections::{HashMap, HashSet};

// Should be ISO 639-1 (or ISO 639-2) compliant
// Legacy projects use language names while newer ones use language code lprojs
const LANG_ID_TO_LANG_PROJ: &[(&str, &[&str])] = &[
    ("da", &["Danish.lproj", "da.lproj"]),
    ("nl", &["Dutch.lproj", "nl.lproj"]),
    ("en", &["English.lproj", "en.lproj"]),
    ("fi", &["Finnish.lproj", "fi.lproj"]),
    ("fr", &["French.lproj", "fr.lproj"]),
    ("de", &["German.lproj", "de.lproj"]),
    ("it", &["Italian.lproj", "it.lproj"]),
    ("ja", &["Japanese.lproj", "ja.lproj"]),
    ("ko", &["Korean.lproj", "ko.lproj"]),
    ("no", &["Norwegian.lproj", "no.lproj"]),
    ("pt", &["Portuguese.lproj", "pt.lproj"]),
    ("ru", &["Russian.lproj", "ru.lproj"]),
    ("zh", &["Chinese.lproj", "zh.lproj"]),
    ("es", &["Spanish.lproj", "es.lproj"]),
    ("sv", &["Swedish.lproj", "sv.lproj"]),
    ("tr", &["Turkish.lproj", "tr.lproj"]),
];

#[derive(Default)]
pub struct State {
    main_bundle: Option<id>,
    // Keyed by bundle path String → NSBundle*
    bundle_cache: HashMap<String, id>,
    localization_tables: HashMap<id, id>, // NSString* → NSDictionary*
}

#[derive(Default)]
pub struct NSBundleHostObject {
    /// If this is None, this is the main bundle's NSBundle instance and the
    /// Bundle is stored in crate::Environment, not here.
    pub bundle: Option<Bundle>,
    /// NSString* with bundle path.
    bundle_path: id,
    /// NSString* with bundle identifier.
    bundle_identifier: id,
    /// NSURL* with bundle path. None if not created yet.
    bundle_url: Option<id>,
    /// `NSDictionary*` for the `Info.plist` content. None if not created yet.
    info_dictionary: Option<id>,
}

impl HostObject for NSBundleHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation NSBundle: NSObject

// =========================================================================
// MARK: - Class methods / constructors
// =========================================================================

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = NSBundleHostObject {
        bundle: None,
        bundle_path: nil,
        bundle_identifier: nil,
        bundle_url: None,
        info_dictionary: None,
    };
    env.objc.alloc_object(this, Box::new(host_object), &mut env.mem)
}

+ (id)mainBundle {
    if let Some(bundle) = env.framework_state.foundation.ns_bundle.main_bundle {
        bundle
    } else {
        let new = msg_class![env; _touchHLE_NSBundle_Static alloc];
        env.framework_state.foundation.ns_bundle.main_bundle = Some(new);
        new
    }
}

+ (id)bundleForClass:(id)_aClass {
    // Return the main bundle. For single-bundle iPhone apps this is always
    // correct.
    // A full implementation would look up which bundle contains the given
    // class.
    msg_class![env; NSBundle mainBundle]
}

+ (id)bundleWithPath:(id)path { // NSString*
    let new: id = msg![env; this alloc];
    let new: id = msg![env; new initWithPath:path];
    autorelease(env, new)
}

+ (id)bundleWithURL:(id)url { // NSURL*
    let new: id = msg![env; this alloc];
    let new: id = msg![env; new initWithURL:url];
    autorelease(env, new)
}

+ (id)bundleWithIdentifier:(id)identifier { // NSString*
    if identifier == nil {
        return nil;
    }
    // Check main bundle first.
    let main: id = msg_class![env; NSBundle mainBundle];
    let main_id: id = msg![env; main bundleIdentifier];
    if main_id != nil {
        let equal: bool = msg![env; main_id isEqualToString:identifier];
        if equal {
            return main;
        }
    }
    let target_str = ns_string::to_rust_string(env, identifier);
    // ПРАВКА: Собираем бандлы в вектор, чтобы отпустить immutable borrow env.
    let cached_bundles: Vec<id> = env.framework_state.foundation.ns_bundle.bundle_cache.values().copied().collect();
    // Check cached sub-bundles (like Scoreloop)
    for cached_bundle in cached_bundles {
        let cached_id: id = msg![env; cached_bundle bundleIdentifier];
        if cached_id != nil {
            let cached_id_str = ns_string::to_rust_string(env, cached_id);
            if cached_id_str == target_str {
                return cached_bundle;
            }
        }
    }
    log!("Warning: [NSBundle bundleWithIdentifier:{}] not found", target_str);
    nil
}

+ (id)bundleURL {
    if let Some(url) = env.objc.borrow::<NSBundleHostObject>(this).bundle_url {
        url
    } else {
        let bundle_path: id = msg![env; this bundlePath];
        let new: id = msg_class![env; NSURL alloc];
        let new: id = msg![env; new initFileURLWithPath:bundle_path];
        env.objc.borrow_mut::<NSBundleHostObject>(this).bundle_url = Some(new);
        new
    }
}

+ (id)bundlePath {
    env.objc.borrow::<NSBundleHostObject>(this).bundle_path
}

+ (id)resourcePath {
    msg![env; this bundlePath]
}

+ (id)resourceURL {
    msg![env; this bundleURL]
}

+ (id)executablePath {
    let exec_path_str = env.bundle.executable_path().as_str().to_string();
    let exec_path = from_rust_string(env, exec_path_str);
    autorelease(env, exec_path)
}

+ (id)executableURL {
    let exec_path: id = msg![env; this executablePath];
    if exec_path == nil {
        return nil;
    }
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:exec_path];
    autorelease(env, url)
}

+ (id)pathForResource:(id)name          // NSString*
               ofType:(id)extension     // NSString*
          inDirectory:(id)directory {   // NSString*
    let path = path_for_resource_helper(env, this, name, nil, directory, extension);
    if path != nil {
        return path;
    }
    // Try preferred languages in order.
    let langs: id = msg_class![env; NSLocale preferredLanguages];
    let lang_count: NSUInteger = msg![env; langs count];
    let mut unknown_codes = HashSet::new();
    for i in 0..lang_count {
        let lang_code: id = msg![env; langs objectAtIndex:i];
        let lang_code_str = ns_string::to_rust_string(env, lang_code);
        if let Some(&(_, lprojs)) = LANG_ID_TO_LANG_PROJ
            .iter()
            .find(|&&(code, _)| code == lang_code_str)
        {
            for lproj in lprojs {
                let lproj_ns: id = ns_string::get_static_str(env, lproj);
                let localized_path =
                    path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
                if localized_path != nil {
                    return localized_path;
                }
            }
        } else {
            unknown_codes.insert(lang_code_str.into_owned());
        }
    }
    if !unknown_codes.is_empty() {
        log!(
            "TODO: language codes {:?} aren't mapped to a language name, falling back to English",
            unknown_codes
        );
    }
    // Fallback to English.
    for lproj in ["English.lproj", "en.lproj"] {
        let lproj_ns: id = ns_string::get_static_str(env, lproj);
        let path = path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
        if path != nil {
            return path;
        }
    }
    // Base.lproj is Apple's Base internationalization fallback used by
    // storyboards and other resources that don't have a per-language copy.
    {
        let lproj_ns: id = ns_string::get_static_str(env, "Base.lproj");
        let path = path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
        if path != nil {
            return path;
        }
    }
    nil
}

+ (id)allBundles {
    let arr: id = msg_class![env; NSMutableArray new];
    let main: id = msg_class![env; NSBundle mainBundle];
    let _: () = msg![env; arr addObject:main];
    // ПРАВКА: Собираем бандлы в вектор, чтобы отпустить immutable borrow env.
    let cached_bundles: Vec<id> = env.framework_state.foundation.ns_bundle.bundle_cache.values().copied().collect();
    // Include dynamically created sub-bundles (plugins/frameworks)
    for cached_bundle in cached_bundles {
        let _: () = msg![env; arr addObject:cached_bundle];
    }
    autorelease(env, arr)
}

+ (id)allFrameworks {
    // No dynamically loaded frameworks in touchHLE.
    msg_class![env; NSArray array]
}

+ (id)preferredLocalizationsFromArray:(id)localizations_array { // NSArray<NSString*>*
    let preferred = CFBundleCopyPreferredLocalizationsFromArray(env, localizations_array);
    autorelease(env, preferred)
}

+ (id)preferredLocalizationsFromArray:(id)localizations_array
                       forPreferences:(id)_locale_identifiers_array {
    // Ignore the explicit preferences list and fall back to the system default.
    let preferred = CFBundleCopyPreferredLocalizationsFromArray(env, localizations_array);
    autorelease(env, preferred)
}

// =========================================================================
// MARK: - Instance methods / Initialization
// =========================================================================

- (id)init {
    this
}

- (id)initWithPath:(id)path { // NSString*
    if path == nil {
        log_dbg!("NSBundle initWithPath: nil path provided");
        release(env, this);
        return nil;
    }
    let path_str = ns_string::to_rust_string(env, path).into_owned();

    // 1. CACHE CHECK
    if let Some(&cached) = env.framework_state.foundation.ns_bundle.bundle_cache.get(&path_str) {
        release(env, this);
        return retain(env, cached);
    }

    // 2. FILESYSTEM VALIDATION
    let plist_file_path = format!("{}/Info.plist", path_str);
    let plist_guest = crate::fs::GuestPath::new(&plist_file_path);
    let mut dict: id = nil;
    let mut bundle_identifier: id = nil;
    if env.fs.read(plist_guest).is_ok() {
        // Normal Path: Info.plist exists
        let plist_path_ns = ns_string::from_rust_string(env, plist_file_path);
        dict = msg_class![env; NSDictionary alloc];
        dict = msg![env; dict initWithContentsOfFile:plist_path_ns];
        release(env, plist_path_ns);
        if dict != nil {
            let id_key = ns_string::get_static_str(env, "CFBundleIdentifier");
            let val: id = msg![env; dict objectForKey:id_key];
            if val != nil {
                bundle_identifier = retain(env, val);
            }
        }
    }

    let bundle_path_ns = ns_string::from_rust_string(env, path_str.clone());

    // 3. STUB FALLBACK
    // Provide a smart default bundle identifier if Info.plist parsing failed
    if bundle_identifier == nil {
        let last_comp: id = msg![env; bundle_path_ns lastPathComponent];
        if last_comp != nil {
            bundle_identifier = retain(env, last_comp);
        } else {
            bundle_identifier = ns_string::get_static_str(env, "com.unknown.stub");
        }
    }

    // 4. HOST OBJECT INITIALIZATION
    *env.objc.borrow_mut::<NSBundleHostObject>(this) = NSBundleHostObject {
        bundle: None,
        bundle_path: bundle_path_ns,
        bundle_identifier,
        bundle_url: None,
        info_dictionary: if dict != nil { Some(dict) } else { None },
    };

    // 5. CACHE INSERTION
    env.framework_state
        .foundation
        .ns_bundle
        .bundle_cache
        .insert(path_str, this);
    this
}

- (id)initWithURL:(id)url { // NSURL*
    if url == nil {
        release(env, this);
        return nil;
    }
    let path: id = msg![env; url path];
    msg![env; this initWithPath:path]
}

// =========================================================================
// MARK: - Dealloc
// =========================================================================

- (())dealloc {
    // Release ALL owned NSString/NSURL/NSDictionary fields.
    // bundle_path and bundle_identifier are always owned (+1) for non-static
    // bundles; bundle_url and info_dictionary are optional.
    let host = env.objc.borrow::<NSBundleHostObject>(this);
    let bundle_path       = host.bundle_path;
    let bundle_identifier = host.bundle_identifier;
    let bundle_url        = host.bundle_url;
    let info_dictionary   = host.info_dictionary;
    if bundle_path != nil { release(env, bundle_path); }
    if bundle_identifier != nil { release(env, bundle_identifier); }
    if let Some(url)  = bundle_url       { release(env, url); }
    if let Some(dict) = info_dictionary  { release(env, dict); }
    env.objc.dealloc_object(this, &mut env.mem)
}

// =========================================================================
// MARK: - Identity
// =========================================================================

- (id)bundlePath {
    env.objc.borrow::<NSBundleHostObject>(this).bundle_path
}

- (id)bundleIdentifier {
    env.objc.borrow::<NSBundleHostObject>(this).bundle_identifier
}

- (id)bundleURL {
    if let Some(url) = env.objc.borrow::<NSBundleHostObject>(this).bundle_url {
        return url;
    }
    let bundle_path: id = msg![env; this bundlePath];
    if bundle_path == nil {
        return nil;
    }
    let new: id = msg_class![env; NSURL alloc];
    let new: id = msg![env; new initFileURLWithPath:bundle_path];
    env.objc.borrow_mut::<NSBundleHostObject>(this).bundle_url = Some(new);
    new
}

// =========================================================================
// MARK: - Load state
// =========================================================================

- (bool)isLoaded { true }

// `- (BOOL)load;`
//
// Per Apple's [NSBundle Reference](https://developer.apple.com/documentation/foundation/nsbundle/1418338-load):
// "Dynamically loads the bundle's executable code into a running
// program, if the code has not already been loaded." touchHLE links
// every bundle's executable image up-front at app launch (via dyld), so
// there is never any deferred code to load. The documented return
// value for "code already loaded successfully" is YES, which is what
// we return.
- (bool)load {
    true
}

- (bool)unload {
    log!("TODO: [NSBundle unload] — returning NO");
    false
}

- (bool)preflightAndReturnError:(id)_error { true }  // NSError**

// `- (BOOL)loadAndReturnError:(NSError **)error;`
//
// Per Apple's [NSBundle Reference](https://developer.apple.com/documentation/foundation/nsbundle/1417447-loadandreturnerror):
// "On output, if the bundle was not loaded successfully, this contains
// an error object describing why; otherwise, it contains no value."
// touchHLE always treats the bundle as loaded (see -load above), so we
// never populate the out-error and return YES.
- (bool)loadAndReturnError:(id)_error { // NSError**
    true
}

// =========================================================================
// MARK: - NIB loading
// =========================================================================

- (id)loadNibNamed:(id)name
             owner:(id)owner
           options:(id)options {
    if options != nil {
        let options_count: NSUInteger = msg![env; options count];
        assert_eq!(options_count, 0);
    }
    let nib: id = msg_class![env; UINib nibWithNibName:name bundle:this];
    msg![env; nib instantiateWithOwner:owner options:nil]
}

// =========================================================================
// MARK: - Paths and URLs
// =========================================================================

- (id)resourcePath  { msg![env; this bundlePath] }
- (id)resourceURL   { msg![env; this bundleURL]  }

- (id)executablePath {
    let exec_path_str = env.bundle.executable_path().as_str().to_string();
    let exec_path = from_rust_string(env, exec_path_str);
    autorelease(env, exec_path)
}
- (id)executableURL {
    // TODO: cache result
    let exec_path: id = msg![env; this executablePath];
    if exec_path == nil { return nil; }
    msg_class![env; NSURL fileURLWithPath:exec_path]
}

- (id)privateFrameworksPath {
    let base: id = msg![env; this bundlePath];
    let comp: id = ns_string::get_static_str(env, "Frameworks");
    let path: id = msg![env; base stringByAppendingPathComponent:comp];
    autorelease(env, path)
}

- (id)privateFrameworksURL {
    let path: id = msg![env; this privateFrameworksPath];
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:path];
    autorelease(env, url)
}

- (id)sharedFrameworksPath {
    let base: id = msg![env; this bundlePath];
    let comp: id = ns_string::get_static_str(env, "SharedFrameworks");
    let path: id = msg![env; base stringByAppendingPathComponent:comp];
    autorelease(env, path)
}

- (id)sharedFrameworksURL {
    let path: id = msg![env; this sharedFrameworksPath];
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:path];
    autorelease(env, url)
}

- (id)builtInPlugInsPath {
    let base: id = msg![env; this bundlePath];
    let comp: id = ns_string::get_static_str(env, "PlugIns");
    let path: id = msg![env; base stringByAppendingPathComponent:comp];
    autorelease(env, path)
}

- (id)builtInPlugInsURL {
    let path: id = msg![env; this builtInPlugInsPath];
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:path];
    autorelease(env, url)
}

- (id)sharedSupportPath {
    let base: id = msg![env; this bundlePath];
    let comp: id = ns_string::get_static_str(env, "SharedSupport");
    let path: id = msg![env; base stringByAppendingPathComponent:comp];
    autorelease(env, path)
}

- (id)sharedSupportURL {
    let path: id = msg![env; this sharedSupportPath];
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:path];
    autorelease(env, url)
}

- (id)appStoreReceiptURL {
    log!("TODO: [NSBundle appStoreReceiptURL] — returning nil");
    nil
}

// =========================================================================
// MARK: - Resource lookup
// =========================================================================

- (id)pathsForResourcesOfType:(id)ext inDirectory:(id)subpath {
    let ext_str = if ext != nil {
        let s = ns_string::to_rust_string(env, ext);
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    };
    let mut dir_path: id = msg![env; this resourcePath];
    if subpath != nil {
        let subpath_str = ns_string::to_rust_string(env, subpath);
        if !subpath_str.is_empty() {
            dir_path = msg![env; dir_path stringByAppendingPathComponent:subpath];
        }
    }
    let dir_str = ns_string::to_rust_string(env, dir_path);
    let rust_dir_path = std::path::Path::new(dir_str.as_ref());
    let mut actual_dir_str = dir_str.clone();
    // Case-insensitive fallback: если папка не читается напрямую, ищем её у
    // родителя без учёта регистра
    if env.fs.enumerate(crate::fs::GuestPath::new(&dir_str)).is_err() {
        if let (Some(parent), Some(dir_name)) = (rust_dir_path.parent(), rust_dir_path.file_name()) {
            let parent_str = parent.to_str().unwrap_or("");
            let target_name = dir_name.to_str().unwrap_or("").to_lowercase();
            if let Ok(mut entries) = env.fs.enumerate(crate::fs::GuestPath::new(parent_str)) {
                if let Some(real_name) = entries.find(|e| e.to_lowercase() == target_name) {
                    actual_dir_str = format!("{}/{}", parent_str, real_name).into();
                }
            }
        }
    }
    let array: id = msg_class![env; NSMutableArray array];

    // Собираем имена файлов в вектор в отдельном блоке,
    // чтобы заимствование env.fs освободилось до вызовов msg! /
    // from_rust_string
    let matched_files: Vec<String> = {
        let target_ext = ext_str.as_ref().map(|s| s.to_lowercase());
        match env.fs.enumerate(crate::fs::GuestPath::new(&actual_dir_str)) {
            Ok(entries) => {
                entries
                    .filter(|entry| {
                        if let Some(t_ext) = &target_ext {
                            let entry_ext = std::path::Path::new(entry)
                                .extension()
                                .and_then(|e| e.to_str())
                                .map(|s| s.to_lowercase());
                            entry_ext.as_ref() == Some(t_ext)
                        } else {
                            // Если расширение не указано (nil или пустое),
                            // возвращаем все файлы
                            true
                        }
                    })
                    .map(|s| s.to_string())
                    .collect()
            }
            Err(e) => {
                log_dbg!("Warning: pathsForResourcesOfType:inDirectory: could not read directory {:?} (error: {:?})", actual_dir_str, e);
                Vec::new()
            }
        }
    };

    // Теперь env.fs не заимствован — можно безопасно использовать env
    for file_name in matched_files {
        let ns_file_name = ns_string::from_rust_string(env, file_name);
        let full_path: id = msg![env; dir_path stringByAppendingPathComponent:ns_file_name];
        let _: () = msg![env; array addObject:full_path];
    }

    array
}

- (id)pathsForResourcesOfType:(id)ext          // NSString*
                  inDirectory:(id)subpath      // NSString*
              forLocalization:(id)localization { // NSString*
    let effective_subpath: id = if localization != nil {
        let lproj_suffix: id = ns_string::get_static_str(env, ".lproj");
        let lproj_dir: id = msg![env; localization stringByAppendingString:lproj_suffix];
        if subpath != nil {
            msg![env; lproj_dir stringByAppendingPathComponent:subpath]
        } else {
            lproj_dir
        }
    } else {
        subpath
    };
    msg![env; this pathsForResourcesOfType:ext inDirectory:effective_subpath]
}

- (id)URLsForResourcesWithExtension:(id)ext       // NSString*
                       subdirectory:(id)subpath { // NSString*
    let paths: id = msg![env; this pathsForResourcesOfType:ext inDirectory:subpath];
    let count: NSUInteger = msg![env; paths count];
    let result: id = msg_class![env; NSMutableArray new];
    let mut i: NSUInteger = 0;
    while i < count {
        let path: id = msg![env; paths objectAtIndex:i];
        let url: id = msg_class![env; NSURL alloc];
        let url: id = msg![env; url initFileURLWithPath:path];
        let _: () = msg![env; result addObject:url];
        release(env, url);
        i += 1;
    }
    autorelease(env, result)
}

- (id)pathForResource:(id)name          // NSString*
               ofType:(id)extension     // NSString*
          inDirectory:(id)directory {   // NSString*
    let path = path_for_resource_helper(env, this, name, nil, directory, extension);
    if path != nil {
        return path;
    }
    // Try preferred languages in order.
    let langs: id = msg_class![env; NSLocale preferredLanguages];
    let lang_count: NSUInteger = msg![env; langs count];
    let mut unknown_codes = HashSet::new();
    for i in 0..lang_count {
        let lang_code: id = msg![env; langs objectAtIndex:i];
        let lang_code_str = ns_string::to_rust_string(env, lang_code);
        if let Some(&(_, lprojs)) = LANG_ID_TO_LANG_PROJ
            .iter()
            .find(|&&(code, _)| code == lang_code_str)
        {
            for lproj in lprojs {
                let lproj_ns: id = ns_string::get_static_str(env, lproj);
                let localized_path =
                    path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
                if localized_path != nil {
                    return localized_path;
                }
            }
        } else {
            unknown_codes.insert(lang_code_str.into_owned());
        }
    }
    if !unknown_codes.is_empty() {
        log!(
            "TODO: language codes {:?} aren't mapped to a language name, falling back to English",
            unknown_codes
        );
    }
    // Fallback to English.
    for lproj in ["English.lproj", "en.lproj"] {
        let lproj_ns: id = ns_string::get_static_str(env, lproj);
        let path = path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
        if path != nil {
            return path;
        }
    }
    // Base.lproj is Apple's Base internationalization fallback used by
    // storyboards and other resources that don't have a per-language copy.
    {
        let lproj_ns: id = ns_string::get_static_str(env, "Base.lproj");
        let path = path_for_resource_helper(env, this, name, lproj_ns, directory, extension);
        if path != nil {
            return path;
        }
    }
    nil
}

- (id)pathForResource:(id)name        // NSString*
               ofType:(id)extension { // NSString*
    msg![env; this pathForResource:name ofType:extension inDirectory:nil]
}

- (id)pathForResource:(id)name           // NSString*
               ofType:(id)extension      // NSString*
          inDirectory:(id)directory      // NSString*
      forLocalization:(id)localization { // NSString*
    if localization != nil {
        let lproj_suffix: id = ns_string::get_static_str(env, ".lproj");
        let lproj_dir: id = msg![env; localization stringByAppendingString:lproj_suffix];
        let path = path_for_resource_helper(env, this, name, lproj_dir, directory, extension);
        if path != nil {
            return path;
        }
    }
    msg![env; this pathForResource:name ofType:extension inDirectory:directory]
}

- (id)URLForResource:(id)name          // NSString*
       withExtension:(id)extension     // NSString*
        subdirectory:(id)subpath {     // NSString*
    let path_string: id = msg![env; this pathForResource:name ofType:extension inDirectory:subpath];
    if path_string == nil { return nil; }
    let url: id = msg_class![env; NSURL alloc];
    let url: id = msg![env; url initFileURLWithPath:path_string];
    autorelease(env, url)
}

- (id)URLForResource:(id)name          // NSString*
       withExtension:(id)extension {   // NSString*
    msg![env; this URLForResource:name withExtension:extension subdirectory:nil]
}

- (id)URLForResource:(id)name          // NSString*
       withExtension:(id)extension     // NSString*
        subdirectory:(id)subpath       // NSString*
        localization:(id)localization { // NSString*
    if localization != nil {
        let lproj_suffix: id = ns_string::get_static_str(env, ".lproj");
        let lproj_dir: id = msg![env; localization stringByAppendingString:lproj_suffix];
        let effective_subpath: id = if subpath != nil {
            msg![env; lproj_dir stringByAppendingPathComponent:subpath]
        } else {
            lproj_dir
        };
        let path: id = msg![env; this pathForResource:name
                                             ofType:extension
                                        inDirectory:effective_subpath];
        if path != nil {
            let url: id = msg_class![env; NSURL alloc];
            let url: id = msg![env; url initFileURLWithPath:path];
            return autorelease(env, url);
        }
    }
    msg![env; this URLForResource:name withExtension:extension subdirectory:subpath]
}

// =========================================================================
// MARK: - Info dictionary
// =========================================================================

- (id)infoDictionary {
    if let Some(dict) = env.objc.borrow::<NSBundleHostObject>(this).info_dictionary {
        return dict;
    }
    let bundle_path = env.objc.borrow::<NSBundleHostObject>(this).bundle_path;
    if bundle_path == nil {
        return nil;
    }
    let plist_comp = ns_string::get_static_str(env, "Info.plist");
    let plist_path: id = msg![env; bundle_path stringByAppendingPathComponent:plist_comp];
    let dict: id = msg_class![env; NSDictionary alloc];
    let dict: id = msg![env; dict initWithContentsOfFile:plist_path];
    env.objc.borrow_mut::<NSBundleHostObject>(this).info_dictionary = Some(dict);
    dict
}

- (id)objectForInfoDictionaryKey:(id)key {
    let info_dict: id = msg![env; this infoDictionary];
    msg![env; info_dict objectForKey:key]
}

- (id)localizedInfoDictionary {
    log!("TODO: [NSBundle localizedInfoDictionary] — returning plain infoDictionary");
    msg![env; this infoDictionary]
}

// =========================================================================
// MARK: - Localization
// =========================================================================

- (id)localizedStringForKey:(id)key
                      value:(id)value
                      table:(id)tableName {
    // 1. Debug logging (essential for tracking what the game is looking for)
    log_dbg!(
        "localizedStringForKey key:'{}' table:'{}'",
        if key == nil { "nil".into() } else { ns_string::to_rust_string(env, key) },
        if tableName == nil { "Localizable".into() } else { ns_string::to_rust_string(env, tableName) }
    );
    let empty_str: id = ns_string::get_static_str(env, "");
    // 2. Early exit for nil keys
    if key == nil {
        return if value == nil { empty_str } else { value };
    }
    // 3. Determine the table name
    let name = if tableName == nil {
        ns_string::get_static_str(env, "Localizable")
    } else {
        tableName
    };
    // 4. Bundle Check
    // We should allow table lookup on ANY bundle that has a path.
    let host = env.objc.borrow::<NSBundleHostObject>(this);
    let is_valid_bundle = host.bundle_path != nil;
    if !is_valid_bundle {
        return if value != nil && value != empty_str { value } else { key };
    }
    // 5. Localization Table Lookup & Caching
    // We cache dictionaries per-bundle/per-table to prevent repeated IO
    let dict = if let Some(&table_dict) = env
        .framework_state
        .foundation
        .ns_bundle
        .localization_tables
        .get(&name)
    {
        table_dict
    } else {
        let extension = ns_string::get_static_str(env, "strings");
        // Attempt to find the [Table].strings file
        let dict_url: id = msg![env; this URLForResource:name withExtension:extension];
        if dict_url == nil {
            // Log that a translation table is missing (common in many Gamevil
            // ports)
            log_dbg!("Localization table '{}.strings' not found, using fallback",
                ns_string::to_rust_string(env, name));
            return if value == nil || value == empty_str { key } else { value };
        }
        // Load the strings file into a dictionary
        let dict: id = msg_class![env; NSDictionary dictionaryWithContentsOfURL:dict_url];
        let dict: id = if dict != nil {
            dict
        } else {
            // Else, load as standard format
            load_strings_as_standard_format(env, dict_url)
        };
        if dict == nil {
            return if value == nil || value == empty_str { key } else { value };
        }
        // Store in cache to avoid re-loading from disk
        retain(env, name);
        retain(env, dict);
        env.framework_state
            .foundation
            .ns_bundle
            .localization_tables
            .insert(name, dict);
        dict
    };
    // 6. Final String Extraction
    let res: id = msg![env; dict objectForKey:key];
    if res == nil {
        // Return the 'value' if provided, otherwise the 'key' itself
        return if value == nil || value == empty_str { key } else { value };
    }
    res
}

- (id)localizations {
    let localizations = CFBundleCopyBundleLocalizations(env, this);
    autorelease(env, localizations)
}

- (id)preferredLocalizations {
    let loc_array = CFBundleCopyBundleLocalizations(env, this);
    let preferred = CFBundleCopyPreferredLocalizationsFromArray(env, loc_array);
    autorelease(env, preferred)
}

- (id)developmentLocalization {
    // Read CFBundleDevelopmentRegion from Info.plist; fall back to "en".
    let key: id = ns_string::get_static_str(env, "CFBundleDevelopmentRegion");
    let val: id = msg![env; this objectForInfoDictionaryKey:key];
    if val != nil { val } else { ns_string::get_static_str(env, "en") }
}

- (id)localizedStringForKey:(id)key
                      value:(id)value
                      table:(id)tableName
               localization:(id)_localization {
    // Ignore the explicit localization hint and use the system preference.
    msg![env; this localizedStringForKey:key value:value table:tableName]
}

// =========================================================================
// MARK: - Class lookup
// =========================================================================

- (id)classNamed:(id)class_name { // NSString*
    if class_name == nil { return nil; }
    let name_str = ns_string::to_rust_string(env, class_name);
    log_dbg!("[NSBundle classNamed:{}]", name_str);
    // Apple documents -classNamed: as returning `Nil` when no class with the
    // given name is loaded. We must NOT fall back to get_known_class here:
    // that installs an UnimplementedClass placeholder for unknown (or, worse,
    // empty/garbage) names. A placeholder is non-nil, so the caller's typical
    // `if ((c = [bundle classNamed:name])) { obj = [c new]; }` probe wrongly
    // succeeds, then sends `+new` to a class that does not actually exist —
    // which returns nil and leaves the app in a broken state (observed with a
    // corrupted/empty guest class name producing a `Class "" ... +new` call).
    // try_get_known_class returns a real/linkable class when we have one and
    // None otherwise, matching the documented contract. This mirrors the same
    // fix already applied to NSClassFromString.
    let class = env
        .objc
        .try_get_known_class(&name_str, &mut env.mem)
        .unwrap_or(nil);
    if class == nil {
        log!("Warning: [NSBundle classNamed:{}] — class not found", name_str);
    }
    class
}

- (id)principalClass {
    let key: id = ns_string::get_static_str(env, "NSPrincipalClass");
    let val: id = msg![env; this objectForInfoDictionaryKey:key];
    if val == nil { return nil; }
    msg![env; this classNamed:val]
}

@end

// =========================================================================
// MARK: - _touchHLE_NSBundle_Static (main bundle — never released)
// =========================================================================

@implementation _touchHLE_NSBundle_Static: NSBundle

+ (id)allocWithZone:(NSZonePtr)_zone {
    let bundle_path = env.bundle.bundle_path().as_str().to_string();
    let bundle_path = ns_string::from_rust_string(env, bundle_path);
    let bundle_identifier = env.bundle.bundle_identifier().to_string();
    let bundle_identifier = ns_string::from_rust_string(env, bundle_identifier);
    let host_object = NSBundleHostObject {
        bundle: None,
        bundle_path,
        bundle_identifier,
        bundle_url: None,
        info_dictionary: None,
    };
    env.objc.alloc_object(this, Box::new(host_object), &mut env.mem)
}

// Main bundle is a singleton — ignore retain/release/autorelease.
- (id)retain      { this }
- (())release     {}
- (id)autorelease { this }

@end

};

// =========================================================================
// MARK: - path_for_resource_helper
// =========================================================================

// =========================================================================
// MARK: - path_for_resource_helper
// =========================================================================

fn path_for_resource_helper(
    env: &mut Environment,
    bundle: id,
    name: id,
    lproj: id,     // Ожидается кодом ниже
    directory: id, // Ожидается кодом ниже
    extension: id,
) -> id {
    if name == nil {
        // В реальной iOS метод pathForResource:ofType: при name == nil обязан
        // возвращать nil.
        return nil;
    }

    let mut path: id = msg![env; bundle resourcePath];
    if lproj != nil {
        path = msg![env; path stringByAppendingPathComponent:lproj];
    }
    if directory != nil {
        path = msg![env; path stringByAppendingPathComponent:directory];
    }

    // Честное поведение iOS: никаких костылей для PvZ.
    // Если name = @"", мы ничего не приклеиваем.
    // path останется директорией ресурсов (напр. .../ZumaHD.app), что является
    // легальным путем.
    let name_str = ns_string::to_rust_string(env, name);
    if name_str.is_empty() {
        // Empty-name compatibility shim for SexyAppBase-derived games such as
        // Plants vs. Zombies 1.x / Bejeweled 2 / Zuma's Revenge. Those games
        // call `[[NSBundle mainBundle] pathForResource:@"" ofType:nil]` and
        // then run the result through `SexyAppBase::GetFileDir()`, which
        // *strips the last path component*. On real iOS 2.x the value
        // returned points at a file inside the bundle (effectively the
        // executable), so stripping its last component yields the bundle
        // directory and resource lookups like
        // `<bundleDir>/resources.xml` succeed.
        //
        // Returning the bundle directory itself (the strict "modern iOS"
        // behaviour) breaks this idiom: GetFileDir() then strips one level
        // too many and the games look for their assets in the parent of the
        // .app bundle, where nothing exists. That presented as a black
        // screen because asset loading aborted before any UI was drawn.
        //
        // Pointing at the executable path mirrors the layout SexyAppBase
        // games actually expect and is harmless for callers that just use
        // the value as a directory hint.
        let exec_path_str = env.bundle.executable_path().as_str().to_string();
        return ns_string::from_rust_string(env, exec_path_str);
    }
    // Real iOS looks for the device-specific file first and only falls back to
    // the plain name, so try the candidates in that order.
    let base_path = path;
    let mut names: Vec<String> = Vec::new();
    if let Some(variant) = device_specific_resource_name(env, &name_str) {
        names.push(variant);
    }
    names.push(name_str.into_owned());

    let file_manager: id = msg_class![env; NSFileManager defaultManager];
    for candidate in names {
        let candidate: id = ns_string::from_rust_string(env, candidate);
        path = msg![env; base_path stringByAppendingPathComponent:candidate];

        if extension != nil {
            let ext_str = ns_string::to_rust_string(env, extension);
            if !ext_str.is_empty() {
                path = msg![env; path stringByAppendingPathExtension:extension];
            }
        }

        let file_exists: bool = msg![env; file_manager fileExistsAtPath:path];
        if file_exists {
            return path;
        }
        // Otherwise fall through with `path` left pointing at the last
        // (plain-name) candidate, which the fallbacks below work from.
    }

    // Unity iOS players keep their serialized data files in a sibling Data
    // directory inside the app bundle rather than at the bundle root.
    // NSBundle's normal lookup remains first; this fallback only applies when
    // the requested resource is not found there.
    let data_component = ns_string::get_static_str(env, "Data");
    let data_path: id = msg![env; path stringByAppendingPathComponent:data_component];
    let data_path: id = msg![env; data_path stringByAppendingPathComponent:name];
    let data_path_exists: bool = msg![env; file_manager fileExistsAtPath:data_path];
    log!(
        "NSBundle resource lookup: {:?} missing, Unity Data fallback {:?} exists={}",
        path,
        data_path,
        data_path_exists
    );
    if data_path_exists {
        return data_path;
    }

    // Case-insensitive fallback: scan the parent directory.
    let path_str = ns_string::to_rust_string(env, path);
    let rust_path = std::path::Path::new(path_str.as_ref());
    if let (Some(parent), Some(file_name)) = (rust_path.parent(), rust_path.file_name()) {
        let parent_str = parent.to_str().unwrap_or("");
        let target_name = file_name.to_str().unwrap_or("").to_lowercase();
        let parent_guest = crate::fs::GuestPath::new(parent_str);
        // Collect all entries first so env.fs borrow is dropped before we call
        // from_rust_string (which needs a mutable borrow on env).
        let found: Option<String> = env.fs.enumerate(parent_guest).ok().and_then(|mut entries| {
            entries
                .find(|e| e.to_lowercase() == target_name)
                .map(|e| format!("{}/{}", parent_str, e))
        });
        if let Some(full) = found {
            return ns_string::from_rust_string(env, full);
        }
    }
    nil
}

/// The device-specific filename `NSBundle` looks for before the plain one:
/// `Foo~ipad.nib` on iPad, `Foo~iphone.nib` on iPhone and iPod touch.
///
/// Universal apps ship both variants and rely on this resolution happening
/// inside `NSBundle`. The Sims Medieval for iPad keeps an iPhone
/// `MainWindow.nib`, whose `EAGLView` is the 320x460 Interface Builder iPhone
/// canvas, right next to its iPad one; loading the plain name left the game
/// drawing into the top-left corner of a 768x1024 window with the rest of the
/// screen blank.
///
/// Returns `None` if the name already carries a device suffix, or in headless
/// mode where there is no device family to ask about.
fn device_specific_resource_name(env: &Environment, name: &str) -> Option<String> {
    let suffix = if env.window.as_ref()?.device_family().is_ipad() {
        "~ipad"
    } else {
        "~iphone"
    };

    // The suffix goes on the stem of the *last* path component, before any
    // extension the name already carries: `gfx/menu.png` -> `gfx/menu~ipad.png`.
    // Only a dot in the last component counts as an extension separator, so
    // directory names containing dots (`gfx/1.5/tiles`) aren't split.
    let last_component = name.rfind('/').map_or(0, |slash| slash + 1);
    let stem_end = name[last_component..]
        .rfind('.')
        .map_or(name.len(), |dot| last_component + dot);
    let (stem, extension) = name.split_at(stem_end);
    if stem.ends_with("~ipad") || stem.ends_with("~iphone") {
        return None;
    }

    Some(format!("{stem}{suffix}{extension}"))
}

/// Helper function which loads a `strings` file from an `dict_url` and parses
/// it as standard format - one or more key-value pairs along with optional
/// comments. Returned dictionary is autoreleased, so it's a responsibility of
/// the caller to retain it.
/// [String Resources reference](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/LoadingResources/Strings/Strings.html#//apple_ref/doc/uid/10000051i-CH6)
fn load_strings_as_standard_format(env: &mut Environment, dict_url: id) -> id {
    let res: id = msg_class![env; NSMutableDictionary new];
    // TODO: avoid loading whole file in memory
    let data: id = msg_class![env; NSData dataWithContentsOfURL:dict_url];
    assert!(data != nil); // TODO
    let length: NSUInteger = msg![env; data length];
    assert!(length > 2);
    let bytes: ConstVoidPtr = msg![env; data bytes];
    let maybe_bom = env.mem.bytes_at(bytes.cast(), 2);
    assert!(maybe_bom[0..2] != [0xFE, 0xFF] && maybe_bom[0..2] != [0xFF, 0xFE]); // TODO: UTF-16 cases
    let strings_str = msg_class![env; NSString alloc];
    let strings_str: id = msg![env; strings_str initWithData:data encoding:NSUTF8StringEncoding];
    assert!(strings_str != nil); // TODO

    let comment_start = ns_string::get_static_str(env, "/*");
    let comment_end = ns_string::get_static_str(env, "*/");
    let equal_sign = ns_string::get_static_str(env, "=");
    let semicolon = ns_string::get_static_str(env, ";");

    let null_ptr: MutPtr<id> = Ptr::null();

    let scanner: id = msg_class![env; NSScanner scannerWithString:strings_str];
    release(env, strings_str);
    while !msg![env; scanner isAtEnd] {
        while msg![env; scanner scanString:comment_start intoString:null_ptr] {
            // Assume no nested comments!
            let _: bool = msg![env; scanner scanUpToString:comment_end intoString:null_ptr];
            let has_comment_end: bool =
                msg![env; scanner scanString:comment_end intoString:null_ptr];
            assert!(has_comment_end);
            if msg![env; scanner isAtEnd] {
                break;
            }
        }
        if msg![env; scanner isAtEnd] {
            break;
        }
        let key: id = scan_quoted_sanitized(env, scanner);

        let _: bool = msg![env; scanner scanUpToString:equal_sign intoString:null_ptr];
        let has_equal_sign: bool = msg![env; scanner scanString:equal_sign intoString:null_ptr];
        assert!(has_equal_sign);

        let val: id = scan_quoted_sanitized(env, scanner);

        let has_semicolon: bool = msg![env; scanner scanString:semicolon intoString:null_ptr];
        assert!(has_semicolon);

        log_dbg!(
            "Parsed strings: '{}' -> '{}'",
            ns_string::to_rust_string(env, key),
            ns_string::to_rust_string(env, val)
        );
        () = msg![env; res setObject:val forKey:key];
    }

    let res_imm = msg![env; res copy];
    release(env, res);
    autorelease(env, res_imm)
}

fn scan_quoted_sanitized(env: &mut Environment, scanner: id) -> id {
    let quote = ns_string::get_static_str(env, "\"");
    let null_ptr: MutPtr<id> = Ptr::null();
    let res_ptr: MutPtr<id> = env.mem.alloc_and_write(Ptr::null());

    let orig_skip_set = msg![env; scanner charactersToBeSkipped];
    retain(env, orig_skip_set);

    let has_open_quote: bool = msg![env; scanner scanString:quote intoString:null_ptr];
    assert!(has_open_quote);
    // Should not skip chars at the beginning!
    () = msg![env; scanner setCharactersToBeSkipped:nil];
    let _: bool = msg![env; scanner scanUpToString:quote intoString:res_ptr];
    () = msg![env; scanner setCharactersToBeSkipped:orig_skip_set];
    release(env, orig_skip_set);
    let has_end_quote: bool = msg![env; scanner scanString:quote intoString:null_ptr];
    assert!(has_end_quote);

    let res = env.mem.read(res_ptr);
    env.mem.free(res_ptr.cast());
    assert!(res != nil); // TODO

    // TODO: implement generic parsing approach for unquoting
    let quoted_newline: id = ns_string::get_static_str(env, "\\n");
    let unquoted_newline: id = ns_string::get_static_str(env, "\n");
    let res = msg![env; res stringByReplacingOccurrencesOfString:quoted_newline withString:unquoted_newline];

    let backslash = ns_string::get_static_str(env, "\\");
    let range: NSRange = msg![env; res rangeOfString:backslash];
    assert!(range.location == NSNotFound as NSUInteger); // TODO
    res
}
