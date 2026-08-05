/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//!
//! The Core Location framework.
//!
//! Proper implementation of this framework _could_ make sense on Android,
//! but it seems like early iOS games were using it exclusively to
//! ~~spy on~~ track users without actual location-based gameplay.
//!
//! Some apps (e.g. maps) would _require_ location support to work properly,
//! but it is not the current focus of the touchHLE. The current focus is,
//! you know, **GAMES**.

use crate::abi::GuestArg;
use crate::dyld::{ConstantExports, HostConstant, HostDylib};
use crate::frameworks::foundation::ns_string;
use crate::mem::SafeRead;
use crate::objc::{
    id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject, NSZonePtr,
};

pub const DYLIB: HostDylib = HostDylib {
    path: "/System/Library/Frameworks/CoreLocation.framework/CoreLocation",
    aliases: &[],
    class_exports: &[CLASSES],
    constant_exports: &[CONSTANTS],
    function_exports: &[],
};

// MARK: - Type aliases

type CLLocationAccuracy = f64;
type CLLocationDegrees = f64;
type CLLocationDistance = f64;
type CLLocationSpeed = f64;
type CLLocationDirection = f64;
type CLHeadingComponentValue = f64;
type CLAuthorizationStatus = i32;
type CLDeviceOrientation = i32;
type CLActivityType = i32;

// MARK: - CLLocationCoordinate2D

/// A geographical coordinate (latitude/longitude pair), matching Apple's
/// `CLLocationCoordinate2D` struct layout on 32-bit ARM.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct CLLocationCoordinate2D {
    pub latitude: CLLocationDegrees,
    pub longitude: CLLocationDegrees,
}
unsafe impl SafeRead for CLLocationCoordinate2D {}
impl GuestArg for CLLocationCoordinate2D {
    const REG_COUNT: usize = 4; // 2 x f64, each f64 = 2 u32 registers
    fn from_regs(regs: &[u32]) -> Self {
        CLLocationCoordinate2D {
            latitude: f64::from_regs(&regs[0..2]),
            longitude: f64::from_regs(&regs[2..4]),
        }
    }
    fn to_regs(self, regs: &mut [u32]) {
        self.latitude.to_regs(&mut regs[0..2]);
        self.longitude.to_regs(&mut regs[2..4]);
    }
}

// MARK: - CLAuthorizationStatus constants
const CL_AUTHORIZATION_STATUS_NOT_DETERMINED: CLAuthorizationStatus = 0;
const CL_AUTHORIZATION_STATUS_RESTRICTED: CLAuthorizationStatus = 1;
const CL_AUTHORIZATION_STATUS_DENIED: CLAuthorizationStatus = 2;
const CL_AUTHORIZATION_STATUS_AUTHORIZED: CLAuthorizationStatus = 3; // iOS <8 name
const CL_AUTHORIZATION_STATUS_AUTHORIZED_ALWAYS: CLAuthorizationStatus = 3;
const CL_AUTHORIZATION_STATUS_AUTHORIZED_WHEN_IN_USE: CLAuthorizationStatus = 4;

// MARK: - CLDeviceOrientation constants (same as UIDeviceOrientation)
const CL_DEVICE_ORIENTATION_UNKNOWN: CLDeviceOrientation = 0;
const CL_DEVICE_ORIENTATION_PORTRAIT: CLDeviceOrientation = 1;

// MARK: - CLActivityType constants
const CL_ACTIVITY_TYPE_OTHER: CLActivityType = 1;
const CL_ACTIVITY_TYPE_AUTOMOTIVE_NAVIGATION: CLActivityType = 2;
const CL_ACTIVITY_TYPE_FITNESS: CLActivityType = 3;
const CL_ACTIVITY_TYPE_OTHER_NAVIGATION: CLActivityType = 4;

// MARK: - CLLocationManager host object

#[derive(Default)]
struct CLLocationManagerHostObject {
    delegate: id,
    desired_accuracy: CLLocationAccuracy,
    distance_filter: CLLocationDistance,
    heading_filter: CLLocationDirection,
    heading_orientation: CLDeviceOrientation,
    activity_type: CLActivityType,
    pauses_location_updates_automatically: bool,
    allows_background_location_updates: bool,
    shows_background_location_indicator: bool,
    updates_location: bool,
    updates_heading: bool,
}
impl HostObject for CLLocationManagerHostObject {}

// MARK: - CLLocation host object

#[derive(Default)]
struct CLLocationHostObject {
    latitude: CLLocationDegrees,
    longitude: CLLocationDegrees,
    altitude: CLLocationDistance,
    horizontal_accuracy: CLLocationAccuracy,
    vertical_accuracy: CLLocationAccuracy,
    speed: CLLocationSpeed,
    course: CLLocationDirection,
    // NSDate* timestamp
    timestamp: id,
}
impl HostObject for CLLocationHostObject {}

// MARK: - CLHeading host object

#[derive(Default)]
struct CLHeadingHostObject {
    magnetic_heading: CLHeadingComponentValue,
    true_heading: CLHeadingComponentValue,
    heading_accuracy: CLHeadingComponentValue,
    x: CLHeadingComponentValue,
    y: CLHeadingComponentValue,
    z: CLHeadingComponentValue,
    // NSDate* timestamp
    timestamp: id,
}
impl HostObject for CLHeadingHostObject {}

const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// =========================================================================
// MARK: - CLLocationManager
// =========================================================================

@implementation CLLocationManager: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CLLocationManagerHostObject {
        delegate: nil,
        desired_accuracy: 10.0,   // kCLLocationAccuracyNearestTenMeters
        distance_filter: -1.0,    // kCLDistanceFilterNone
        heading_filter: 1.0,      // kCLHeadingFilterDegree (1 degree)
        heading_orientation: CL_DEVICE_ORIENTATION_PORTRAIT,
        activity_type: CL_ACTIVITY_TYPE_OTHER,
        pauses_location_updates_automatically: true,
        allows_background_location_updates: false,
        shows_background_location_indicator: false,
        updates_location: false,
        updates_heading: false,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// MARK: Availability (class methods)

+ (bool)locationServicesEnabled {
    false
}

+ (bool)headingAvailable {
    false
}

+ (bool)significantLocationChangeMonitoringAvailable {
    false
}

+ (bool)isMonitoringAvailableForClass:(id)_region_class {
    false
}

+ (bool)isRangingAvailable {
    false
}

+ (CLAuthorizationStatus)authorizationStatus {
    // Report "denied" so apps that check will give up gracefully.
    CL_AUTHORIZATION_STATUS_DENIED
}

- (id)init {
    this
}

- (())dealloc {
    let delegate = env.objc.borrow::<CLLocationManagerHostObject>(this).delegate;
    release(env, delegate);
    env.objc.dealloc_object(this, &mut env.mem)
}

// MARK: Delegate

- (id)delegate {
    env.objc.borrow::<CLLocationManagerHostObject>(this).delegate
}

- (())setDelegate:(id)delegate {
    let old = env.objc.borrow::<CLLocationManagerHostObject>(this).delegate;
    release(env, old);
    retain(env, delegate);
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).delegate = delegate;
}

// MARK: Accuracy / filter

- (CLLocationAccuracy)desiredAccuracy {
    env.objc.borrow::<CLLocationManagerHostObject>(this).desired_accuracy
}

- (())setDesiredAccuracy:(CLLocationAccuracy)acc {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).desired_accuracy = acc;
}

- (CLLocationDistance)distanceFilter {
    env.objc.borrow::<CLLocationManagerHostObject>(this).distance_filter
}

- (())setDistanceFilter:(CLLocationDistance)filter {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).distance_filter = filter;
}

- (CLLocationDirection)headingFilter {
    env.objc.borrow::<CLLocationManagerHostObject>(this).heading_filter
}

- (())setHeadingFilter:(CLLocationDirection)filter {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).heading_filter = filter;
}

- (CLDeviceOrientation)headingOrientation {
    env.objc.borrow::<CLLocationManagerHostObject>(this).heading_orientation
}

- (bool)locationServicesEnabled {
    false
}

- (())setHeadingOrientation:(CLDeviceOrientation)orientation {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).heading_orientation = orientation;
}

- (CLActivityType)activityType {
    env.objc.borrow::<CLLocationManagerHostObject>(this).activity_type
}

- (())setActivityType:(CLActivityType)activity_type {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).activity_type = activity_type;
}

- (bool)pausesLocationUpdatesAutomatically {
    env.objc.borrow::<CLLocationManagerHostObject>(this).pauses_location_updates_automatically
}

- (())setPausesLocationUpdatesAutomatically:(bool)pauses {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this)
        .pauses_location_updates_automatically = pauses;
}

- (bool)allowsBackgroundLocationUpdates {
    env.objc.borrow::<CLLocationManagerHostObject>(this).allows_background_location_updates
}

- (())setAllowsBackgroundLocationUpdates:(bool)allows {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this)
        .allows_background_location_updates = allows;
}

- (bool)showsBackgroundLocationIndicator {
    env.objc.borrow::<CLLocationManagerHostObject>(this).shows_background_location_indicator
}

- (())setShowsBackgroundLocationIndicator:(bool)shows {
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this)
        .shows_background_location_indicator = shows;
}

// MARK: Location updates

- (())startUpdatingLocation {
    log_dbg!("CLLocationManager startUpdatingLocation — location services unavailable");
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).updates_location = true;
    // Notify the delegate that we failed immediately.
    let delegate = env.objc.borrow::<CLLocationManagerHostObject>(this).delegate;
    if delegate != nil {
        // Use `register_host_selector` rather than
        // `lookup_selector(...).unwrap()`: the selector only exists if some
        // loaded binary references that exact string, and an app is free to
        // set a delegate that implements none of the failure callbacks.
        // Crystal SDK 1.2 (Sword of Fargoal) does exactly that, and the
        // unwrap took the emulator down as soon as the game asked for
        // location updates.
        let sel = env
            .objc
            .register_host_selector("locationManager:didFailWithError:".to_string(), &mut env.mem);
        let responds: bool = msg![env; delegate respondsToSelector:sel];
        if responds {
            let domain = ns_string::get_static_str(env, "kCLErrorDomain");
            let error: id = msg_class![env; NSError errorWithDomain:domain
                                                               code:0
                                                           userInfo:nil];
            let _: () = msg![env; delegate locationManager:this didFailWithError:error];
        }
    }
}

- (())stopUpdatingLocation {
    log_dbg!("CLLocationManager stopUpdatingLocation");
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).updates_location = false;
}

- (())requestLocation {
    // iOS 9+ one-shot request — same stub behaviour as startUpdatingLocation.
    let _: () = msg![env; this startUpdatingLocation];
}

- (())startMonitoringSignificantLocationChanges {
    log_dbg!("CLLocationManager startMonitoringSignificantLocationChanges — ignored");
}

- (())stopMonitoringSignificantLocationChanges {
    log_dbg!("CLLocationManager stopMonitoringSignificantLocationChanges — ignored");
}

// MARK: Heading updates

- (bool)headingAvailable {
    false
}

- (())startUpdatingHeading {
    log_dbg!("CLLocationManager startUpdatingHeading — heading unavailable");
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).updates_heading = true;
}

- (())stopUpdatingHeading {
    log_dbg!("CLLocationManager stopUpdatingHeading");
    env.objc.borrow_mut::<CLLocationManagerHostObject>(this).updates_heading = false;
}

- (())dismissHeadingCalibrationDisplay {
    // No-op — we never show calibration UI.
}

// MARK: Authorization

- (())requestWhenInUseAuthorization {
    log_dbg!("CLLocationManager requestWhenInUseAuthorization — denied");
    let delegate = env.objc.borrow::<CLLocationManagerHostObject>(this).delegate;
    if delegate != nil {
        // Same reasoning as `startUpdatingLocation` above.
        let sel = env.objc.register_host_selector(
            "locationManager:didChangeAuthorizationStatus:".to_string(),
            &mut env.mem,
        );
        let responds: bool = msg![env; delegate respondsToSelector:sel];
        if responds {
            let _: () = msg![env;
            delegate locationManager:this
                              didChangeAuthorizationStatus:CL_AUTHORIZATION_STATUS_DENIED];
        }
    }
}

- (())requestAlwaysAuthorization {
    let _: () = msg![env; this requestWhenInUseAuthorization];
}

// MARK: Current location (always nil — no location available)

- (id)location { // CLLocation*
    nil
}

- (id)heading { // CLHeading*
    nil
}

@end

// =========================================================================
// MARK: - CLLocation
// =========================================================================

@implementation CLLocation: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CLLocationHostObject {
        latitude:  0.0,
        longitude: 0.0,
        altitude:  0.0,
        horizontal_accuracy: -1.0,
        vertical_accuracy:   -1.0,
        speed: -1.0,
        course: -1.0,
        timestamp: nil,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithLatitude:(CLLocationDegrees)lat
             longitude:(CLLocationDegrees)lon {
    {
        let host = env.objc.borrow_mut::<CLLocationHostObject>(this);
        host.latitude  = lat;
        host.longitude = lon;
        host.horizontal_accuracy = 0.0;
    }
    let ts: id = msg_class![env; NSDate date];
    env.objc.borrow_mut::<CLLocationHostObject>(this).timestamp = ts;
    this
}

// Replace the 6-parameter init with two separate inits that are within
// the HostIMP parameter limit.
- (id)initWithCoordinate:(CLLocationDegrees)lat
              longitude:(CLLocationDegrees)lon
               altitude:(CLLocationDistance)alt
     horizontalAccuracy:(CLLocationAccuracy)h_acc
       verticalAccuracy:(CLLocationAccuracy)v_acc {
    {
        let host = env.objc.borrow_mut::<CLLocationHostObject>(this);
        host.latitude  = lat;
        host.longitude = lon;
        host.altitude  = alt;
        host.horizontal_accuracy = h_acc;
        host.vertical_accuracy   = v_acc;
    }
    let ts: id = msg_class![env; NSDate date];
    env.objc.borrow_mut::<CLLocationHostObject>(this).timestamp = ts;
    this
}

// Apple's full initializer:
// - (id)initWithCoordinate:altitude:horizontalAccuracy:verticalAccuracy:timestamp:
// where coordinate is a CLLocationCoordinate2D struct.
- (id)initWithCoordinate:(CLLocationCoordinate2D)coord
               altitude:(CLLocationDistance)alt
     horizontalAccuracy:(CLLocationAccuracy)h_acc
       verticalAccuracy:(CLLocationAccuracy)v_acc
              timestamp:(id)ts {
    {
        let host = env.objc.borrow_mut::<CLLocationHostObject>(this);
        host.latitude  = coord.latitude;
        host.longitude = coord.longitude;
        host.altitude  = alt;
        host.horizontal_accuracy = h_acc;
        host.vertical_accuracy   = v_acc;
    }
    retain(env, ts);
    env.objc.borrow_mut::<CLLocationHostObject>(this).timestamp = ts;
    this
}

- (())dealloc {
    let ts = env.objc.borrow::<CLLocationHostObject>(this).timestamp;
    release(env, ts);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (CLLocationDegrees)latitude  { env.objc.borrow::<CLLocationHostObject>(this).latitude }
- (CLLocationDegrees)longitude { env.objc.borrow::<CLLocationHostObject>(this).longitude }
- (CLLocationDistance)altitude { env.objc.borrow::<CLLocationHostObject>(this).altitude }
- (CLLocationAccuracy)horizontalAccuracy { env.objc.borrow::<CLLocationHostObject>(this).horizontal_accuracy }
- (CLLocationAccuracy)verticalAccuracy   { env.objc.borrow::<CLLocationHostObject>(this).vertical_accuracy }
- (CLLocationSpeed)speed       { env.objc.borrow::<CLLocationHostObject>(this).speed }
- (CLLocationDirection)course  { env.objc.borrow::<CLLocationHostObject>(this).course }
- (id)timestamp                { env.objc.borrow::<CLLocationHostObject>(this).timestamp }

// In the initializer above, use NSDate for the timestamp automatically.
// Then expose a separate timestamp setter for callers that need it:

- (())_setTimestamp:(id)ts { // NSDate* — private helper
    let old = env.objc.borrow::<CLLocationHostObject>(this).timestamp;
    release(env, old);
    retain(env, ts);
    env.objc.borrow_mut::<CLLocationHostObject>(this).timestamp = ts;
}

- (CLLocationDistance)distanceFromLocation:(id)other { // CLLocation*
    // Simple equirectangular approximation — sufficient for games.
    let (lat1, lon1) = {
        let h = env.objc.borrow::<CLLocationHostObject>(this);
        (h.latitude, h.longitude)
    };
    let (lat2, lon2) = {
        let h = env.objc.borrow::<CLLocationHostObject>(other);
        (h.latitude, h.longitude)
    };
    let earth_r = 6_371_000.0f64; // metres
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    earth_r * c
}

- (id)description { // NSString*
    let (lat, lon, alt, h_acc, v_acc) = {
        let h = env.objc.borrow::<CLLocationHostObject>(this);
        (h.latitude, h.longitude, h.altitude, h.horizontal_accuracy, h.vertical_accuracy)
    };
    let s = format!(
        "<+{:.6},{:.6}> +/- {:.0}m (speed {:.2} mps / course {:.2}) @ alt {:.0}m +/- {:.0}m",
        lat, lon, h_acc, -1.0f64, -1.0f64, alt, v_acc
    );
    let cstr = env.mem.alloc_and_write_cstr(s.as_bytes());
    msg_class![env; NSString stringWithUTF8String:cstr]
}

@end

// =========================================================================
// MARK: - CLHeading
// =========================================================================

@implementation CLHeading: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CLHeadingHostObject {
        magnetic_heading:  0.0,
        true_heading:     -1.0,
        heading_accuracy: -1.0,
        x: 0.0, y: 0.0, z: 0.0,
        timestamp: nil,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let ts = env.objc.borrow::<CLHeadingHostObject>(this).timestamp;
    release(env, ts);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (CLHeadingComponentValue)magneticHeading { env.objc.borrow::<CLHeadingHostObject>(this).magnetic_heading }
- (CLHeadingComponentValue)trueHeading     { env.objc.borrow::<CLHeadingHostObject>(this).true_heading }
- (CLHeadingComponentValue)headingAccuracy { env.objc.borrow::<CLHeadingHostObject>(this).heading_accuracy }
- (CLHeadingComponentValue)x               { env.objc.borrow::<CLHeadingHostObject>(this).x }
- (CLHeadingComponentValue)y               { env.objc.borrow::<CLHeadingHostObject>(this).y }
- (CLHeadingComponentValue)z               { env.objc.borrow::<CLHeadingHostObject>(this).z }
- (id)timestamp                            { env.objc.borrow::<CLHeadingHostObject>(this).timestamp }

- (id)description { // NSString*
    let (mag, tru, acc) = {
        let h = env.objc.borrow::<CLHeadingHostObject>(this);
        (h.magnetic_heading, h.true_heading, h.heading_accuracy)
    };
    let s = format!(
        "<CLHeading: magnetic={:.1} true={:.1} accuracy={:.1}>",
        mag, tru, acc
    );
    let cstr = env.mem.alloc_and_write_cstr(s.as_bytes());
    msg_class![env; NSString stringWithUTF8String:cstr]
}

@end

};

const CONSTANTS: ConstantExports = &[
    // Accuracy presets
    (
        "_kCLLocationAccuracyBestForNavigation",
        HostConstant::Custom(|env| env.mem.alloc_and_write(-2f64).cast().cast_const()),
    ),
    (
        "_kCLLocationAccuracyBest",
        HostConstant::Custom(|env| env.mem.alloc_and_write(-1f64).cast().cast_const()),
    ),
    (
        "_kCLLocationAccuracyNearestTenMeters",
        HostConstant::Custom(|env| env.mem.alloc_and_write(10f64).cast().cast_const()),
    ),
    (
        "_kCLLocationAccuracyHundredMeters",
        HostConstant::Custom(|env| env.mem.alloc_and_write(100f64).cast().cast_const()),
    ),
    (
        "_kCLLocationAccuracyKilometer",
        HostConstant::Custom(|env| env.mem.alloc_and_write(1000f64).cast().cast_const()),
    ),
    (
        "_kCLLocationAccuracyThreeKilometers",
        HostConstant::Custom(|env| env.mem.alloc_and_write(3000f64).cast().cast_const()),
    ),
    // Distance filter
    (
        "_kCLDistanceFilterNone",
        HostConstant::Custom(|env| env.mem.alloc_and_write(-1f64).cast().cast_const()),
    ),
    // Heading filter
    (
        "_kCLHeadingFilterNone",
        HostConstant::Custom(|env| env.mem.alloc_and_write(-1f64).cast().cast_const()),
    ),
    // Error domain
    ("_kCLErrorDomain", HostConstant::NSString("kCLErrorDomain")),
];

// =========================================================================
// MARK: - Region Monitoring (per Apple CLLocationManager docs iOS 4.0+)
// =========================================================================

// Region monitoring is not supported in the emulator (no GPS hardware).
// Per Apple docs, when monitoring is started for a region, the location
// manager calls the delegate's locationManager:monitoringDidFailForRegion:
// withError: method if the region cannot be monitored.
// We don't implement this callback because:
// 1. isMonitoringAvailableForClass: already returns false
// 2. Games check availability before calling startMonitoring
// 3. The ObjC runtime gracefully handles unrecognized selectors
