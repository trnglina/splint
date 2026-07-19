#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// Symbolic engine handles from SWI-Prolog.h. These are pointer-cast macros
// (`#define PL_ENGINE_MAIN ((PL_engine_t)0x1)` etc.) that bindgen does not
// translate, so they are defined by hand. They are sentinels, not valid
// pointers: never dereference them. `PL_set_engine` accepts PL_ENGINE_MAIN
// and PL_ENGINE_CURRENT but NOT PL_ENGINE_NONE (it would be dereferenced as
// an engine pointer); PL_ENGINE_NONE is only produced/consumed by
// `_PL_switch_engine`/`_PL_reset_engine`. Detaching via `PL_set_engine` uses
// a null pointer instead.
pub const PL_ENGINE_MAIN: PL_engine_t = 0x1 as PL_engine_t;
pub const PL_ENGINE_CURRENT: PL_engine_t = 0x2 as PL_engine_t;
pub const PL_ENGINE_NONE: PL_engine_t = 0x3 as PL_engine_t;
