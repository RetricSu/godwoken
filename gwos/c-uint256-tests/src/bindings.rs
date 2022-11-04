/* automatically generated by rust-bindgen 0.59.2 */

pub const true_: u32 = 1;
pub const false_: u32 = 0;
pub const INT8_MIN: i32 = -128;
pub const INT16_MIN: i32 = -32768;
pub const INT32_MIN: i32 = -2147483648;
pub const INT64_MIN: i64 = -9223372036854775808;
pub const INT8_MAX: u32 = 127;
pub const INT16_MAX: u32 = 32767;
pub const INT32_MAX: u32 = 2147483647;
pub const INT64_MAX: u64 = 9223372036854775807;
pub const UINT8_MAX: u32 = 255;
pub const UINT16_MAX: u32 = 65535;
pub const UINT32_MAX: u32 = 4294967295;
pub const UINT64_MAX: i32 = -1;
pub const SIZE_MAX: i32 = -1;
pub type size_t = ::std::os::raw::c_ulong;
pub type ssize_t = ::std::os::raw::c_long;
extern "C" {
    pub fn memset(
        dest: *mut ::std::os::raw::c_void,
        c: ::std::os::raw::c_int,
        n: ::std::os::raw::c_ulong,
    ) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn memcpy(
        dest: *mut ::std::os::raw::c_void,
        src: *const ::std::os::raw::c_void,
        n: ::std::os::raw::c_ulong,
    ) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn memcmp(
        vl: *const ::std::os::raw::c_void,
        vr: *const ::std::os::raw::c_void,
        n: ::std::os::raw::c_ulong,
    ) -> ::std::os::raw::c_int;
}
pub type WT = size_t;
extern "C" {
    pub fn memmove(
        dest: *mut ::std::os::raw::c_void,
        src: *const ::std::os::raw::c_void,
        n: ::std::os::raw::c_ulong,
    ) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn strcpy(
        d: *mut ::std::os::raw::c_char,
        s: *const ::std::os::raw::c_char,
    ) -> *mut ::std::os::raw::c_char;
}
extern "C" {
    pub fn strlen(s: *const ::std::os::raw::c_char) -> ::std::os::raw::c_ulong;
}
extern "C" {
    pub fn strcmp(
        l: *const ::std::os::raw::c_char,
        r: *const ::std::os::raw::c_char,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn malloc(size: ::std::os::raw::c_ulong) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn free(ptr: *mut ::std::os::raw::c_void);
}
extern "C" {
    pub fn calloc(
        nmemb: ::std::os::raw::c_ulong,
        size: ::std::os::raw::c_ulong,
    ) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn realloc(
        ptr: *mut ::std::os::raw::c_void,
        size: ::std::os::raw::c_ulong,
    ) -> *mut ::std::os::raw::c_void;
}
pub type cmpfun = ::std::option::Option<
    unsafe extern "C" fn(
        arg1: *const ::std::os::raw::c_void,
        arg2: *const ::std::os::raw::c_void,
    ) -> ::std::os::raw::c_int,
>;
extern "C" {
    pub fn qsort(base: *mut ::std::os::raw::c_void, nel: size_t, width: size_t, cmp: cmpfun);
}
extern "C" {
    pub fn bsearch(
        key: *const ::std::os::raw::c_void,
        base: *const ::std::os::raw::c_void,
        nel: size_t,
        width: size_t,
        cmp: ::std::option::Option<
            unsafe extern "C" fn(
                arg1: *const ::std::os::raw::c_void,
                arg2: *const ::std::os::raw::c_void,
            ) -> ::std::os::raw::c_int,
        >,
    ) -> *mut ::std::os::raw::c_void;
}
extern "C" {
    pub fn printf(format: *const ::std::os::raw::c_char, ...) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn _start();
}
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct uint256_t {
    pub array: [u32; 8usize],
}
#[test]
fn bindgen_test_layout_uint256_t() {
    assert_eq!(
        ::std::mem::size_of::<uint256_t>(),
        32usize,
        concat!("Size of: ", stringify!(uint256_t))
    );
    assert_eq!(
        ::std::mem::align_of::<uint256_t>(),
        4usize,
        concat!("Alignment of ", stringify!(uint256_t))
    );
    assert_eq!(
        unsafe { &(*(::std::ptr::null::<uint256_t>())).array as *const _ as usize },
        0usize,
        concat!(
            "Offset of field: ",
            stringify!(uint256_t),
            "::",
            stringify!(array)
        )
    );
}
extern "C" {
    pub fn gw_uint256_zero(num: *mut uint256_t);
}
extern "C" {
    pub fn gw_uint256_one(num: *mut uint256_t);
}
extern "C" {
    pub fn gw_uint256_max(num: *mut uint256_t);
}
extern "C" {
    pub fn gw_uint256_overflow_add(
        a: uint256_t,
        b: uint256_t,
        sum: *mut uint256_t,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn gw_uint256_underflow_sub(
        a: uint256_t,
        b: uint256_t,
        rem: *mut uint256_t,
    ) -> ::std::os::raw::c_int;
}
pub const GW_UINT256_SMALLER: ::std::os::raw::c_int = -1;
pub const GW_UINT256_EQUAL: ::std::os::raw::c_int = 0;
pub const GW_UINT256_LARGER: ::std::os::raw::c_int = 1;
pub type _bindgen_ty_1 = ::std::os::raw::c_int;
extern "C" {
    pub fn gw_uint256_cmp(a: uint256_t, b: uint256_t) -> ::std::os::raw::c_int;
}
