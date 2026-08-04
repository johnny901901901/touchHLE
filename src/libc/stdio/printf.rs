/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//!
//! `printf` function family. The implementation is also used by `NSLog` etc.

use crate::abi::{DotDotDot, VaList};
use crate::dyld::{export_c_func, FunctionExports};
use crate::frameworks::foundation::{ns_string, unichar};
use crate::libc::clocale::{setlocale, LC_CTYPE};
use crate::libc::errno::set_errno;
use crate::libc::posix_io::{STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
use crate::libc::stdio::{fwrite, getc, ungetc, EOF, FILE};
use crate::libc::stdlib::{atof_inner_generic, str_to_int_inner_generic};
use crate::libc::string::strlen;
use crate::libc::wchar::wchar_t;
use crate::mem::{guest_size_of, ConstPtr, GuestUSize, Mem, MutPtr, MutVoidPtr, Ptr};
use crate::objc::{id, msg, nil};
use crate::Environment;
use std::collections::HashSet;
use std::io::Write;

// ALL_SPECIFIERS: d i o u x X f F e E g G a A c s p n C S % @ D U O = 25
// + b'+' b'#' b'-' would be 28 but those are flags not specifiers — omit them.
// Add b'A' which was missing from the original.
const ALL_SPECIFIERS: [u8; 26] = [
    // IEEE printf specifiers
    b'd', b'i', b'o', b'u', b'x', b'X', b'f', b'F', b'e', b'E', b'g', b'G', b'a', b'A', b'c', b's',
    b'p', b'n', b'C', b'S', b'%', // NSString formatting
    b'@', // Darwin long variants
    b'D', b'U', b'O', // Bracket set specifier for sscanf
    b'[',
];
const INTEGER_SPECIFIERS: [u8; 9] = [b'd', b'i', b'o', b'u', b'x', b'X', b'D', b'U', b'O'];
const FLOAT_SPECIFIERS: [u8; 8] = [b'f', b'F', b'e', b'E', b'g', b'G', b'a', b'A'];
/// String formatting implementation for `printf` and `NSLog` function families.
///
/// `NS_LOG` is [true] for the `NSLog` format string type, or [false] for the
/// `printf` format string type.
///
/// `get_format_char` is a callback that returns the byte at a given index in
/// the format string, or `'\0'` if the index is one past the last byte.
pub fn printf_inner<const NS_LOG: bool, F: Fn(&Mem, GuestUSize) -> u8>(
    env: &mut Environment,
    get_format_char: F,
    mut args: VaList,
) -> Vec<u8> {
    let mut res = Vec::<u8>::new();
    let mut format_char_idx = 0;

    loop {
        let c = get_format_char(&env.mem, format_char_idx);
        format_char_idx += 1;

        if c == b'\0' {
            break;
        }
        if c != b'%' {
            res.push(c);
            continue;
        }

        let prepend_sign = if get_format_char(&env.mem, format_char_idx) == b'+' {
            format_char_idx += 1;
            true
        } else {
            false
        };
        let alternative_form = if get_format_char(&env.mem, format_char_idx) == b'#' {
            // Alternative form handling: adds 0x/0X prefix for hex, 0 prefix
            // for octal
            format_char_idx += 1;
            true
        } else {
            false
        };
        let pad_char = if get_format_char(&env.mem, format_char_idx) == b'0' {
            format_char_idx += 1;
            '0'
        } else {
            ' '
        };
        let left_justified = if get_format_char(&env.mem, format_char_idx) == b'-' {
            format_char_idx += 1;
            true
        } else {
            false
        };
        let pad_width = if get_format_char(&env.mem, format_char_idx) == b'*' {
            let pad_width = args.next::<i32>(env);
            format_char_idx += 1;
            pad_width
        } else {
            let mut pad_width: i32 = 0;
            while let c @ b'0'..=b'9' = get_format_char(&env.mem, format_char_idx) {
                pad_width = pad_width * 10 + (c - b'0') as i32;
                format_char_idx += 1;
            }
            pad_width
        };
        assert!(pad_width >= 0); // TODO: Implement right-padding

        let precision = if get_format_char(&env.mem, format_char_idx) == b'.' {
            format_char_idx += 1;
            let precision = if get_format_char(&env.mem, format_char_idx) == b'*' {
                let precision = args.next::<i32>(env);
                assert!(precision >= 0); // TODO: ignore negative
                format_char_idx += 1;
                precision as usize
            } else {
                let mut precision = 0;
                while let c @ b'0'..=b'9' = get_format_char(&env.mem, format_char_idx) {
                    precision = precision * 10 + (c - b'0') as usize;
                    format_char_idx += 1;
                }
                precision
            };
            Some(precision)
        } else {
            None
        };
        let length_modifier = match get_format_char(&env.mem, format_char_idx) {
            b'h' => {
                format_char_idx += 1;
                if get_format_char(&env.mem, format_char_idx) == b'h' {
                    format_char_idx += 1;
                    Some("hh")
                } else {
                    Some("h")
                }
            }
            b'l' => {
                format_char_idx += 1;
                if get_format_char(&env.mem, format_char_idx) == b'l' {
                    format_char_idx += 1;
                    Some("ll")
                } else {
                    Some("l")
                }
            }
            // q seems to be an equivalent of 'll'
            // https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Strings/Articles/formatSpecifiers.html#//apple_ref/doc/uid/TP40004265-SW1
            b'q' => {
                format_char_idx += 1;
                Some("ll")
            }
            b'j' => {
                format_char_idx += 1;
                Some("ll") // intmax_t (64-bit)
            }
            b'z' | b't' => {
                format_char_idx += 1;
                Some("l") // size_t и ptrdiff_t (32-bit на ARMv6/v7)
            }
            b'L' => {
                format_char_idx += 1;
                Some("L") // long double (эквивалентно 64-bit f64)
            }
            _ => None,
        };
        let specifier = get_format_char(&env.mem, format_char_idx);
        format_char_idx += 1;

        if !ALL_SPECIFIERS.contains(&specifier) {
            // According to `printf` specs, this behaviour is undefined.
            // But as seen on both macOS and iOS, the '%' just got skipped.
            // Also, we need to back-track 1 position
            format_char_idx -= 1;
            continue;
        }

        if specifier == b'\0' {
            // Apparently, errno is not set in this case (tested on macOS),
            // thus we treat this situation as a normal
            // and just stop the formatting.
            assert_eq!(b'%', get_format_char(&env.mem, format_char_idx - 2));
            log!("printf_inner encountered '%' at the end of format string, ignoring.");
            break;
        }
        if specifier == b'%' {
            res.push(b'%');
            continue;
        }

        if precision.is_some() {
            assert!(
                INTEGER_SPECIFIERS.contains(&specifier)
                    || FLOAT_SPECIFIERS.contains(&specifier)
                    || specifier == b's'
            )
        }

        match specifier {
            // Integer specifiers
            b'c' => {
                // '+' flag is undefined for non-numeric conversions; per real
                // printf implementations (glibc / Apple libSystem) it is
                // silently ignored rather than aborting.
                let _ = prepend_sign;

                // Если передали %lc, обрабатываем как широкий символ (аналог
                // %C)
                if length_modifier == Some("l") {
                    let c: wchar_t = args.next(env);
                    // Безопасно парсим, если символ кривой - ставим '?' вместо
                    // краша
                    let ch = char::from_u32(c as u32).unwrap_or('?');
                    write!(&mut res, "{ch}").unwrap();
                } else {
                    let c: u8 = args.next(env);
                    assert!(pad_char == ' ' && pad_width == 0);
                    // TODO
                    res.push(c);
                }
            }
            // Apple extension?
            // Seemingly works in both NSLog and printf.
            // Apple extension?
            // Seemingly works in both NSLog and printf.
            b'C' => {
                // '+' flag is ignored for the (Apple-extension) wide-char
                // conversion — see note for %c above.
                let _ = prepend_sign;
                // Убрали assert!(length_modifier.is_none());
                let c: unichar = args.next(env);
                assert!(pad_char == ' ' && pad_width == 0);
                // Заменяем .unwrap() на .unwrap_or('?'), чтобы не было паники
                // на невалидном UTF-16!
                let c = char::from_u32(c.into()).unwrap_or('?');
                write!(&mut res, "{c}").unwrap();
            }
            b's' => {
                // assert!(!prepend_sign);
                // TODO: support length modifier
                // assert!(length_modifier.is_none());
                let c_string: ConstPtr<u8> = args.next(env);
                // assert!(pad_char == ' ');
                // TODO
                if !c_string.is_null() {
                    if let Some(precision) = precision {
                        let str_len = strlen(env, c_string);
                        res.extend_from_slice(
                            env.mem.bytes_at(c_string, str_len.min(precision as _)),
                        )
                    } else if pad_width > 0 {
                        let pad_width = pad_width as usize;
                        let str = env.mem.cstr_at_utf8(c_string).unwrap();
                        if left_justified {
                            write!(&mut res, "{str:<pad_width$}").unwrap();
                        } else {
                            write!(&mut res, "{str:>pad_width$}").unwrap();
                        }
                    } else {
                        res.extend_from_slice(env.mem.cstr_at(c_string));
                    }
                } else {
                    // assert!(precision.is_none());
                    res.extend_from_slice("(null)".as_bytes());
                }
            }
            b'S' => {
                // '+' flag is undefined for string conversions; ignore it
                // rather than aborting (matches glibc / Apple behaviour).
                let _ = prepend_sign;
                // Убрали assert!(length_modifier.is_none());
                // TODO: support other locales
                let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
                assert_eq!(env.mem.read(ctype_locale), b'C');
                let w_string: ConstPtr<wchar_t> = args.next(env);
                assert!(pad_char == ' ' && pad_width == 0);
                // TODO
                if !w_string.is_null() {
                    res.extend_from_slice(env.mem.wcstr_at(w_string).as_bytes());
                } else {
                    res.extend_from_slice("(null)".as_bytes());
                }
            }
            b'd' | b'i' => {
                let int: i64 = if length_modifier == Some("ll") {
                    args.next(env)
                } else if length_modifier == Some("hh") {
                    let int: i8 = args.next(env);
                    int.into()
                } else if length_modifier == Some("h") {
                    let int: i16 = args.next(env);
                    int.into()
                } else {
                    let int: i32 = args.next(env);
                    int.into()
                };
                let s = apply_int_pad(
                    int,
                    pad_width as usize,
                    pad_char,
                    left_justified,
                    prepend_sign,
                    precision,
                );
                res.extend_from_slice(s.as_bytes());
            }
            b'u' => {
                let uint: u64 = if length_modifier == Some("ll") {
                    args.next(env)
                } else if length_modifier == Some("hh") {
                    let uint: u8 = args.next(env);
                    uint.into()
                } else if length_modifier == Some("h") {
                    let uint: u16 = args.next(env);
                    uint.into()
                } else {
                    let uint: u32 = args.next(env);
                    uint.into()
                };
                let s = apply_uint_pad(
                    uint,
                    pad_width as usize,
                    pad_char,
                    left_justified,
                    precision,
                );
                res.extend_from_slice(s.as_bytes());
            }
            b'@' if NS_LOG => {
                // '+' flag is undefined for Cocoa's %@ object conversion;
                // ignore it instead of aborting.
                let _ = prepend_sign;
                // Убрали assert!(length_modifier.is_none());
                let object: id = args.next(env);
                // TODO: use localized description if available?
                let description: id = msg![env; object description];
                if description != nil {
                    // TODO: avoid copy
                    // TODO: what if the description isn't valid UTF-16?
                    let description = ns_string::to_rust_string(env, description);
                    write!(&mut res, "{description}").unwrap();
                } else {
                    write!(&mut res, "(null)").unwrap();
                }
            }
            b'o' => {
                // Octal specifier
                // '+' flag is undefined for unsigned conversions; ignore it.
                let _ = prepend_sign;
                let uint: u64 = if length_modifier == Some("ll") {
                    args.next(env)
                } else if length_modifier == Some("hh") {
                    let uint: u8 = args.next(env);
                    uint.into()
                } else if length_modifier == Some("h") {
                    let uint: u16 = args.next(env);
                    uint.into()
                } else {
                    let uint: u32 = args.next(env);
                    uint.into()
                };
                let prefix = if alternative_form && uint != 0 {
                    "0"
                } else {
                    ""
                };
                if pad_width > 0 {
                    let pad_width = pad_width as usize;
                    let formatted = format!("{}{:o}", prefix, uint);
                    if pad_char == '0' && precision.is_none() && !left_justified {
                        // For zero padding with alternative form, prefix goes
                        // before zeros
                        if alternative_form && uint != 0 {
                            let remaining_width = pad_width.saturating_sub(1);
                            write!(&mut res, "0{:0>1$o}", uint, remaining_width).unwrap();
                        } else {
                            write!(&mut res, "{:0>pad_width$o}", uint).unwrap();
                        }
                    } else if left_justified {
                        write!(&mut res, "{:<pad_width$}", formatted).unwrap();
                    } else {
                        write!(&mut res, "{:>pad_width$}", formatted).unwrap();
                    }
                } else {
                    let tmp = if precision.is_some_and(|value| value > 0) {
                        let prec = precision.unwrap();
                        format!("{}{:0>prec$o}", prefix, uint, prec = prec)
                    } else {
                        format!("{}{:o}", prefix, uint)
                    };
                    res.extend_from_slice(tmp.as_bytes());
                }
            }
            b'x' => {
                // '+' flag is undefined for unsigned conversions; ignore it.
                let _ = prepend_sign;
                let uint: u64 = if length_modifier == Some("ll") {
                    args.next(env)
                } else if length_modifier == Some("hh") {
                    let uint: u8 = args.next(env);
                    uint.into()
                } else if length_modifier == Some("h") {
                    let uint: u16 = args.next(env);
                    uint.into()
                } else {
                    let uint: u32 = args.next(env);
                    uint.into()
                };
                let prefix = if alternative_form && uint != 0 {
                    "0x"
                } else {
                    ""
                };
                if pad_width > 0 {
                    let pad_width = pad_width as usize;
                    // When precision is specified, it gives minimum digits
                    // (zero-padded); pad_char '0' width-padding is suppressed
                    // per POSIX/C99 when precision is given.
                    let digits = if let Some(prec) = precision {
                        format!("{:0>prec$x}", uint, prec = prec)
                    } else {
                        format!("{:x}", uint)
                    };
                    let formatted_with_prefix = format!("{}{}", prefix, digits);
                    if pad_char == '0' && precision.is_none() && !left_justified {
                        // For zero padding with alternative form, prefix goes
                        // before zeros
                        if alternative_form && uint != 0 {
                            let remaining_width = pad_width.saturating_sub(2);
                            write!(&mut res, "0x{:0>1$x}", uint, remaining_width).unwrap();
                        } else {
                            write!(&mut res, "{:0>pad_width$x}", uint).unwrap();
                        }
                    } else if left_justified {
                        write!(&mut res, "{:<pad_width$}", formatted_with_prefix).unwrap();
                    } else {
                        write!(&mut res, "{:>pad_width$}", formatted_with_prefix).unwrap();
                    }
                } else {
                    let tmp = if let Some(prec) = precision {
                        if prec == 0 && uint == 0 {
                            // %.0x with value 0 produces no output per C99
                            String::new()
                        } else {
                            format!("{}{:0>prec$x}", prefix, uint, prec = prec)
                        }
                    } else {
                        format!("{}{:x}", prefix, uint)
                    };
                    res.extend_from_slice(tmp.as_bytes());
                }
            }
            b'X' => {
                // '+' flag is undefined for unsigned conversions; ignore it.
                let _ = prepend_sign;
                let uint: u64 = if length_modifier == Some("ll") {
                    args.next(env)
                } else if length_modifier == Some("hh") {
                    let uint: u8 = args.next(env);
                    uint.into()
                } else if length_modifier == Some("h") {
                    let uint: u16 = args.next(env);
                    uint.into()
                } else {
                    let uint: u32 = args.next(env);
                    uint.into()
                };
                let prefix = if alternative_form && uint != 0 {
                    "0X"
                } else {
                    ""
                };
                if pad_width > 0 {
                    let pad_width = pad_width as usize;
                    // When precision is specified, it gives minimum digits
                    // (zero-padded); pad_char '0' width-padding is suppressed
                    // per POSIX/C99 when precision is given.
                    let digits = if let Some(prec) = precision {
                        format!("{:0>prec$X}", uint, prec = prec)
                    } else {
                        format!("{:X}", uint)
                    };
                    let formatted_with_prefix = format!("{}{}", prefix, digits);
                    if pad_char == '0' && precision.is_none() && !left_justified {
                        // For zero padding with alternative form, prefix goes
                        // before zeros
                        if alternative_form && uint != 0 {
                            let remaining_width = pad_width.saturating_sub(2);
                            write!(&mut res, "0X{:0>1$X}", uint, remaining_width).unwrap();
                        } else {
                            write!(&mut res, "{:0>pad_width$X}", uint).unwrap();
                        }
                    } else if left_justified {
                        write!(&mut res, "{:<pad_width$}", formatted_with_prefix).unwrap();
                    } else {
                        write!(&mut res, "{:>pad_width$}", formatted_with_prefix).unwrap();
                    }
                } else {
                    let tmp = if let Some(prec) = precision {
                        if prec == 0 && uint == 0 {
                            // %.0X with value 0 produces no output per C99
                            String::new()
                        } else {
                            format!("{}{:0>prec$X}", prefix, uint, prec = prec)
                        }
                    } else {
                        format!("{}{:X}", prefix, uint)
                    };
                    res.extend_from_slice(tmp.as_bytes());
                }
            }
            b'p' => {
                // '+' flag is undefined for pointer conversions; ignore it.
                let _ = prepend_sign;
                // Убрали assert!(length_modifier.is_none());
                let ptr: MutVoidPtr = args.next(env);
                // '%p' is implementation defined,
                // but this matches iOS simulator output
                let tmp = format!("{:#x}", ptr.to_bits());
                if pad_width > 0 {
                    let pad_width = pad_width as usize;
                    assert!(pad_char == ' '); // TODO
                    if left_justified {
                        write!(&mut res, "{tmp:<pad_width$}").unwrap();
                    } else {
                        write!(&mut res, "{tmp:>pad_width$}").unwrap();
                    }
                } else {
                    res.extend_from_slice(tmp.as_bytes());
                }
            }
            // Float specifiers
            b'f' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                let precision = precision.unwrap_or(6);
                let formatted = f_format(float, pad_width, pad_char, precision, left_justified);
                let formatted = apply_float_sign(&formatted, float, prepend_sign);
                res.extend_from_slice(formatted.as_bytes());
            }
            b'e' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                let precision = precision.unwrap_or(6);
                let formatted = e_format(float, pad_width, pad_char, precision, left_justified);
                let formatted = apply_float_sign(&formatted, float, prepend_sign);
                res.extend_from_slice(formatted.as_bytes());
            }
            b'g' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                // Reference https://en.cppreference.com/w/c/io/vfprintf
                let P: i32 = if let Some(precision) = precision {
                    if precision == 0 {
                        1
                    } else {
                        precision.try_into().unwrap()
                    }
                } else {
                    6
                };
                let X: i32 = if float == 0.0 {
                    0
                } else {
                    float.abs().log10().floor() as i32
                };
                log_dbg!(
                    "float {}, pad_width {}, pad_char '{}', P {}, X {}",
                    float,
                    pad_width,
                    pad_char,
                    P,
                    X
                );
                if P > X && X >= -4 {
                    let precision: usize = (P - X - 1).try_into().unwrap();
                    let result = f_format(float, pad_width, pad_char, precision, left_justified);

                    // TODO: skip if alternative representation is requested
                    let trimmed_result = if result.contains('.') {
                        result.trim_end_matches('0').trim_end_matches('.')
                    } else {
                        &result
                    };
                    let trimmed_result = if pad_width > 0 && trimmed_result.len() < pad_width {
                        if left_justified {
                            format!("{trimmed_result:<pad_width$}")
                        } else if pad_char == '0' {
                            format!("{trimmed_result:0>pad_width$}")
                        } else {
                            format!("{trimmed_result:>pad_width$}")
                        }
                    } else {
                        trimmed_result.to_string()
                    };
                    let trimmed_result =
                        apply_float_sign(&trimmed_result, float, prepend_sign);
                    res.extend_from_slice(trimmed_result.as_bytes());
                } else {
                    let precision: usize = (P - 1).try_into().unwrap();
                    let formatted = e_format(float, pad_width, pad_char, precision, left_justified);
                    let formatted = apply_float_sign(&formatted, float, prepend_sign);
                    res.extend_from_slice(formatted.as_bytes());
                }
            }
            b'F' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                let precision = precision.unwrap_or(6);
                let s = format!("{float:.precision$}").to_uppercase();
                let s = apply_pad(&s, pad_width, pad_char, left_justified);
                let s = apply_float_sign(&s, float, prepend_sign);
                res.extend_from_slice(s.as_bytes());
            }
            b'E' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                let precision = precision.unwrap_or(6);
                let s =
                    e_format(float, pad_width, pad_char, precision, left_justified).to_uppercase();
                let s = apply_float_sign(&s, float, prepend_sign);
                res.extend_from_slice(s.as_bytes());
            }
            b'G' => {
                let float: f64 = args.next(env);
                let pad_width = pad_width as usize;
                let p: i32 = precision
                    .map(|p| if p == 0 { 1 } else { p as i32 })
                    .unwrap_or(6);
                let x: i32 = if float == 0.0 {
                    0
                } else {
                    float.abs().log10().floor() as i32
                };
                let s = if p > x && x >= -4 {
                    let prec = (p - x - 1) as usize;
                    let raw = f_format(float, 0, ' ', prec, false);
                    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
                    apply_pad(trimmed, pad_width, pad_char, left_justified)
                } else {
                    e_format(float, pad_width, pad_char, (p - 1) as usize, left_justified)
                };
                let s = apply_float_sign(&s.to_uppercase(), float, prepend_sign);
                res.extend_from_slice(s.as_bytes());
            }
            b'a' => {
                let float: f64 = args.next(env);
                let s = format!("{:e}", float).replace('e', "p");
                let s = apply_pad(&s, pad_width as usize, pad_char, left_justified);
                let s = apply_float_sign(&s, float, prepend_sign);
                res.extend_from_slice(s.as_bytes());
            }
            b'A' => {
                let float: f64 = args.next(env);
                let s = format!("{:e}", float).replace('e', "P").to_uppercase();
                let s = apply_pad(&s, pad_width as usize, pad_char, left_justified);
                let s = apply_float_sign(&s, float, prepend_sign);
                res.extend_from_slice(s.as_bytes());
            }
            b'n' => {
                let ptr: MutPtr<i32> = args.next(env);
                if !ptr.is_null() {
                    env.mem.write(ptr, res.len() as i32);
                }
            }
            b'D' => {
                let int: i32 = args.next(env);
                let s = apply_int_pad(
                    int as i64,
                    pad_width as usize,
                    pad_char,
                    left_justified,
                    prepend_sign,
                    precision,
                );
                res.extend_from_slice(s.as_bytes());
            }
            b'U' => {
                let uint: u32 = args.next(env);
                let s = apply_uint_pad(
                    uint as u64,
                    pad_width as usize,
                    pad_char,
                    left_justified,
                    precision,
                );
                res.extend_from_slice(s.as_bytes());
            }
            b'O' => {
                let uint: u32 = args.next(env);
                let prefix = if alternative_form && uint != 0 {
                    "0"
                } else {
                    ""
                };
                let s = format!("{}{:o}", prefix, uint);
                let s = apply_pad(&s, pad_width as usize, pad_char, left_justified);
                res.extend_from_slice(s.as_bytes());
            }
            _ => {
                log_dbg!(
                    "printf_inner: unhandled specifier '%{}' at index {} — skipping",
                    specifier as char,
                    format_char_idx
                );
            }
        } // end match specifier
    } // end loop

    log_dbg!("=> {:?}", std::str::from_utf8(&res));
    res
} // end printf_inner — THIS is the only closing brace needed here

fn f_format(
    float: f64,
    pad_width: usize,
    pad_char: char,
    precision: usize,
    left_justified: bool,
) -> String {
    if left_justified {
        format!("{float:<pad_width$.precision$}")
    } else if pad_char == '0' {
        format!("{float:0>pad_width$.precision$}")
    } else {
        assert!(pad_char == ' ');
        // TODO
        format!("{float:>pad_width$.precision$}")
    }
}

fn e_format(
    float: f64,
    pad_width: usize,
    pad_char: char,
    precision: usize,
    left_justified: bool,
) -> String {
    let exponent = if float == 0.0 {
        0.0
    } else {
        float.abs().log10().floor()
    };
    let mantissa = float.abs() / 10f64.powf(exponent);
    let sign = if float.is_sign_negative() { "-" } else { "" };
    let float_exp_notation = format!("{mantissa:.precision$}e{exponent:+03}");
    let full_str = format!("{sign}{float_exp_notation}");

    if left_justified {
        format!("{full_str:<pad_width$}")
    } else if pad_char == '0' {
        format!(
            "{0}{1:0>2$}",
            sign,
            float_exp_notation,
            pad_width.saturating_sub(sign.len())
        )
    } else {
        assert!(pad_char == ' ');
        // TODO
        format!("{full_str:>pad_width$}")
    }
}

fn apply_pad(s: &str, pad_width: usize, pad_char: char, left_justified: bool) -> String {
    if pad_width == 0 || s.len() >= pad_width {
        return s.to_string();
    }
    if left_justified {
        format!("{s:<pad_width$}")
    } else if pad_char == '0' {
        format!("{s:0>pad_width$}")
    } else {
        format!("{s:>pad_width$}")
    }
}

/// Apply the `+` flag for a signed floating-point conversion.
///
/// Per C99 §7.19.6.1 / Apple's printf(3) manual, the `+` flag forces a sign
/// character to always be placed before a value produced by a signed
/// conversion. The width-padding has already been applied, so we splice the
/// `+` in front of the first non-space character (preserving any leading
/// padding that was already added for right-justified output).
fn apply_float_sign(formatted: &str, value: f64, prepend_sign: bool) -> String {
    if !prepend_sign {
        return formatted.to_string();
    }
    // The value is already negative-prefixed by Rust's float formatter; only
    // act on non-negative values (including +0.0 and NaN, which Apple's
    // printf treats as positive).
    if value.is_sign_negative() {
        return formatted.to_string();
    }
    // Find the first non-space character; if the string was right-justified
    // with spaces we want to put the '+' immediately before the digits and
    // consume one leading space so the overall field width is preserved.
    if let Some(first_non_space) = formatted.find(|c: char| c != ' ') {
        if first_non_space > 0 {
            // Replace one leading space with '+'.
            let mut out = String::with_capacity(formatted.len());
            out.push_str(&formatted[..first_non_space - 1]);
            out.push('+');
            out.push_str(&formatted[first_non_space..]);
            return out;
        }
        // No leading spaces — prepend '+'. This may grow the field by 1,
        // which matches glibc/Apple behaviour when width <= len.
        let mut out = String::with_capacity(formatted.len() + 1);
        out.push('+');
        out.push_str(formatted);
        return out;
    }
    // String is entirely whitespace (shouldn't happen for a real number).
    let mut out = String::with_capacity(formatted.len() + 1);
    out.push('+');
    out.push_str(formatted);
    out
}

fn apply_int_pad(
    int: i64,
    pad_width: usize,
    pad_char: char,
    left_justified: bool,
    prepend_sign: bool,
    precision: Option<usize>,
) -> String {
    let with_prec = if precision.is_some_and(|p| p > 0) {
        format!("{:01$}", int, precision.unwrap())
    } else {
        format!("{int}")
    };
    if pad_width == 0 {
        if prepend_sign && int >= 0 {
            return format!("+{with_prec}");
        }
        return with_prec;
    }
    if pad_char == '0' && precision.is_none() && !left_justified {
        if prepend_sign {
            let sign = if int >= 0 { '+' } else { '-' };
            let abs_str = format!("{:0>1$}", int.unsigned_abs(), pad_width.saturating_sub(1));
            format!("{sign}{abs_str}")
        } else {
            format!("{int:0>pad_width$}")
        }
    } else if left_justified {
        format!("{with_prec:<pad_width$}")
    } else {
        format!("{with_prec:>pad_width$}")
    }
}

fn apply_uint_pad(
    uint: u64,
    pad_width: usize,
    pad_char: char,
    left_justified: bool,
    precision: Option<usize>,
) -> String {
    let with_prec = if precision.is_some_and(|p| p > 0) {
        format!("{:01$}", uint, precision.unwrap())
    } else {
        format!("{uint}")
    };
    apply_pad(&with_prec, pad_width, pad_char, left_justified)
}

fn snprintf(
    env: &mut Environment,
    dest: MutPtr<u8>,
    n: GuestUSize,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!("snprintf() implemented as a wrapper of vsnprintf()");

    vsnprintf(env, dest, n, format, args.start())
}

fn vasprintf(
    env: &mut Environment,
    ret: MutPtr<MutPtr<u8>>,
    format: ConstPtr<u8>,
    arg: VaList,
) -> i32 {
    log_dbg!(
        "vasprintf({:?}, {:?} ({:?}), ...)",
        ret,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), arg);
    let count: GuestUSize = (res.len() + 1).try_into().unwrap();
    let dest: MutPtr<u8> = env.mem.alloc(count * guest_size_of::<u8>()).cast();

    let dest_slice = env.mem.bytes_at_mut(dest, count);
    for (i, &byte) in res.iter().chain(b"\0".iter()).enumerate() {
        dest_slice[i] = byte;
    }

    env.mem.write(ret, dest);

    res.len().try_into().unwrap()
}

fn vprintf(env: &mut Environment, format: ConstPtr<u8>, arg: VaList) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "vprintf({:?} ({:?}), ...)",
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), arg);
    // TODO: I/O error handling
    let _ = crate::guest_console::stdout().write_all(&res);
    res.len().try_into().unwrap()
}

fn vsnprintf(
    env: &mut Environment,
    dest: MutPtr<u8>,
    n: GuestUSize,
    format: ConstPtr<u8>,
    arg: VaList,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "vsnprintf({:?} {:?} {:?})",
        dest,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), arg);
    if n == 0 {
        return res.len().try_into().unwrap();
    }
    let middle = if ((n - 1) as usize) < res.len() {
        &res[..(n - 1) as usize]
    } else {
        &res[..]
    };
    let dest_slice = env.mem.bytes_at_mut(dest, n);
    for (i, &byte) in middle.iter().chain(b"\0".iter()).enumerate() {
        dest_slice[i] = byte;
    }

    res.len().try_into().unwrap()
}

fn __vsnprintf_chk(
    env: &mut Environment,
    s: MutPtr<u8>,
    maxlen: GuestUSize,
    _flag: i32,
    _os: GuestUSize,
    format: ConstPtr<u8>,
    ap: VaList,
) -> i32 {
    vsnprintf(env, s, maxlen, format, ap)
}

/// `int __snprintf_chk(char *str, size_t maxlen, int flag, size_t strlen,
///                     const char *format, ...)` — the `_FORTIFY_SOURCE`
/// wrapper around `snprintf` emitted by Apple's libc / clang when an iOS
/// app is built with hardening. `strlen` is the compiler-known size of the
/// destination buffer; `maxlen` is the user-supplied `n` parameter. The
/// real runtime aborts the process with `__chk_fail()` when `maxlen >
/// strlen`. For touchHLE we log loudly and clamp `maxlen` so we don't take
/// down the host process, then delegate to the underlying `vsnprintf`.
fn __snprintf_chk(
    env: &mut Environment,
    s: MutPtr<u8>,
    maxlen: GuestUSize,
    _flag: i32,
    strlen: GuestUSize,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    let effective_maxlen = if strlen != 0 && maxlen > strlen {
        log!(
            "Warning: __snprintf_chk: maxlen ({}) > strlen ({}); clamping to strlen (real libc would abort with __chk_fail).",
            maxlen, strlen
        );
        strlen
    } else {
        maxlen
    };
    set_errno(env, 0);
    vsnprintf(env, s, effective_maxlen, format, args.start())
}

fn __vsprintf_chk(
    env: &mut Environment,
    s: MutPtr<u8>,
    maxlen: GuestUSize,
    _flag: i32,
    _os: GuestUSize,
    format: ConstPtr<u8>,
    ap: VaList,
) -> i32 {
    vsnprintf(env, s, maxlen, format, ap)
}

fn vsprintf(env: &mut Environment, dest: MutPtr<u8>, format: ConstPtr<u8>, arg: VaList) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "vsprintf({:?}, {:?} ({:?}), ...)",
        dest,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), arg);
    let dest_slice = env
        .mem
        .bytes_at_mut(dest, (res.len() + 1).try_into().unwrap());
    for (i, &byte) in res.iter().chain(b"\0".iter()).enumerate() {
        dest_slice[i] = byte;
    }

    res.len().try_into().unwrap()
}

fn __sprintf_chk(
    env: &mut Environment,
    dest: MutPtr<u8>,
    _flags: i32,
    strlen: GuestUSize,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    if strlen == 0 {
        // __sprintf_chk is the FORTIFY_SOURCE wrapper around sprintf(); a
        // zero-byte destination means there is nowhere to even write the
        // terminating NUL. Real glibc/Apple libc would call __chk_fail()
        // and abort the process. Log loudly and return 0 ("nothing was
        // written") instead of crashing the host so an end-of-life iOS app
        // doesn't take down the emulator.
        log!(
            "Warning: __sprintf_chk(dest={:?}, strlen=0, format={:?}): zero-byte destination, refusing to write.",
            dest,
            env.mem.cstr_at_utf8(format)
        );
        return 0;
    }
    // TODO: respect flags level
    // TODO: full overflow check
    sprintf(env, dest, format, args)
}

// Locale-aware printf variants from `xlocale.h`. touchHLE only supports a
// single (C / system default) locale, so we ignore the `locale_t` argument
// and dispatch to the locale-less implementation.

fn sprintf_l(
    env: &mut Environment,
    dest: MutPtr<u8>,
    _loc: MutVoidPtr,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    sprintf(env, dest, format, args)
}

fn snprintf_l(
    env: &mut Environment,
    dest: MutPtr<u8>,
    n: GuestUSize,
    _loc: MutVoidPtr,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    vsnprintf(env, dest, n, format, args.start())
}

fn vsprintf_l(
    env: &mut Environment,
    dest: MutPtr<u8>,
    _loc: MutVoidPtr,
    format: ConstPtr<u8>,
    arg: VaList,
) -> i32 {
    vsprintf(env, dest, format, arg)
}

fn vsnprintf_l(
    env: &mut Environment,
    dest: MutPtr<u8>,
    n: GuestUSize,
    _loc: MutVoidPtr,
    format: ConstPtr<u8>,
    arg: VaList,
) -> i32 {
    vsnprintf(env, dest, n, format, arg)
}

fn sprintf(env: &mut Environment, dest: MutPtr<u8>, format: ConstPtr<u8>, args: DotDotDot) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "sprintf({:?}, {:?} ({:?}), ...)",
        dest,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), args.start());
    let dest_slice = env
        .mem
        .bytes_at_mut(dest, (res.len() + 1).try_into().unwrap());
    for (i, &byte) in res.iter().chain(b"\0".iter()).enumerate() {
        dest_slice[i] = byte;
    }

    res.len().try_into().unwrap()
}

fn swprintf(
    env: &mut Environment,
    ws: MutPtr<wchar_t>,
    n: GuestUSize,
    format: ConstPtr<wchar_t>,
    args: DotDotDot,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!("swprintf() implemented as a wrapper of vswprintf()");

    vswprintf(env, ws, n, format, args.start())
}

fn vswprintf(
    env: &mut Environment,
    ws: MutPtr<wchar_t>,
    n: GuestUSize,
    format: ConstPtr<wchar_t>,
    args: VaList,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    // TODO: support other locales
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');

    let wcstr_format = env.mem.wcstr_at(format);
    log_dbg!(
        "vswprintf({:?}, {}, {:?} ({:?}), ...)",
        ws,
        n,
        format,
        wcstr_format
    );
    let wcstr_format_bytes = wcstr_format.as_bytes();
    let len: GuestUSize = wcstr_format_bytes.len() as GuestUSize;
    let res = printf_inner::<false, _>(
        env,
        |_mem, idx| {
            if idx == len {
                b'\0'
            } else {
                wcstr_format_bytes[idx as usize]
            }
        },
        args,
    );
    let to_write = n.min(res.len() as GuestUSize);
    for i in 0..to_write {
        env.mem.write(ws + i, res[i as usize] as wchar_t);
    }
    if to_write >= n {
        // TODO: set errno
        return -1;
    }
    env.mem.write(ws + to_write, wchar_t::default());
    to_write as i32
}

fn wprintf(env: &mut Environment, format: ConstPtr<wchar_t>, args: DotDotDot) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    // TODO: support other locales
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');

    let wcstr_format = env.mem.wcstr_at(format);
    log_dbg!("wprintf({:?} ({:?}), ...)", format, wcstr_format);

    let wcstr_format_bytes = wcstr_format.as_bytes();
    let len: GuestUSize = wcstr_format_bytes.len() as GuestUSize;
    let res = printf_inner::<false, _>(
        env,
        |_mem, idx| {
            if idx == len {
                b'\0'
            } else {
                wcstr_format_bytes[idx as usize]
            }
        },
        args.start(),
    );

    let _ = crate::guest_console::stdout().write_all(&res);
    res.len().try_into().unwrap()
}

fn printf(env: &mut Environment, format: ConstPtr<u8>, args: DotDotDot) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "printf({:?} ({:?}), ...)",
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), args.start());
    // TODO: I/O error handling
    let _ = crate::guest_console::stdout().write_all(&res);
    res.len().try_into().unwrap()
}

// TODO: more printf variants

/// A simple wrapper around [sscanf_common_generic] for the case of C string.
fn sscanf_common(
    env: &mut Environment,
    src: ConstPtr<u8>,
    format: ConstPtr<u8>,
    args: VaList,
) -> i32 {
    sscanf_common_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        src.cast_mut(),
        format,
        args,
    )
}

/// Formatted scan implementation for `sscanf` family.
///
/// `getc_fn` is a callback to get next character from `subject`.
/// 3rd parameter in this callback is a index which is safe to ignore
/// (for example, in case of a file stream).
/// Error signifies an abnormal stop of input,
/// such as [crate::libc::stdio::EOF] in the file stream.
/// Note: `'\0'` does not necessary expect to produce an error!
///
/// `ungetc_fn` is a callback to un-get character from `subject`.
/// Could be ignored entirely (for example, in case of a string).
///
/// `subject` is either C string or file stream (for now).
///
/// `args` is the list of arguments to store produced outputs.
///
/// TODO: instead of `u8: From<T>` constraint, implement a conversion callback
fn sscanf_common_generic<
    T,
    U,
    F1: Fn(&mut Environment, MutPtr<U>, GuestUSize) -> Result<T, ()>,
    F2: Fn(&mut Environment, MutPtr<U>, u8), // TODO: make last param generic too?
>(
    env: &mut Environment,
    getc_fn: F1,
    ungetc_fn: F2,
    subject: MutPtr<U>,
    format: ConstPtr<u8>,
    mut args: VaList,
) -> i32
where
    u8: From<T>,
{
    let mut src_char_idx = 0;
    let mut format_char_idx = 0;

    let mut matched_args = 0;
    'outer: loop {
        let c = env.mem.read(format + format_char_idx);
        format_char_idx += 1;
        if c == b'\0' {
            break;
        }
        if c != b'%' {
            let x = getc_fn(env, subject, src_char_idx);
            if x.is_err() {
                break 'outer;
            }
            let mut cc: u8 = x.unwrap().into();

            if isspace(env, format + format_char_idx - 1) {
                // "any single whitespace character in the format string
                // consumes all available consecutive whitespace characters
                // from the input"
                let mut eof = false;
                while isspace_inner(cc) {
                    src_char_idx += 1;
                    let x2 = getc_fn(env, subject, src_char_idx);
                    if x2.is_err() {
                        eof = true;
                        break;
                    }
                    cc = x2.unwrap().into();
                }
                if !eof {
                    // backtrack one
                    ungetc_fn(env, subject, cc);
                }
                continue;
            }
            if c != cc {
                return matched_args;
            }
            src_char_idx += 1;
            continue;
        }

        let suppress_assignment = if env.mem.read(format + format_char_idx) == b'*' {
            format_char_idx += 1;
            true
        } else {
            false
        };
        let mut max_width: u32 = 0;
        while let c @ b'0'..=b'9' = env.mem.read(format + format_char_idx) {
            max_width = max_width * 10 + (c - b'0') as u32;
            format_char_idx += 1;
        }

        let length_modifier = match env.mem.read(format + format_char_idx) {
            b'h' => {
                format_char_idx += 1;
                if env.mem.read(format + format_char_idx) == b'h' {
                    format_char_idx += 1;
                    Some("hh")
                } else {
                    Some("h")
                }
            }
            b'l' => {
                format_char_idx += 1;
                if env.mem.read(format + format_char_idx) == b'l' {
                    format_char_idx += 1;
                    Some("ll")
                } else {
                    Some("l")
                }
            }
            // q seems to be an equivalent of 'll'
            // https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Strings/Articles/formatSpecifiers.html#//apple_ref/doc/uid/TP40004265-SW1
            b'q' => {
                format_char_idx += 1;
                Some("ll")
            }

            _ => None,
        };
        let specifier = env.mem.read(format + format_char_idx);
        format_char_idx += 1;

        if ![b'[', b'c', b'n'].contains(&specifier) {
            // skip whitespaces
            let x = getc_fn(env, subject, src_char_idx);
            if x.is_err() {
                break 'outer;
            }
            let mut cc: u8 = x.unwrap().into();
            let mut eof = false;
            while isspace_inner(cc) {
                src_char_idx += 1;
                let x2 = getc_fn(env, subject, src_char_idx);
                if x2.is_err() {
                    eof = true;
                    break;
                }
                cc = x2.unwrap().into();
            }
            if eof {
                break 'outer;
            }
            // backtrack one
            ungetc_fn(env, subject, cc);
        }

        match specifier {
            b'd' | b'i' => {
                let base: u32 = if specifier == b'd' {
                    10
                } else {
                    // automatic base detection in strtol
                    0
                };
                match length_modifier {
                    Some(lm) => {
                        match lm {
                            "h" => {
                                // signed short*
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| i16::from_str_radix(s, base).unwrap_or(i16::MAX),
                                    |num| num.checked_mul(-1).unwrap_or(i16::MIN),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<i16> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            "hh" => {
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| i8::from_str_radix(s, base).unwrap_or(i8::MAX) as i16,
                                    |num| num.checked_mul(-1).unwrap_or(i16::MIN),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<i8> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val as i8);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            "l" => {
                                // On 32-bit ARM, long is 32-bit.
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| i32::from_str_radix(s, base).unwrap_or(i32::MAX),
                                    |num| num.checked_mul(-1).unwrap_or(i32::MIN),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<i32> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            "ll" => {
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| i64::from_str_radix(s, base).unwrap_or(i64::MAX),
                                    |num| num.checked_mul(-1).unwrap_or(i64::MIN),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<i64> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            _ => {
                                log!("Warning: sscanf %d/%i: unhandled length modifier '{}' — skipping", lm);
                                break;
                            }
                        }
                    }
                    _ => {
                        let res = str_to_int_inner_generic(
                            env,
                            &getc_fn,
                            &ungetc_fn,
                            subject,
                            src_char_idx,
                            base,
                            if max_width > 0 { max_width } else { u32::MAX },
                            |s, base| i32::from_str_radix(s, base).unwrap_or(i32::MAX),
                            |num| num.checked_mul(-1).unwrap_or(i32::MIN),
                        );
                        match res {
                            Ok((val, len)) => {
                                src_char_idx += len;
                                if !suppress_assignment {
                                    let c_int_ptr: ConstPtr<i32> = args.next(env);
                                    env.mem.write(c_int_ptr.cast_mut(), val);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            b'f' | b'g' => {
                // NOTE: max_width prefix (e.g. "%5f") is ignored — the
                // float parser reads whatever the input looks like.
                // Tightening this would require teaching
                // atof_inner_generic about a max-width budget; for now
                // an over-read is the same behaviour as before.
                let _ = max_width;
                let res = atof_inner_generic(env, &getc_fn, &ungetc_fn, subject, src_char_idx);
                let val = match res {
                    Ok((val, len)) => {
                        src_char_idx += len;
                        val
                    }
                    Err(_) => break,
                };
                match length_modifier {
                    None => {
                        if !suppress_assignment {
                            let c_int_ptr: ConstPtr<f32> = args.next(env);
                            env.mem.write(c_int_ptr.cast_mut(), val as f32);
                        }
                    }
                    Some("l") => {
                        if !suppress_assignment {
                            let c_int_ptr: ConstPtr<f64> = args.next(env);
                            env.mem.write(c_int_ptr.cast_mut(), val);
                        }
                    }
                    Some(modifier) => {
                        // Length modifier we don't model on a %f/%g
                        // specifier. Drop the parsed value and abort the
                        // scan rather than crashing the host.
                        log!(
                            "Warning: sscanf(): unsupported length modifier '{}' for 'f'/'g'; stopping scan.",
                            modifier
                        );
                        break;
                    }
                }
            }
            b'x' | b'X' | b'u' => {
                let base: u32 = match specifier {
                    b'x' | b'X' => 16,
                    b'u' => 10,
                    // Outer match guarantees specifier is one of x/X/u, but
                    // be defensive against future refactors.
                    _ => 10,
                };
                match length_modifier {
                    Some(lm) => {
                        match lm {
                            "h" => { /* same as below generally */ }
                            "hh" => {
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| u8::from_str_radix(s, base).unwrap_or(u8::MAX) as u16,
                                    |num| num.wrapping_neg(),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<u8> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val as u8);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            "l" => {
                                // On 32-bit ARM, unsigned long is 32-bit.
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| u32::from_str_radix(s, base).unwrap_or(u32::MAX),
                                    |num| num.wrapping_neg(),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<u32> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            "ll" => {
                                let res = str_to_int_inner_generic(
                                    env,
                                    &getc_fn,
                                    &ungetc_fn,
                                    subject,
                                    src_char_idx,
                                    base,
                                    if max_width > 0 { max_width } else { u32::MAX },
                                    |s, base| u64::from_str_radix(s, base).unwrap_or(u64::MAX),
                                    |num| num.wrapping_neg(),
                                );
                                match res {
                                    Ok((val, len)) => {
                                        src_char_idx += len;
                                        if !suppress_assignment {
                                            let ptr: ConstPtr<u64> = args.next(env);
                                            env.mem.write(ptr.cast_mut(), val);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            _ => {
                                log!("Warning: sscanf %x/%u: unhandled length modifier '{}' — skipping", lm);
                                break;
                            }
                        }
                    }
                    None => {
                        let res = str_to_int_inner_generic(
                            env,
                            &getc_fn,
                            &ungetc_fn,
                            subject,
                            src_char_idx,
                            base,
                            if max_width > 0 { max_width } else { u32::MAX },
                            |s, base| u32::from_str_radix(s, base).unwrap_or(u32::MAX),
                            |num| num.wrapping_neg(),
                        );
                        match res {
                            Ok((val, len)) => {
                                src_char_idx += len;
                                if !suppress_assignment {
                                    let c_u32_ptr: ConstPtr<u32> = args.next(env);
                                    env.mem.write(c_u32_ptr.cast_mut(), val);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            b'[' => {
                // `%[set]` conversion. The character set is parsed per the
                // C standard (ISO C / POSIX, mirrored by Apple's BSD
                // `vfscanf`): see
                // <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/scanf.3.html>.
                //
                // Two well-known special cases must NOT be treated as
                // terminators, otherwise valid format strings crash the
                // parser:
                //
                //   * A `]` that is the *first* character of the scanlist
                //     (i.e. `%[]...]` or `%[^]...]`) is a literal member of
                //     the set, not the closing bracket.
                //   * A `-` is a range operator only when it sits *between*
                //     two characters; a `-` that is the first or the last
                //     character of the scanlist is a literal `-`.
                //
                // We also stop at a NUL terminator so a malformed format with
                // no closing `]` can't read past the end of guest memory.
                //
                // Honour an optional max-width prefix (e.g. `%15[abc]`); 0
                // means "no limit". The matching loop below stops after
                // `max_width` chars so the destination buffer (sized for
                // max_width+1) is not overrun. Used by LEGO Ninjago:
                // Spinjitzu Scavenger Hunt's text loader
                // (e.g. `sscanf(line, "%15[^=]=%s", …)`) and BAROQUE.
                let inverted = if env.mem.read(format + format_char_idx) == b'^' {
                    format_char_idx += 1;
                    true
                } else {
                    false
                };

                // Build set, honouring the literal-`]`-first rule.
                let mut set: HashSet<u8> = HashSet::new();
                let mut first = true;
                loop {
                    let c = env.mem.read(format + format_char_idx);
                    // Closing bracket — unless it's the first scanlist char,
                    // in which case it's a literal member of the set.
                    if c == b']' && !first {
                        format_char_idx += 1;
                        break;
                    }
                    // Defensive: a missing closing `]` would otherwise spin
                    // forever / read out of bounds. Stop at NUL.
                    if c == b'\0' {
                        log!(
                            "Warning: sscanf %[…]: unterminated scanset \
                             (no closing ']'); stopping set parse."
                        );
                        break;
                    }
                    first = false;

                    // Is this a range like `a-z`? Only when a `-` follows the
                    // current char and is itself followed by a real range end
                    // (not `]` and not NUL). Otherwise `-` is a literal.
                    let next = env.mem.read(format + format_char_idx + 1);
                    if next == b'-' {
                        let range_end = env.mem.read(format + format_char_idx + 2);
                        if range_end != b']' && range_end != b'\0' {
                            // Valid range `c..=range_end`. Guard against a
                            // descending range (e.g. `z-a`), which is
                            // undefined in C: insert the endpoints and the
                            // dash literally rather than panicking on a
                            // reversed `RangeInclusive`.
                            if c <= range_end {
                                for x in c..=range_end {
                                    set.insert(x);
                                }
                            } else {
                                set.insert(c);
                                set.insert(b'-');
                                set.insert(range_end);
                            }
                            format_char_idx += 3;
                            continue;
                        }
                    }

                    // Plain literal character (covers a trailing `-`).
                    set.insert(c);
                    format_char_idx += 1;
                }

                let mut dst_ptr: Option<MutPtr<u8>> = if !suppress_assignment {
                    Some(args.next(env))
                } else {
                    None
                };

                let limit = if max_width > 0 { max_width } else { u32::MAX };
                let mut read_count: u32 = 0;
                let mut matched = false;
                loop {
                    if read_count >= limit {
                        break;
                    }
                    let x = getc_fn(env, subject, src_char_idx);
                    if x.is_err() {
                        break;
                    }
                    let cc: u8 = x.unwrap().into();
                    src_char_idx += 1;

                    if cc == b'\0' || !(set.contains(&cc) ^ inverted) {
                        ungetc_fn(env, subject, cc);
                        src_char_idx -= 1;
                        break;
                    }

                    matched = true;
                    read_count += 1;
                    if let Some(ptr) = dst_ptr {
                        env.mem.write(ptr, cc);
                        dst_ptr = Some(ptr + 1);
                    }
                }

                if matched {
                    if let Some(ptr) = dst_ptr {
                        env.mem.write(ptr, b'\0');
                    }
                } else {
                    if !suppress_assignment {
                        matched_args -= 1;
                    }
                }
            }
            b'c' => {
                let x = getc_fn(env, subject, src_char_idx);
                if x.is_err() {
                    break 'outer;
                }
                let cc: u8 = x.unwrap().into();
                src_char_idx += 1;
                if !suppress_assignment {
                    let ptr: MutPtr<u8> = args.next(env);
                    env.mem.write(ptr, cc);
                }
            }
            b'p' => {
                let res = str_to_int_inner_generic(
                    env,
                    &getc_fn,
                    &ungetc_fn,
                    subject,
                    src_char_idx,
                    16,
                    if max_width > 0 { max_width } else { u32::MAX },
                    |s, base| u32::from_str_radix(s, base).unwrap_or(0),
                    |num| num.wrapping_neg(),
                );
                match res {
                    Ok((val, len)) => {
                        src_char_idx += len;
                        if !suppress_assignment {
                            let ptr: MutPtr<MutVoidPtr> = args.next(env);
                            env.mem.write(ptr, crate::mem::Ptr::from_bits(val));
                        }
                    }
                    Err(_) => break,
                }
            }
            b'n' => {
                if !suppress_assignment {
                    let ptr: MutPtr<i32> = args.next(env);
                    env.mem.write(ptr, src_char_idx as i32);
                }
                continue;
            }
            b'%' => {
                let x = getc_fn(env, subject, src_char_idx);
                if x.is_err() {
                    break 'outer;
                }
                let cc: u8 = x.unwrap().into();
                if cc != b'%' {
                    return matched_args;
                }
                src_char_idx += 1;
                continue;
            }
            b's' => {
                // Honour an optional max-width prefix (e.g. "%127s"); 0
                // means "no limit". Stops after `max_width` chars when
                // one is given so the destination buffer (sized for
                // max_width+1) is not overrun.
                let mut dst_ptr: Option<MutPtr<u8>> = if !suppress_assignment {
                    Some(args.next(env))
                } else {
                    None
                };
                let orig_dst_ptr = dst_ptr;
                let limit = if max_width > 0 { max_width } else { u32::MAX };
                let mut read_count: u32 = 0;
                loop {
                    if read_count >= limit {
                        break;
                    }
                    let x = getc_fn(env, subject, src_char_idx);
                    if x.is_err() {
                        break;
                    }
                    let cc: u8 = x.unwrap().into();
                    if !isspace_inner(cc) {
                        if cc == b'\0' {
                            break;
                        }
                        if let Some(ptr) = dst_ptr {
                            env.mem.write(ptr, cc);
                            dst_ptr = Some(ptr + 1);
                        }
                        src_char_idx += 1;
                        read_count += 1;
                    } else {
                        ungetc_fn(env, subject, cc);
                        break;
                    }
                }

                if let Some(ptr) = dst_ptr {
                    env.mem.write(ptr, b'\0');
                    log_dbg!(
                        "sscanf_common_generic read %s '{:?}'",
                        env.mem.cstr_at_utf8(orig_dst_ptr.unwrap())
                    );
                }
            }
            _ => {
                log!(
                    "Warning: sscanf_common_generic: unhandled specifier '%{}' — stopping",
                    specifier as char
                );
                break;
            }
        } // БЫЛА ПРОПУЩЕНА ЭТА ЗАКРЫВАЮЩАЯ СКОБКА

        if !suppress_assignment {
            matched_args += 1;
        }
    }

    matched_args
}

fn sscanf(env: &mut Environment, src: ConstPtr<u8>, format: ConstPtr<u8>, args: DotDotDot) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "sscanf({:?} ({:?}), {:?} ({:?}), ...)",
        src,
        env.mem.cstr_at_utf8(src),
        format,
        env.mem.cstr_at_utf8(format)
    );
    sscanf_common(env, src, format, args.start())
}

fn swscanf(
    env: &mut Environment,
    ws: ConstPtr<wchar_t>,
    format: ConstPtr<wchar_t>,
    args: DotDotDot,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    // TODO: support other locales
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');

    let w_string = env.mem.wcstr_at(ws);
    let w_format = env.mem.wcstr_at(format);
    log_dbg!(
        "swscanf({:?} ({:?}), {:?} ({:?}), ...)",
        ws,
        w_string,
        format,
        w_format
    );
    // TODO: refactor code to parametrise sscanf_common()
    // for normal and wide strings instead
    let c_string = env.mem.alloc_and_write_cstr(w_string.as_bytes());
    let c_format = env.mem.alloc_and_write_cstr(w_format.as_bytes());
    let res = sscanf(env, c_string.cast_const(), c_format.cast_const(), args);
    env.mem.free(c_string.cast());
    env.mem.free(c_format.cast());
    res
}

fn vsscanf(env: &mut Environment, src: ConstPtr<u8>, format: ConstPtr<u8>, arg: VaList) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "vsscanf({:?}, {:?} ({:?}), ...)",
        src,
        format,
        env.mem.cstr_at_utf8(format)
    );
    sscanf_common(env, src, format, arg)
}

fn fscanf(
    env: &mut Environment,
    stream: MutPtr<FILE>,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "fscanf({:?}, {:?} ({:?}), ...)",
        stream,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let cc = getc(env, stream);
    if cc == EOF {
        return EOF;
    } else {
        assert_eq!(cc, ungetc(env, cc, stream));
    }

    sscanf_common_generic(
        env,
        |env, file, _idx| {
            let c = getc(env, file);
            if c == EOF {
                Err::<u8, ()>(())
            } else {
                Ok(<i32 as TryInto<u8>>::try_into(c).unwrap())
            }
        },
        |env, file, c| {
            assert_eq!(c as i32, ungetc(env, c as i32, file));
        },
        stream,
        format,
        args.start(),
    )
}

fn fprintf(
    env: &mut Environment,
    stream: MutPtr<FILE>,
    format: ConstPtr<u8>,
    args: DotDotDot,
) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!("fprintf() implemented as a wrapper of vfprintf()");

    vfprintf(env, stream, format, args.start())
}

fn vfprintf(env: &mut Environment, stream: MutPtr<FILE>, format: ConstPtr<u8>, arg: VaList) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);
    log_dbg!(
        "vfprintf({:?}, {:?} ({:?}), ...)",
        stream,
        format,
        env.mem.cstr_at_utf8(format)
    );
    let res = printf_inner::<false, _>(env, |mem, idx| mem.read(format + idx), arg);
    // TODO: I/O error handling
    match env.mem.read(stream).fd {
        STDIN_FILENO => {
            // vfprintf() to stdin is nonsense; real libc would EBADF. We
            // simply log and discard the bytes rather than crashing.
            log!(
                "Warning: vfprintf() called on STDIN_FILENO; discarding {} bytes.",
                res.len()
            );
        }
        STDOUT_FILENO => _ = crate::guest_console::stdout().write_all(&res),
        STDERR_FILENO => _ = crate::guest_console::stderr().write_all(&res),
        _ => {
            let buf = env.mem.alloc_and_write_cstr(res.as_slice());
            let result = fwrite(
                env,
                buf.cast_const().cast(),
                1,
                res.len() as GuestUSize,
                stream,
            );
            if result != res.len() as GuestUSize {
                log!(
                    "Warning: vfprintf(): fwrite wrote {} bytes but {} bytes were formatted.",
                    result,
                    res.len()
                );
            }
            env.mem.free(buf.cast());
        }
    }
    res.len().try_into().unwrap_or(i32::MAX)
}

fn vwprintf(env: &mut Environment, format: ConstPtr<wchar_t>, arg: VaList) -> i32 {
    // Очищаем errno перед выполнением
    set_errno(env, 0);
    // Используем 'C' локаль для корректной работы с широкими символами,
    // как это реализовано в vswprintf
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');

    let wcstr_format = env.mem.wcstr_at(format);
    log_dbg!("vwprintf({:?} ({:?}), ...)", format, wcstr_format);
    let wcstr_format_bytes = wcstr_format.as_bytes();
    let len: GuestUSize = wcstr_format_bytes.len() as GuestUSize;
    // Передаем байты формата в printf_inner
    let res = printf_inner::<false, _>(
        env,
        |_mem, idx| {
            if idx == len {
                b'\0'
            } else {
                wcstr_format_bytes[idx as usize]
            }
        },
        arg,
    );
    // Пишем результат напрямую в стандартный вывод (stdout)
    let _ = crate::guest_console::stdout().write_all(&res);
    res.len().try_into().unwrap()
}

// Added NSLog/NSLogv per your documentation and requests
fn NSLog(env: &mut Environment, format: id, args: DotDotDot) -> i32 {
    NSLogv(env, format, args.start())
}

fn NSLogv(env: &mut Environment, format: id, arg: VaList) -> i32 {
    if format != nil {
        let rust_str = ns_string::to_rust_string(env, format);
        let bytes = rust_str.as_bytes();
        let len = bytes.len() as GuestUSize;
        let res = printf_inner::<true, _>(
            env,
            |_mem, idx| {
                if idx < len {
                    bytes[idx as usize]
                } else {
                    b'\0'
                }
            },
            arg,
        );

        let msg_str = String::from_utf8_lossy(&res);
        // Already routed to the log above; writing it to the guest's stderr
        // as well would duplicate every line now that stream goes to the log.
        log!("NSLog: {}", msg_str);
    } else {
        log!("NSLog: (null format)");
    }
    0
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(sscanf(_, _, _)),
    export_c_func!(swscanf(_, _, _)),
    export_c_func!(vsscanf(_, _, _)),
    export_c_func!(fscanf(_, _, _)),
    export_c_func!(snprintf(_, _, _, _)),
    export_c_func!(vasprintf(_, _, _)),
    export_c_func!(vprintf(_, _)),
    export_c_func!(vsnprintf(_, _, _, _)),
    export_c_func!(__vsnprintf_chk(_, _, _, _, _, _)),
    export_c_func!(__snprintf_chk(_, _, _, _, _, _)),
    export_c_func!(vsprintf(_, _, _)),
    export_c_func!(__vsprintf_chk(_, _, _, _, _, _)),
    export_c_func!(__sprintf_chk(_, _, _, _, _)),
    export_c_func!(sprintf(_, _, _)),
    export_c_func!(sprintf_l(_, _, _, _)),
    export_c_func!(snprintf_l(_, _, _, _, _)),
    export_c_func!(vsprintf_l(_, _, _, _)),
    export_c_func!(vsnprintf_l(_, _, _, _, _)),
    export_c_func!(swprintf(_, _, _, _)),
    export_c_func!(vswprintf(_, _, _, _)),
    export_c_func!(wprintf(_, _)),
    export_c_func!(printf(_, _)),
    export_c_func!(fprintf(_, _, _)),
    export_c_func!(vfprintf(_, _, _)),
    export_c_func!(vwprintf(_, _)),
    // NSLog and NSLogv are exported from foundation::ns_log; not duplicated.
];

// Helper function, not a part of printf family
// TODO: write proper libc's isspace()
pub fn isspace(env: &mut Environment, src: ConstPtr<u8>) -> bool {
    let c = env.mem.read(src);
    isspace_inner(c)
}

pub fn isspace_inner(c: u8) -> bool {
    // Rust's definition of whitespace excludes vertical tab, unlike C's
    c.is_ascii_whitespace() || c == b'\x0b'
}
