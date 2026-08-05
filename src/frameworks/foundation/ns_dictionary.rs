/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! The `NSDictionary` class cluster, including `NSMutableDictionary`.

use super::ns_array::ArrayHostObject;
use super::ns_property_list_serialization::{
    deserialize_plist_from_file, NSPropertyListBinaryFormat_v1_0,
};
use super::ns_string::{from_rust_string, get_static_str, to_rust_string};
use super::{
    _nib_archive_decoder, ns_array, ns_keyed_archiver, ns_keyed_unarchiver, ns_string, ns_url,
    NSComparisonResult, NSUInteger,
};
use crate::abi::{CallFromHost, GuestFunction, VaList};
use crate::frameworks::core_foundation::{CFHashCode, CFIndex};
use crate::frameworks::foundation::ns_enumerator::{
    fast_enumeration_helper, NSFastEnumerationState,
};
use crate::frameworks::foundation::ns_file_manager::{
    NSFileModificationDate, NSFileSize, NSFileType,
};
use crate::fs::GuestPath;
use crate::libc::stdlib::qsort::qsort_generic;
use crate::mem::{ConstPtr, MutPtr, Ptr, SafeRead};
use crate::objc::{
    autorelease, id, msg, msg_class, msg_send, nil, objc_classes, release, retain, todo_objc_setter, Class,
    ClassExports, HostObject, NSZonePtr, SEL,
};
use crate::{impl_HostObject_with_superclass, Environment};
use std::collections::hash_map::Entry;
use std::collections::HashMap;

/// Alias for the return type of the `hash` method of the `NSObject` protocol.
type Hash = NSUInteger;

/// Belongs to _touchHLE_NSDictionary, also used by _touchHLE_NSSet
#[derive(Debug, Default)]
pub(super) struct DictionaryHostObject {
    /// Since we need custom hashing and custom equality, and these both need a
    /// `&mut Environment`, we can't just use a `HashMap<id, id>`.
    /// So here we are using a `HashMap` as a primitive for implementing a
    /// hash-map, which is not ideally efficient. :)
    /// The keys are the hash values, the values are a list of key-value pairs
    /// where the keys have the same hash value.
    pub(super) map: HashMap<Hash, Vec<(id, id)>>,
    pub(super) count: NSUInteger,
}
impl HostObject for DictionaryHostObject {}
impl DictionaryHostObject {
    pub(super) fn lookup(&self, env: &mut Environment, key: id) -> id {
        let hash: Hash = msg![env; key hash];
        let Some(collisions) = self.map.get(&hash) else {
            return nil;
        };
        for &(candidate_key, value) in collisions {
            if candidate_key == key || msg![env; candidate_key isEqual:key] {
                return value;
            }
        }
        nil
    }
    pub(super) fn insert(&mut self, env: &mut Environment, key: id, value: id, copy_key: bool) {
        let key: id = if copy_key {
            msg![env; key copy]
        } else {
            retain(env, key)
        };
        let hash: Hash = msg![env; key hash];

        let value = retain(env, value);
        let Some(collisions) = self.map.get_mut(&hash) else {
            self.map.insert(hash, vec![(key, value)]);
            self.count += 1;
            return;
        };
        for &mut (candidate_key, ref mut existing_value) in collisions.iter_mut() {
            if candidate_key == key || msg![env; candidate_key isEqual:key] {
                release(env, *existing_value);
                *existing_value = value;
                return;
            }
        }
        collisions.push((key, value));
        self.count += 1;
    }
    pub(super) fn remove(&mut self, env: &mut Environment, key: id) {
        let hash: Hash = msg![env; key hash];
        let Some(collisions) = self.map.get_mut(&hash) else {
            return;
        };
        let Some(idx) = collisions.iter().position(|&(candidate_key, _)| {
            candidate_key == key || msg![env; candidate_key isEqual:key]
        }) else {
            return;
        };
        let (existing_key, value) = collisions[idx];
        release(env, existing_key);
        release(env, value);
        collisions.remove(idx);
        self.count -= 1;
    }
    pub(super) fn release(&mut self, env: &mut Environment) {
        for collisions in self.map.values() {
            for &(key, value) in collisions {
                release(env, key);
                release(env, value);
            }
        }
    }
    pub(super) fn iter_keys(&self) -> impl Iterator<Item = id> + '_ {
        self.map.values().flatten().map(|&(key, _value)| key)
    }
}

// TODO: move those definitions to cf_dictionary.rs
// Right now they are here because we're too tied to
// NSDictionary internals, but separation could be cleaner?
#[repr(C, packed)]
pub struct CFDictionaryKeyCallBacks {
    pub version: CFIndex,         // version
    pub retain: GuestFunction,    // const void *(*retain)(CFAllocatorRef, const void *value)
    pub release: GuestFunction,   // void (*release)(CFAllocatorRef alloc, const void *val)
    pub copy_desc: GuestFunction, // CFStringRef (*copy_desc)(const void *val)
    pub equal: GuestFunction,     // Boolean (*equal)(const void *val1, const void *val2)
    pub hash: GuestFunction,      // CFHashCode (*hash)(const void *val)
}
unsafe impl SafeRead for CFDictionaryKeyCallBacks {}

#[repr(C, packed)]
pub struct CFDictionaryValueCallBacks {
    pub version: CFIndex,         // version
    pub retain: GuestFunction,    // const void *(*retain)(CFAllocatorRef, const void *value)
    pub release: GuestFunction,   // void (*release)(CFAllocatorRef alloc, const void *val)
    pub copy_desc: GuestFunction, // CFStringRef (*copy_desc)(const void *val)
    pub equal: GuestFunction,     // Boolean (*equal)(const void *val1, const void *val2)
}
unsafe impl SafeRead for CFDictionaryValueCallBacks {}

/// The choice of implementing CFDictionary as subclass
/// of NSDictionary is not a hard truth but a reflection
/// on the omnipresence of current NSDictionary implementation
/// as base of NSSet or usage of internals for property lists.
/// It's probably desirable to implement NSDictionary _atop of_
/// CFDictionary instead, but this requires considerable
/// refactoring, which I'm not very comfortable to do on
/// partially tested codebase (we do not have ability right
/// now to test NS objects directly, only CF variants ;( )
/// See TODO comment on the impl too.
pub struct CFDictionaryHostObject {
    superclass: DictionaryHostObject,
    /// `CFDictionaryKeyCallBacks`
    key_callbacks: CFDictionaryKeyCallBacks,
    /// `CFDictionaryValueCallBacks`
    value_callbacks: CFDictionaryValueCallBacks,
}
impl_HostObject_with_superclass!(CFDictionaryHostObject);
impl Default for CFDictionaryHostObject {
    fn default() -> Self {
        CFDictionaryHostObject {
            superclass: Default::default(),
            key_callbacks: CFDictionaryKeyCallBacks {
                version: 0, // version is always 0
                retain: GuestFunction::null_ptr(),
                release: GuestFunction::null_ptr(),
                copy_desc: GuestFunction::null_ptr(),
                equal: GuestFunction::null_ptr(),
                hash: GuestFunction::null_ptr(),
            },
            value_callbacks: CFDictionaryValueCallBacks {
                version: 0, // version is always 0
                retain: GuestFunction::null_ptr(),
                release: GuestFunction::null_ptr(),
                copy_desc: GuestFunction::null_ptr(),
                equal: GuestFunction::null_ptr(),
            },
        }
    }
}
// TODO: Unify implementations of NSDictionary and CFDictionary
impl CFDictionaryHostObject {
    fn lookup(&self, env: &mut Environment, key: id) -> id {
        let hash = self.hash(env, key);
        let Some(collisions) = self.superclass.map.get(&hash) else {
            return nil;
        };
        for &(candidate_key, value) in collisions {
            if self.equal_keys(env, candidate_key, key) {
                return value;
            }
        }
        nil
    }
    fn insert(&mut self, env: &mut Environment, key: id, value: id) {
        let hash = self.hash(env, key);
        let key = self.retain_key(env, key);
        let value = self.retain_value(env, value);
        self.superclass.count += 1;
        if let Entry::Vacant(e) = self.superclass.map.entry(hash) {
            e.insert(vec![(key, value)]);
            return;
        };
        // remove if present (count will be decremented if necessary)
        self.remove(env, key);
        // Don't assume the bucket still exists: `remove()` compares keys by
        // running guest code — a `CFDictionary` key callback, or an ObjC
        // `hash`/`isEqual:` message — which can re-enter and mutate this very
        // dictionary (clearing it, for instance). Unwrapping here turned that
        // into an emulator panic with no indication of which `unwrap()` fired.
        self.superclass
            .map
            .entry(hash)
            .or_default()
            .push((key, value));
    }
    fn remove(&mut self, env: &mut Environment, key: id) -> bool {
        let hash = self.hash(env, key);
        let Some(collisions) = self.superclass.map.get(&hash) else {
            return false;
        };
        let maybe_pos = collisions
            .iter()
            .position(|&(candidate_key, _)| self.equal_keys(env, candidate_key, key));
        if let Some(pos) = maybe_pos {
            // `equal_keys` above runs guest code, so the bucket may have been
            // removed or shortened while we were deciding what to remove from
            // it. Re-check rather than unwrapping and indexing blindly.
            let Some(collisions) = self.superclass.map.get_mut(&hash) else {
                return false;
            };
            if pos >= collisions.len() {
                return false;
            }
            let (existing_key, existing_value) = collisions.remove(pos);
            self.release_key(env, existing_key);
            self.release_value(env, existing_value);
            self.superclass.count -= 1;
            true
        } else {
            false
        }
    }
    // helpers
    fn hash(&self, env: &mut Environment, key: id) -> CFHashCode {
        let hash_func = self.key_callbacks.hash;
        if hash_func.to_ptr().is_null() {
            // use the pointer value as a hash code
            key.to_bits()
        } else {
            hash_func.call_from_host(env, (key,))
        }
    }
    fn equal_keys(&self, env: &mut Environment, key1: id, key2: id) -> bool {
        let equal_func = self.key_callbacks.equal;
        if equal_func.to_ptr().is_null() {
            // pointer equality
            key1 == key2
        } else {
            equal_func.call_from_host(env, (key1, key2))
        }
    }
    fn retain_key(&mut self, env: &mut Environment, key: id) -> id {
        let key_retain_func = self.key_callbacks.retain;
        if key_retain_func.to_ptr().is_null() {
            key
        } else {
            // TODO: custom dict allocator
            key_retain_func.call_from_host(env, (nil, key))
        }
    }
    fn release_key(&mut self, env: &mut Environment, key: id) {
        let key_release_func = self.key_callbacks.release;
        if !key_release_func.to_ptr().is_null() {
            // TODO: custom dict allocator
            key_release_func.call_from_host(env, (nil, key))
        }
    }
    fn retain_value(&mut self, env: &mut Environment, value: id) -> id {
        let value_retain_func = self.value_callbacks.retain;
        if value_retain_func.to_ptr().is_null() {
            value
        } else {
            // TODO: custom dict allocator
            value_retain_func.call_from_host(env, (nil, value))
        }
    }
    fn release_value(&mut self, env: &mut Environment, value: id) {
        let value_release_func = self.value_callbacks.release;
        if !value_release_func.to_ptr().is_null() {
            // TODO: custom dict allocator
            value_release_func.call_from_host(env, (nil, value))
        }
    }
}

/// Helper to enable sharing `dictionaryWithObjectsAndKeys:` and
/// `initWithObjectsAndKeys:`' implementations without vararg passthrough.
pub fn init_with_objects_and_keys(
    env: &mut Environment,
    this: id,
    first_object: id,
    mut va_args: VaList,
) -> id {
    let first_key: id = va_args.next(env);
    // Spec: `dictionaryWithObjectsAndKeys:` should @throw if the first key is
    // nil. We log + return an empty dictionary instead of panicking the host.
    let mut host_object = <DictionaryHostObject as Default>::default();
    if first_key == nil {
        log!(
            "Warning: dictionaryWithObjectsAndKeys:/initWithObjectsAndKeys: first key is nil; \
             returning empty dictionary."
        );
        *env.objc.borrow_mut(this) = host_object;
        return this;
    }
    host_object.insert(env, first_key, first_object, /* copy_key: */ true);

    loop {
        let object: id = va_args.next(env);
        if object == nil {
            break;
        }
        let key: id = va_args.next(env);
        if key == nil {
            log!(
                "Warning: dictionaryWithObjectsAndKeys:/initWithObjectsAndKeys: \
                 nil key for value {:?}; truncating dictionary here.",
                object
            );
            break;
        }
        host_object.insert(env, key, object, /* copy_key: */ true);
    }

    *env.objc.borrow_mut(this) = host_object;

    this
}

/// Helper function to share `initWithDictionary:` implementations
fn init_with_dictionary_common(env: &mut Environment, this: id, other_dict: id) -> id {
    // Apple's `-[NSDictionary initWithDictionary:nil]` returns an empty
    // dictionary instead of crashing. Guard against guest code calling us
    // with `nil` (or a non-dictionary object that has no host record)
    // before we try to swap host objects, so we don't have to rely on the
    // objc phantom-fallback path producing a valid empty `HashMap`.
    if other_dict == nil {
        *env.objc.borrow_mut(this) = <DictionaryHostObject as Default>::default();
        return this;
    }

    let other_host_object: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(other_dict));
    let mut host_object = <DictionaryHostObject as Default>::default();

    for key in other_host_object.iter_keys() {
        let object = other_host_object.lookup(env, key);
        host_object.insert(env, key, object, /* copy_key: */ true);
    }

    *env.objc.borrow_mut(this) = host_object;
    *env.objc.borrow_mut(other_dict) = other_host_object;
    this
}

/// Helper function so share `initWithObjects:ForKeys:` implementations
fn init_with_objects_for_keys_common(env: &mut Environment, this: id, objects: id, keys: id) -> id {
    let keys_size: NSUInteger = msg![env; keys count];
    let objects_size: NSUInteger = msg![env; objects count];
    if keys_size != objects_size {
        log!(
            "Warning: initWithObjects:forKeys: count mismatch (keys={}, objects={}); \
             truncating to the shorter array.",
            keys_size,
            objects_size
        );
    }

    let mut host_object = <DictionaryHostObject as Default>::default();

    let objects_enumerator: id = msg![env; objects objectEnumerator];
    let keys_enumerator: id = msg![env; keys objectEnumerator];

    loop {
        let next_key: id = msg![env; keys_enumerator nextObject];
        let next_object: id = msg![env; objects_enumerator nextObject];
        if next_key == nil || next_object == nil {
            break;
        }
        host_object.insert(env, next_key, next_object, /* copy_key: */ true);
    }
    *env.objc.borrow_mut(this) = host_object;
    this
}

/// Helper function for initWithObjects:forKeys:count: implementation
fn init_with_objects_for_keys_count_common(
    env: &mut Environment,
    this: id,
    objects: ConstPtr<id>,
    keys: ConstPtr<id>,
    count: NSUInteger,
) -> id {
    let mut host_object = <DictionaryHostObject as Default>::default();
    let keys_bits = keys.to_bits();
    let objects_bits = objects.to_bits();
    let elem_size = std::mem::size_of::<id>() as NSUInteger;
    for i in 0..count {
        let offset = i * elem_size;
        let key: id = env.mem.read(ConstPtr::from_bits(keys_bits + offset));
        let object: id = env.mem.read(ConstPtr::from_bits(objects_bits + offset));
        if key == nil {
            log!(
                "Warning: initWithObjects:forKeys:count: nil key at index {}; skipping pair.",
                i
            );
            continue;
        }
        host_object.insert(env, key, object, /* copy_key: */ true);
    }

    *env.objc.borrow_mut(this) = host_object;
    this
}

/// Helper function to share `allKeys` implementations
fn all_keys_common(env: &mut Environment, this: id) -> id {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let keys: Vec<id> = host_obj
        .map
        .values()
        .flatten()
        .map(|&(key, _value)| key)
        .collect();
    *env.objc.borrow_mut(this) = host_obj;
    for &key in &keys {
        retain(env, key);
    }
    let res = ns_array::from_vec(env, keys);
    autorelease(env, res)
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// NSDictionary is an abstract class. A subclass must provide:
// - (id)initWithObjects:(id*)forKeys:(id*)count:(NSUInteger)
// - (NSUInteger)count
// - (id)objectForKey:(id)
// - (NSEnumerator*)keyEnumerator
// We can pick whichever subclass we want for the various alloc methods.
// For the time being, that will always be _touchHLE_NSDictionary.

@implementation NSDictionary: NSObject

+ (id)allocWithZone:(NSZonePtr)zone {
    // NSDictionary might be subclassed by something which needs allocWithZone:
    // to have the normal behaviour. We don't currently support that; warn and
    // fall back to the bridged subclass instead of crashing the host.
    let ns_dict_class = env.objc.get_known_class("NSDictionary", &mut env.mem);
    if this != ns_dict_class {
        log!(
            "Warning: +[NSDictionary allocWithZone:] called on subclass {:?}; \
             treating as NSDictionary.",
            this
        );
    }
    msg_class![env; _touchHLE_NSDictionary allocWithZone:zone]
}

+ (id)dictionary {
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict init];
    autorelease(env, new_dict)
}

+ (id)dictionaryWithObject:(id)object forKey:(id)key {
    if key == nil {
        log!(
            "Warning: +[NSDictionary dictionaryWithObject:forKey:] called with nil key; \
             returning empty dictionary."
        );
        let new_dict = dict_from_keys_and_objects(env, &[]);
        return autorelease(env, new_dict);
    }

    let new_dict = dict_from_keys_and_objects(env, &[(key, object)]);
    autorelease(env, new_dict)
}

+ (id)dictionaryWithObjectsAndKeys:(id)first_object, ...dots {
    let new_dict: id = msg![env; this alloc];
    let new_dict = init_with_objects_and_keys(env, new_dict, first_object, dots.start());
    autorelease(env, new_dict)
}

// These probably comes from some category related to plists.
+ (id)dictionaryWithContentsOfFile:(id)path { // NSString*
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict initWithContentsOfFile:path];
    autorelease(env, new_dict)
}
+ (id)dictionaryWithContentsOfURL:(id)url { // NSURL*
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict initWithContentsOfURL:url];
    autorelease(env, new_dict)
}

+ (id)dictionaryWithObjects:(id)objects //NSArray *
                    forKeys:(id)keys { //NSArray *
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict initWithObjects:objects forKeys:keys];
    autorelease(env, new_dict)
}

// FIX: Added missing class method dictionaryWithObjects:forKeys:count:
// This method takes C arrays (not NSArrays) with a count
+ (id)dictionaryWithObjects:(ConstPtr<id>)objects
                    forKeys:(ConstPtr<id>)keys
                      count:(NSUInteger)count {
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict initWithObjects:objects forKeys:keys count:count];
    autorelease(env, new_dict)
}

+ (id)dictionaryWithDictionary:(id)dict { // NSDictionary*
    let new_dict: id = msg![env; this alloc];
    let new_dict: id = msg![env; new_dict initWithDictionary:dict];
    autorelease(env, new_dict)
}

- (id)init {
    // NSDictionary is abstract; calling -init on the base class is unusual but
    // some buggy apps may still do it. Fall back to an empty mutable dict-
    // backed host object so the receiver remains usable instead of panicking.
    log!(
        "Warning: -[NSDictionary init] called on the abstract base class; \
         returning empty dictionary."
    );
    *env.objc.borrow_mut(this) = <DictionaryHostObject as Default>::default();
    this
}

- (id)keyEnumerator {
    // 1. Получаем все ключи словаря в виде объекта NSArray
    let keys: id = msg![env; this allKeys];

    // 2. Возвращаем готовый энумератор массива (он уже честно реализован в
    // ns_array.rs)
    msg![env; keys objectEnumerator]
}

- (id)objectEnumerator {
    // Заодно добавим и перебор значений, чтобы игра не упала на следующем шаге
    let values: id = msg![env; this allValues];
    msg![env; values objectEnumerator]
}

// These probably comes from some category related to plists.
- (id)initWithContentsOfFile:(id)path { // NSString*
    release(env, this);
    let path = ns_string::to_rust_string(env, path);
    deserialize_plist_from_file(
        env,
        GuestPath::new(&path),
        /* array_expected: */ false,
    )
}
- (id)initWithContentsOfURL:(id)url { // NSURL*
    release(env, this);
    let path = ns_url::to_rust_path(env, url);
    deserialize_plist_from_file(env, &path, /* array_expected: */ false)
}

- (bool)writeToFile:(id)path // NSString*
         atomically:(bool)atomically {
    let error_desc: MutPtr<id> = Ptr::null();
    let data: id = msg_class![env; NSPropertyListSerialization
            dataFromPropertyList:this
                          format:NSPropertyListBinaryFormat_v1_0
                errorDescription:error_desc];
    let res = msg![env; data writeToFile:path atomically:atomically];
    log_dbg!(
        "[(NSDictionary *){:?} writeToFile:{:?} atomically:{}] -> {}",
        this,
        to_rust_string(env, path),
        atomically,
        res
    );
    res
}

// `- (BOOL)writeToURL:(NSURL *)url atomically:(BOOL)atomically` —
// per Apple's [NSDictionary Reference](https://developer.apple.com/documentation/foundation/nsdictionary/1414800-writetourl):
// the URL-flavoured analogue of `-writeToFile:atomically:`. Apple's
// documented contract: the URL must be a file URL, and the dictionary
// must contain only property-list types. We pull the path out of the
// URL with `-[NSURL path]` and delegate to the existing path-based
// implementation, which already serialises via
// `NSPropertyListSerialization` and writes through `-[NSData writeToFile:atomically:]`.
- (bool)writeToURL:(id)url
        atomically:(bool)atomically {
    if url == nil {
        return false;
    }
    let path: id = msg![env; url path];
    msg![env; this writeToFile:path atomically:atomically]
}

// TODO

- (id)valueForKey:(id)key { // NSString*
    let key_str = to_rust_string(env, key);
    // NSKeyValueCoding: keys starting with '@' (e.g. "@count") are KVC
    // operators. We don't implement them; just log and fall back to a normal
    // objectForKey: lookup so the guest doesn't crash.
    if key_str.starts_with('@') {
        log!(
            "Warning: -[NSDictionary valueForKey:] KVC operator {:?} not implemented; \
             falling back to objectForKey:.",
            key_str
        );
    }
    msg![env; this objectForKey:key]
}

// NSDictionary(NSFileAttributes) category
// TODO: implement categories properly
- (id)fileModificationDate {
    let modif_date_key = get_static_str(env, NSFileModificationDate);
    msg![env; this objectForKey:modif_date_key]
}
- (u64)fileSize {
    let size_key = get_static_str(env, NSFileSize);
    let num = msg![env; this objectForKey:size_key];
    if num != nil {
        msg![env; num unsignedLongLongValue]
    } else {
        // GnuStep docs claiming to return NSNotFound here [ref](https://www.gnustep.org/resources/documentation/Developer/Base/Reference/NSFileManager.html#method$NSDictionary(NSFileAttributes)-fileSize)
        // But as seen on iPhone Simulator, it's returning 0 with an empty dict
        0
    }
}
- (id)fileType {
    let file_type_key = get_static_str(env, NSFileType);
    msg![env; this objectForKey:file_type_key]
}

// Apple's
// <https://developer.apple.com/documentation/foundation/nsdictionary/1415900-enumeratekeysandobjectsusingblo>:
// invokes the supplied
// `void (^)(id key, id obj, BOOL *stop)` block once per entry. The
// caller may write `*stop = YES;` to abort the enumeration early.
// Each entry's iteration uses a fresh, zero-initialised `BOOL *stop`
// slot owned by the runtime (never reused storage), and the
// enumeration order is unspecified — matching Apple's documented
// behaviour for plain `NSDictionary`.
- (())enumerateKeysAndObjectsUsingBlock:(MutPtr<u8>)block {
    let opts: NSUInteger = 0;
    () = msg![env; this enumerateKeysAndObjectsWithOptions:opts usingBlock:block];
}

// Apple's
// <https://developer.apple.com/documentation/foundation/nsdictionary/1408008-enumeratekeysandobjectswithopti>.
// `NSEnumerationOptions` is a bitmask:
//   NSEnumerationConcurrent = 1 << 0
//   NSEnumerationReverse    = 1 << 1
// `NSEnumerationReverse` is documented as having no effect on plain
// `NSDictionary` (which has no defined order), and we always enumerate
// serially — both behaviours match Apple's runtime.
- (())enumerateKeysAndObjectsWithOptions:(NSUInteger)_opts usingBlock:(MutPtr<u8>)block {
    if block.is_null() {
        return;
    }
    // Apple Block ABI: the third word (offset +12) of the block struct
    // is the `invoke` function pointer for the underlying C function.
    // See <https://clang.llvm.org/docs/Block-ABI-Apple.html>.
    let invoke_ptr_addr: MutPtr<u32> =
        Ptr::from_bits(block.to_bits() + 12);
    let invoke_addr: u32 = env.mem.read(invoke_ptr_addr);
    if invoke_addr == 0 {
        log!(
            "Warning: enumerateKeysAndObjectsWithOptions:usingBlock: block \
             at {:?} has NULL invoke pointer; skipping.",
            block
        );
        return;
    }
    let invoke = GuestFunction::from_addr_with_thumb_bit(invoke_addr);
    let block_arg: crate::mem::ConstVoidPtr =
        Ptr::from_bits(block.to_bits()).cast_const();

    // `BOOL` on iOS is one byte; we allocate a 4-byte slot because the
    // ARMv7 AAPCS widens small values to a word when passed by pointer
    // and the unused tail costs nothing.
    let stop_ptr: MutPtr<u8> = env.mem.alloc(4).cast();
    env.mem.write(stop_ptr, 0u8);

    // Snapshot the keys first: the block is allowed to mutate the
    // receiver if it is a `NSMutableDictionary`, and Apple's runtime
    // takes a snapshot up-front.
    let keys_array: id = msg![env; this allKeys];
    let count: NSUInteger = msg![env; keys_array count];
    let mut i: NSUInteger = 0;
    while i < count {
        let key: id = msg![env; keys_array objectAtIndex:i];
        let obj: id = msg![env; this objectForKey:key];
        <GuestFunction as CallFromHost<(), (crate::mem::ConstVoidPtr, id, id, MutPtr<u8>)>>::call_from_host(
            &invoke, env, (block_arg, key, obj, stop_ptr),
        );
        if env.mem.read(stop_ptr) != 0 {
            break;
        }
        i += 1;
    }
    env.mem.free(stop_ptr.cast());
}

// `- (NSArray<KeyType> *)keysSortedByValueUsingSelector:(SEL)comparator`
// Per Apple's NSDictionary reference: returns the dictionary's keys, sorted
// by their corresponding values. Each pair of values is compared by sending
// `comparator` (e.g. `compare:`) to one value with the other as its argument;
// the selector must return an `NSComparisonResult`. The keys are reordered to
// match the ascending order of their values.
// <https://developer.apple.com/documentation/foundation/nsdictionary/1410025-keyssortedbyvalueusingselector>
- (id)keysSortedByValueUsingSelector:(SEL)comparator {
    // Snapshot keys and their values up-front (Apple sorts a copy).
    let keys_array: id = msg![env; this allKeys];
    let count: NSUInteger = msg![env; keys_array count];

    let mut keys: Vec<id> = Vec::with_capacity(count as usize);
    let mut values: Vec<id> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let key: id = msg![env; keys_array objectAtIndex:i];
        let value: id = msg![env; this objectForKey:key];
        keys.push(key);
        values.push(value);
    }

    let len = keys.len().try_into().unwrap();
    // Sort key/value indices together: compare values via `comparator`, and
    // apply the same swaps to the parallel `keys` vector so the returned keys
    // line up with their values' ascending order.
    let mut user_data = (env, &mut keys, &mut values);
    qsort_generic(
        &mut user_data,
        len,
        &mut |(env, _keys, values), l, r| {
            let (l, r): (usize, usize) = (l.try_into().unwrap(), r.try_into().unwrap());
            let res: NSComparisonResult = msg_send(env, (values[l], comparator, values[r]));
            res
        },
        &mut |(_, keys, values), l, r| {
            let (l, r): (usize, usize) = (l.try_into().unwrap(), r.try_into().unwrap());
            keys.swap(l, r);
            values.swap(l, r);
        },
    );
    let (env, _, _) = user_data;

    // Keys are owned by the dictionary; the returned array needs its own
    // strong references. Retain each key for the new array, then autorelease
    // the array per Apple's ownership convention (it is not `new`/`copy`).
    for &key in &keys {
        retain(env, key);
    }
    let res = ns_array::from_vec(env, keys);
    autorelease(env, res)
}

- (bool)isEqual:(id)other {
    if this == other {
        return true;
    }
    let class: Class = msg_class![env; NSDictionary class];
    if !msg![env; other isKindOfClass:class] {
        return false;
    }
    msg![env; this isEqualToDictionary:other]
}
- (bool)isEqualToDictionary:(id)other { // NSDictionary *
    if other == nil {
        return false;
    }
    let count: NSUInteger = msg![env; this count];
    let other_count: NSUInteger = msg![env; other count];
    if count != other_count {
        return false;
    }
    let keys_arr = msg![env; this allKeys];
    let keys_count: NSUInteger = msg![env; keys_arr count];
    for i in 0..keys_count {
        let key: id = msg![env; keys_arr objectAtIndex:i];
        let value: id = msg![env; this objectForKey:key];
        let other_value: id = msg![env; other objectForKey:key];
        let equal: bool = msg![env; value isEqual:other_value];
        if !equal {
            return false;
        }
    }
    true
}
// Some apps (e.g. Rigonauts) incorrectly call mutation methods on
// immutable NSDictionary instances. Rather than failing with "does not
// respond to selector", we handle these gracefully. The underlying
// DictionaryHostObject is the same structure for both mutable and
// immutable, so we can safely perform the mutation.
- (())addEntriesFromDictionary:(id)other { // NSDictionary *
    if other == nil {
        return;
    }
    log_dbg!(
        "Warning: addEntriesFromDictionary: called on immutable NSDictionary {:?}; \
         performing mutation anyway for compatibility.",
        this
    );
    let mut keys: Vec<id> = Vec::new();
    let key_enum: id = msg![env; other keyEnumerator];
    if key_enum == nil {
        return;
    }
    loop {
        let next: id = msg![env; key_enum nextObject];
        if next == nil {
            break;
        }
        retain(env, next);
        keys.push(next);
    }
    for key in keys {
        let val: id = msg![env; other objectForKey:key];
        if val != nil {
            // Use the internal insert directly since setObject:forKey:
            // may not be declared on this class.
            let mut host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
            host_obj.insert(env, key, val, /* copy_key: */ true);
            *env.objc.borrow_mut(this) = host_obj;
        }
        release(env, key);
    }
}

// Some apps call setObject:forKey: on immutable dictionaries as well.
- (())setObject:(id)object forKey:(id)key {
    if object == nil || key == nil {
        if key == nil {
            log!("Warning: [NSDictionary setObject:forKey:] attempt to use nil key — ignoring");
        }
        if object == nil {
            log!("Warning: [NSDictionary setObject:forKey:] attempt to insert nil object — ignoring");
        }
        return;
    }
    log_dbg!(
        "Warning: setObject:forKey: called on immutable NSDictionary {:?}; \
         performing mutation anyway for compatibility.",
        this
    );
    let mut host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.insert(env, key, object, /* copy_key: */ true);
    *env.objc.borrow_mut(this) = host_obj;
}

@end

// NSMutableDictionary is an abstract class. A subclass must provide everything
// NSDictionary provides, plus:
// - (void)setObject:(id)object forKey:(id)key;
// - (void)removeObjectForKey:(id)key;
// Note that it inherits from NSDictionary, so we must ensure we override
// any default methods that would be inappropriate for mutability.
@implementation NSMutableDictionary: NSDictionary

+ (id)allocWithZone:(NSZonePtr)zone {
    // NSMutableDictionary might be subclassed; warn but don't crash.
    let mutable_class = env.objc.get_known_class("NSMutableDictionary", &mut env.mem);
    if this != mutable_class {
        log!(
            "Warning: +[NSMutableDictionary allocWithZone:] called on subclass {:?}; \
             treating as NSMutableDictionary.",
            this
        );
    }
    msg_class![env; _touchHLE_NSMutableDictionary allocWithZone:zone]
}

+ (id)dictionaryWithCapacity:(NSUInteger)capacity {
    let new: id = msg![env; this alloc];
    let new: id = msg![env; new initWithCapacity:capacity];
    autorelease(env, new)
}

// These probably comes from some category related to plists.
- (id)initWithContentsOfFile:(id)path { // NSString*
    release(env, this);
    let path = ns_string::to_rust_string(env, path);
    let tmp = deserialize_plist_from_file(
        env,
        GuestPath::new(&path),
        /* array_expected: */ false
    );
    if tmp == nil {
        return nil;
    }
    // We should respect mutability of the top most container!
    let res = msg_class![env; NSMutableDictionary alloc];
    let res = msg![env; res initWithDictionary:tmp];
    release(env, tmp);
    res
}
- (id)initWithContentsOfURL:(id)url { // NSURL*
    release(env, this);
    let path = ns_url::to_rust_path(env, url);
    let tmp = deserialize_plist_from_file(env, &path, /* array_expected: */ false);
    if tmp == nil {
        return nil;
    }
    // We should respect mutability of the top most container!
    let res = msg_class![env; NSMutableDictionary alloc];
    let res = msg![env; res initWithDictionary:tmp];
    release(env, tmp);
    res
}

// РЕАЛИЗАЦИЯ УДАЛЕНИЯ ПО МАССИВУ КЛЮЧЕЙ
- (())removeObjectsForKeys:(id)key_array { // NSArray *
    if key_array == nil {
        return;
    }
    let count: NSUInteger = msg![env; key_array count];
    for i in 0..count {
        let key: id = msg![env; key_array objectAtIndex:i];
        () = msg![env; this removeObjectForKey:key];
    }
}

@end

// Our private subclass that is the single implementation of NSDictionary for
// the time being.
@implementation _touchHLE_NSDictionary: NSDictionary

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<DictionaryHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    std::mem::take(env.objc.borrow_mut::<DictionaryHostObject>(this)).release(env);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)initWithObjectsAndKeys:(id)first_object, ...dots {
    init_with_objects_and_keys(env, this, first_object, dots.start())
}

- (id)init {
    *env.objc.borrow_mut(this) = <DictionaryHostObject as Default>::default();
    this
}

- (id)initWithDictionary:(id)dictionary {
    init_with_dictionary_common(env, this, dictionary)
}

- (id)initWithObjects:(id)objects //NSArray *
              forKeys:(id)keys { //NSArray *
    init_with_objects_for_keys_common(env, this, objects, keys)
}

// FIX: Added missing instance method initWithObjects:forKeys:count:
// This method takes C arrays (not NSArrays) with a count
- (id)initWithObjects:(ConstPtr<id>)objects
              forKeys:(ConstPtr<id>)keys
                count:(NSUInteger)count {
    init_with_objects_for_keys_count_common(env, this, objects, keys, count)
}

// TODO: enumeration, more init methods, etc

- (NSUInteger)count {
    env.objc.borrow::<DictionaryHostObject>(this).count
}
- (id)objectForKey:(id)key {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let res = host_obj.lookup(env, key);
    *env.objc.borrow_mut(this) = host_obj;
    res
}

// Objective-C 2.0 subscripting: `dict[key]` lowers to a call to
// `-objectForKeyedSubscript:`. Apple's NSDictionary forwards directly to
// `-objectForKey:`. See
// <https://developer.apple.com/documentation/foundation/nsdictionary/1408301-objectforkeyedsubscript>.
- (id)objectForKeyedSubscript:(id)key {
    msg![env; this objectForKey:key]
}

- (id)allKeys {
    all_keys_common(env, this)
}

- (id)allValues {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let values: Vec<id> = host_obj.map.values().flatten().map(|&(_key, value)| value).collect();
    *env.objc.borrow_mut(this) = host_obj;
    for &val in &values {
        retain(env, val);
    }
    let res = crate::frameworks::foundation::ns_array::from_vec(env, values);
    autorelease(env, res)
}

// Returns every key mapped to a value equal to `obj`. See:
// https://developer.apple.com/documentation/foundation/nsdictionary/1408301-allkeysforobject
- (id)allKeysForObject:(id)obj {
    let res: id = msg_class![env; NSMutableArray new];

    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.map.values().flatten().for_each(|&(key, value)| {
        let equal = value == obj || msg![env; obj isEqual:value];
        if equal {
            () = msg![env; res addObject:key];
        }
    });
    *env.objc.borrow_mut(this) = host_obj;

    let res_imm: id = msg![env; res copy];
    release(env, res);
    autorelease(env, res_imm)
}

// NSFastEnumeration implementation
- (NSUInteger)countByEnumeratingWithState:(MutPtr<NSFastEnumerationState>)state
                                  objects:(MutPtr<id>)stackbuf
                                    count:(NSUInteger)len {
    // We assume that order in which objects are reported is consistent
    // between calls!
    let objects: id = msg![env; this allKeys];
    let count: NSUInteger = msg![env; objects count];
    fast_enumeration_helper(env, this, |env, idx| {
        if idx < count {
            msg![env; objects objectAtIndex:idx]
        } else {
            nil
        }
    }, state, stackbuf, len)
}

// NSCopying implementation
- (id)copyWithZone:(NSZonePtr)_zone {
    retain(env, this)
}

// NSMutableCopying implementation
- (id)mutableCopyWithZone:(NSZonePtr)_zone {
    let mut_dict: id = msg_class![env; NSMutableDictionary alloc];
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    for (k, v) in host_obj.map.values().flatten() {
        () = msg![env; mut_dict setObject:(*v) forKey:(*k)];
    }
    *env.objc.borrow_mut(this) = host_obj;
    mut_dict
}

- (id)description {
    build_description(env, this)
}

// NSCoding implementation for immutable NSDictionary
- (id)initWithCoder:(id)coder {
    let class: Class = msg![env; coder class];
    let keyed_unarch_class: Class = msg_class![env; NSKeyedUnarchiver class];
    let nib_archive_class: Class = msg_class![env; _touchHLE_NIBArchiveDecoder class];
    let tuples = if env.objc.class_is_subclass_of(class, keyed_unarch_class) {
        ns_keyed_unarchiver::decode_current_dict(env, coder)
    } else if env.objc.class_is_subclass_of(class, nib_archive_class) {
        _nib_archive_decoder::decode_current_dict(env, coder)
    } else {
        log!(
            "Warning: -[_touchHLE_NSDictionary initWithCoder:] unsupported coder class {:?}; \
             returning empty dictionary.",
            class
        );
        Vec::new()
    };
    release(env, this);
    dict_from_keys_and_objects(env, &tuples)
}

- (())encodeWithCoder:(id)coder {
    let class: Class = msg![env; coder class];
    let keyed_arch_class: Class = msg_class![env; NSKeyedArchiver class];

    if env.objc.class_is_subclass_of(class, keyed_arch_class) {
        let host = env.objc.borrow::<DictionaryHostObject>(this);
        let pairs: Vec<(id, id)> = host.map.values()
           .flat_map(|v| v.iter().copied())
           .collect();
        let keys: Vec<id> = pairs.iter().map(|&(k, _)| k).collect();
        let objects: Vec<id> = pairs.iter().map(|&(_, v)| v).collect();

        // NSKeyedArchiver stores a dictionary's contents as two parallel inline
        // arrays of UID references, under "NS.keys" and "NS.objects" (Apple's
        // format, which is what our decoder reads back). See
        // `encode_objects_as_uid_array`.
        ns_keyed_archiver::encode_objects_as_uid_array(env, coder, "NS.keys", &keys);
        ns_keyed_archiver::encode_objects_as_uid_array(env, coder, "NS.objects", &objects);
    } else {
        log!(
            "Warning: -[_touchHLE_NSDictionary encodeWithCoder:] unsupported coder class {:?}",
            class
        );
    }
}

@end

// Our private subclass that is the single implementation of
// NSMutableDictionary for the time being.
@implementation _touchHLE_NSMutableDictionary: NSMutableDictionary

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<DictionaryHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    std::mem::take(env.objc.borrow_mut::<DictionaryHostObject>(this)).release(env);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (())setDictionary:(id)dict {
    todo_objc_setter!(this, dict);
}

- (id)initWithObjectsAndKeys:(id)first_object, ...dots {
    init_with_objects_and_keys(env, this, first_object, dots.start())
}

- (id)initWithDictionary:(id)dictionary {
    init_with_dictionary_common(env, this, dictionary)
}

- (id)init {
    *env.objc.borrow_mut(this) = <DictionaryHostObject as Default>::default();
    this
}

- (id)initWithCapacity:(NSUInteger)_capacity {
    // TODO: capacity
    msg![env; this init]
}

// NSCoding implementation
- (id)initWithCoder:(id)coder {
    let class: Class = msg![env; coder class];
    let keyed_unarch_class: Class = msg_class![env; NSKeyedUnarchiver class];
    let nib_archive_class: Class = msg_class![env; _touchHLE_NIBArchiveDecoder class];
    let tuples = if env.objc.class_is_subclass_of(class, keyed_unarch_class) {
        // It seems that every NSDictionary item in an NSKeyedArchiver plist
        // looks like:
        // {
        //   "$class" => (uid of NSDictionary class goes here),
        //   "NS.keys" => [
        //     // keys here
        //   ]
        //   "NS.objects" => [
        //     // objects here
        //   ]
        // }
        ns_keyed_unarchiver::decode_current_dict(env, coder)
    } else if env.objc.class_is_subclass_of(class, nib_archive_class) {
        _nib_archive_decoder::decode_current_dict(env, coder)
    } else {
        log!(
            "Warning: -[NSMutableDictionary initWithCoder:] unsupported coder class {:?}; \
             returning empty dictionary.",
            class
        );
        Vec::new()
    };
    release(env, this);
    let dict = dict_from_keys_and_objects(env, &tuples);

    let mut_dict = msg![env; dict mutableCopy];
    release(env, dict);
    mut_dict
}

- (id)initWithObjects:(id)objects //NSArray *
              forKeys:(id)keys { //NSArray *
    init_with_objects_for_keys_common(env, this, objects, keys)
}

// FIX: Added missing instance method initWithObjects:forKeys:count: for mutable
// dictionary
- (id)initWithObjects:(ConstPtr<id>)objects
              forKeys:(ConstPtr<id>)keys
                count:(NSUInteger)count {
    init_with_objects_for_keys_count_common(env, this, objects, keys, count)
}

// TODO: enumeration, more init methods, etc

- (NSUInteger)count {
    env.objc.borrow::<DictionaryHostObject>(this).count
}
- (id)objectForKey:(id)key {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let res = host_obj.lookup(env, key);
    *env.objc.borrow_mut(this) = host_obj;
    res
}

// Objective-C 2.0 dictionary subscripting:
//   `dict[key]`        → `-objectForKeyedSubscript:`
//   `dict[key] = obj`  → `-setObject:forKeyedSubscript:`
// Apple's reference implementation forwards both to the keyed
// `-objectForKey:` / `-setObject:forKey:` family; if `obj` is nil the
// setter removes the key instead. See
// <https://developer.apple.com/documentation/foundation/nsmutabledictionary/1410865-setobject>.
- (id)objectForKeyedSubscript:(id)key {
    msg![env; this objectForKey:key]
}

- (())setObject:(id)object forKeyedSubscript:(id)key {
    if object == nil {
        () = msg![env; this removeObjectForKey:key];
    } else {
        () = msg![env; this setObject:object forKey:key];
    }
}

// NSCoding implementation
- (())encodeWithCoder:(id)coder {
    let class: Class = msg![env; coder class];
    let keyed_arch_class: Class = msg_class![env; NSKeyedArchiver class];

    if env.objc.class_is_subclass_of(class, keyed_arch_class) {
        // Mirror of initWithCoder: decode format:
        // {
        //   "NS.keys" => [ keys here ]
        //   "NS.objects" => [ objects here ]
        // }
        let host = env.objc.borrow::<DictionaryHostObject>(this);
        let pairs: Vec<(id, id)> = host.map.values()
           .flat_map(|v| v.iter().copied())
           .collect();

        let keys_array: id = msg_class![env; NSMutableArray new];
        let objects_array: id = msg_class![env; NSMutableArray new];
        for (k, v) in &pairs {
            let key = *k;
            let val = *v;
            () = msg![env; keys_array addObject:key];
            () = msg![env; objects_array addObject:val];
        }

        let keys_str = from_rust_string(env, "NS.keys".to_string());
        let objects_str = from_rust_string(env, "NS.objects".to_string());
        () = msg![env; coder encodeObject:keys_array forKey:keys_str];
        () = msg![env; coder encodeObject:objects_array forKey:objects_str];

        release(env, keys_str);
        release(env, objects_str);
        release(env, keys_array);
        release(env, objects_array);
    } else {
        log!(
            "Warning: NSMutableDictionary encodeWithCoder: unsupported coder class, skipping"
        );
    }
}

// NSFastEnumeration implementation
- (NSUInteger)countByEnumeratingWithState:(MutPtr<NSFastEnumerationState>)state
                                  objects:(MutPtr<id>)stackbuf
                                    count:(NSUInteger)len {
    // TODO: check that dict wasn't mutated!
    // We assume that order in which objects are reported is consistent
    // between calls!
    let objects: id = msg![env; this allKeys];
    let count: NSUInteger = msg![env; objects count];
    fast_enumeration_helper(env, this, |env, idx| {
        if idx < count {
            msg![env; objects objectAtIndex:idx]
        } else {
            nil
        }
    }, state, stackbuf, len)
}

// NSCopying implementation
- (id)copyWithZone:(NSZonePtr)_zone {
    let entries: Vec<_> =
        env.objc.borrow_mut::<DictionaryHostObject>(this).map.values().flatten().copied().collect();
    dict_from_keys_and_objects(env, &entries)
}

// NSMutableCopying implementation
- (id)mutableCopyWithZone:(NSZonePtr)_zone {
    let mut_dict: id = msg_class![env; NSMutableDictionary alloc];
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    for (k, v) in host_obj.map.values().flatten() {
        () = msg![env; mut_dict setObject:(*v) forKey:(*k)];
    }
    *env.objc.borrow_mut(this) = host_obj;
    mut_dict
}

- (())setValue:(id)value
        forKey:(id)key { // NSString *
    // TODO: assert that key is a string when using key-value coding
    if value == nil {
        msg![env; this removeObjectForKey:key]
    } else {
        msg![env; this setObject:value forKey:key]
    }
}

- (())setObject:(id)object
             forKey:(id)key {
        // Если объект nil, по правилам iOS должно быть исключение
        // NSInvalidArgumentException.
        // Чтобы не ронять эмулятор паникой, логируем ошибку и прерываем
        // добавление.
        if object == nil {
            let key_str = if key != nil {
                crate::frameworks::foundation::ns_string::to_rust_string(env, key).to_string()
            } else {
                "nil".to_string()
            };
            log!("Warning: [NSMutableDictionary setObject:forKey:] attempt to insert nil object for key {} — ignoring", key_str);
            return;
        }

        if key == nil {
            log!("Warning: [NSMutableDictionary setObject:forKey:] attempt to use nil key — ignoring");
            return;
        }

        let mut host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
        host_obj.insert(env, key, object, /* copy_key: */ true);
        *env.objc.borrow_mut(this) = host_obj;
    }

- (())removeObjectForKey:(id)key {
    if key.is_null() {
        log!("Warning: [NSMutableDictionary removeObjectForKey:] key is nil — ignored");
        return;
    }
    let mut host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.remove(env, key);
    *env.objc.borrow_mut(this) = host_obj;
}

- (())removeAllObjects {
    let mut old_host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    old_host_obj.release(env);
}

- (())addEntriesFromDictionary:(id)other { // NSDictionary *
    // Iterate `other` through the public NSDictionary API so this works for
    // any subclass (CFDictionary, custom guest subclasses, …) — not only
    // ones backed by a DictionaryHostObject. We collect keys first so the
    // dictionary state can't be mutated mid-enumeration, and so it's safe
    // when `other == this` (in which case the work is a self-no-op after the
    // first pass anyway since every key already maps to its value).
    if other == nil {
        return;
    }
    let mut keys: Vec<id> = Vec::new();
    let key_enum: id = msg![env; other keyEnumerator];
    if key_enum == nil {
        return;
    }
    loop {
        let next: id = msg![env; key_enum nextObject];
        if next == nil {
            break;
        }
        // Retain so the key cannot be deallocated between collection and
        // use (e.g. by autorelease pools draining during the loop).
        retain(env, next);
        keys.push(next);
    }
    for key in keys {
        let val: id = msg![env; other objectForKey:key];
        if val != nil {
            () = msg![env; this setObject:val forKey:key];
        }
        release(env, key);
    }
}

- (())setValuesForKeysWithDictionary:(id)keyed_values { // NSDictionary *
    // Apple's NSKeyValueCoding docs (NSObject(NSKeyValueCoding)):
    // "For each key in keyedValues, the corresponding value is set in the
    //  receiver by invoking -setValue:forKey:. The default implementation
    //  invokes -setValue:forKey: for each key-value pair, substituting nil
    //  for instances of NSNull."
    //
    // We must route through -setValue:forKey: rather than -setObject:forKey:
    // so that KVC-only setters (and guest overrides) are honored. Collect the
    // keys first so the source dictionary can be mutated by side-effects of
    // the setters without invalidating our enumeration.
    if keyed_values == nil {
        return;
    }
    let key_enum: id = msg![env; keyed_values keyEnumerator];
    if key_enum == nil {
        return;
    }
    let mut keys: Vec<id> = Vec::new();
    loop {
        let next: id = msg![env; key_enum nextObject];
        if next == nil {
            break;
        }
        retain(env, next);
        keys.push(next);
    }
    let ns_null: id = msg_class![env; NSNull null];
    for key in keys {
        let val: id = msg![env; keyed_values objectForKey:key];
        // Per Apple docs, NSNull is treated as nil. -setValue:forKey:'s own
        // contract is then "remove the value for the key" when the argument
        // is nil — sending the nil through is therefore the correct thing.
        let arg: id = if val == ns_null { nil } else { val };
        () = msg![env; this setValue:arg forKey:key];
        release(env, key);
    }
}

- (id)description {
    build_description(env, this)
}

- (id)allKeys {
    all_keys_common(env, this)
}

- (id)allValues {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let values: Vec<id> = host_obj.map.values().flatten().map(|&(_key, value)| value).collect();
    *env.objc.borrow_mut(this) = host_obj;
    for &val in &values {
        retain(env, val);
    }
    let res = ns_array::from_vec(env, values);
    autorelease(env, res)
}

- (id)allKeysForObject:(id)obj {
    let res: id = msg_class![env; NSMutableArray new];

    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.map.values().flatten().for_each(|&(key, value)| {
        let equal = msg![env; obj isEqual:value];
        if equal {
            () = msg![env; res addObject:key];
        }
    });
    *env.objc.borrow_mut(this) = host_obj;

    let res_imm = msg![env; res copy];
    release(env, res);
    autorelease(env, res_imm)
}

- (id)objectEnumerator { // NSEnumerator*
    let values: id = msg![env; this allValues];
    msg![env; values objectEnumerator]
}

@end

// Special variant for use by CFDictionary with NULL callbacks: objects aren't
// necessarily Objective-C objects and won't be retained/released.
// TODO: refactor with lookup/insert methods to use callbacks
@implementation _touchHLE_NSMutableDictionary_non_retaining: _touchHLE_NSMutableDictionary

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<CFDictionaryHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// our custom init, not a part of API
- (id)initWithKeyCallbacks:(ConstPtr<CFDictionaryKeyCallBacks>)key_callbacks
         andValueCallbacks:(ConstPtr<CFDictionaryValueCallBacks>)value_callbacks {
    if !key_callbacks.is_null() {
        if value_callbacks.is_null() {
            log!(
                "Warning: _touchHLE_NSMutableDictionary_non_retaining initWithKeyCallbacks: \
                 value_callbacks is NULL while key_callbacks is set; using default value callbacks."
            );
        } else {
            let host_object = env.objc.borrow_mut::<CFDictionaryHostObject>(this);
            host_object.value_callbacks = env.mem.read(value_callbacks);
        }
        let host_object = env.objc.borrow_mut::<CFDictionaryHostObject>(this);
        host_object.key_callbacks = env.mem.read(key_callbacks);
    }
    this
}

- (())dealloc {
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)initWithObjectsAndKeys:(id)_first_object, ..._dots {
    log!(
        "Warning: -[_touchHLE_NSMutableDictionary_non_retaining initWithObjectsAndKeys:] \
         is not implemented; returning empty CF dictionary."
    );
    this
}
- (id)description {
    log!(
        "Warning: -[_touchHLE_NSMutableDictionary_non_retaining description] \
         is not implemented; returning empty string."
    );
    from_rust_string(env, String::new())
}
- (id)copyWithZone:(NSZonePtr)_zone {
    log!(
        "Warning: -[_touchHLE_NSMutableDictionary_non_retaining copyWithZone:] \
         is not implemented; returning self (no copy)."
    );
    retain(env, this);
    this
}
- (id)mutableCopyWithZone:(NSZonePtr)_zone {
    log!(
        "Warning: -[_touchHLE_NSMutableDictionary_non_retaining mutableCopyWithZone:] \
         is not implemented; returning retained self."
    );
    retain(env, this);
    this
}

- (id)objectForKey:(id)key {
    let host_obj: CFDictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let res = host_obj.lookup(env, key);
    *env.objc.borrow_mut(this) = host_obj;
    res
}

- (id)valueForKey:(id)_key {
    log!(
        "Warning: -[_touchHLE_NSMutableDictionary_non_retaining valueForKey:] \
         not supported; returning nil."
    );
    nil
}

- (())setObject:(id)object
         forKey:(id)key {
    // ИСПРАВЛЕНИЕ: Безопасная обработка nil-ключей и объектов (как в основном словаре)
    if object == nil {
        log!("Warning: [_touchHLE_NSMutableDictionary_non_retaining setObject:forKey:] attempt to insert nil object — ignoring");
        return;
    }
    if key == nil {
        log!("Warning: [_touchHLE_NSMutableDictionary_non_retaining setObject:forKey:] attempt to use nil key — ignoring");
        return;
    }

    let mut host_obj: CFDictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.insert(env, key, object);
    *env.objc.borrow_mut(this) = host_obj;
}

- (())removeObjectForKey:(id)key {
    // ИСПРАВЛЕНИЕ: Безопасная обработка nil-ключей
    if key == nil {
        log!("Warning: [_touchHLE_NSMutableDictionary_non_retaining removeObjectForKey:] key is nil — ignored");
        return;
    }

    let mut host_obj: CFDictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    host_obj.remove(env, key);
    *env.objc.borrow_mut(this) = host_obj;
}

- (id)allKeys {
    let host_obj: DictionaryHostObject = std::mem::take(env.objc.borrow_mut(this));
    let keys: Vec<id> = host_obj.map.values().flatten().map(|&(key, _value)| key).collect();
    *env.objc.borrow_mut(this) = host_obj;

    let array: id = msg_class![env; _touchHLE_NSArray_non_retaining alloc];
    env.objc.borrow_mut::<ArrayHostObject>(array).array = keys;
    array
}

@end

};
/// Direct constructor for use by host code, similar to
/// `[[NSDictionary alloc] initWithObjectsAndKeys:]` but without variadics and
/// with a more intuitive argument order.
/// Unlike [super::ns_array::from_vec],
/// this **does** copy and retain!
pub fn dict_from_keys_and_objects(env: &mut Environment, keys_and_objects: &[(id, id)]) -> id {
    let dict: id = msg_class![env; NSDictionary alloc];

    let mut host_object = <DictionaryHostObject as Default>::default();
    for &(key, object) in keys_and_objects {
        host_object.insert(env, key, object, /* copy_key: */ true);
    }
    *env.objc.borrow_mut(dict) = host_object;

    dict
}

/// Direct constructor for use by host code, similar to
/// `[[NSMutableDictionary alloc] initWithObjectsAndKeys:]` but without
/// variadics and with a more intuitive argument order.
/// Unlike [super::ns_array::mutable_from_vec], this **does** copy and retain!
pub fn mutable_dict_from_keys_and_objects(
    env: &mut Environment,
    keys_and_objects: &[(id, id)],
) -> id {
    let dict: id = msg_class![env; NSMutableDictionary alloc];

    let mut host_object = <DictionaryHostObject as Default>::default();
    for &(key, object) in keys_and_objects {
        host_object.insert(env, key, object, /* copy_key: */ true);
    }
    *env.objc.borrow_mut(dict) = host_object;

    dict
}

/// A helper to build a description NSString
/// for a NSDictionary or a NSMutableDictionary.
fn build_description(env: &mut Environment, dict: id) -> id {
    // According to docs, this description should be formatted as property list.
    // But by the same docs, it's meant to be used for debugging purposes only.
    let desc: id = msg_class![env; NSMutableString new];
    let prefix: id = from_rust_string(env, "{\n".to_string());
    () = msg![env; desc appendString:prefix];
    release(env, prefix);
    let keys: Vec<id> = env
        .objc
        .borrow_mut::<DictionaryHostObject>(dict)
        .iter_keys()
        .collect();
    for key in keys {
        let key_desc: id = msg![env; key description];
        let value: id = msg![env; dict objectForKey:key];
        let val_desc: id = msg![env; value description];
        // TODO: respect nesting and padding
        let format = format!(
            "\t{} = {};\n",
            to_rust_string(env, key_desc),
            to_rust_string(env, val_desc)
        );
        let format = from_rust_string(env, format);
        () = msg![env; desc appendString:format];
        release(env, format);
    }
    let suffix: id = from_rust_string(env, "}".to_string());
    () = msg![env; desc appendString:suffix];
    release(env, suffix);
    let desc_imm = msg![env; desc copy];
    release(env, desc);
    autorelease(env, desc_imm)
}
