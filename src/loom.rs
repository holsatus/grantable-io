//! Compatibility shim so the crate can be compiled against either the real
//! atomics or Loom's model checker.

#[allow(unused_imports)]
pub(crate) use self::inner::*;

/// Define a `const fn` that is only `const` when *not* compiling under Loom.
macro_rules! loom_const_fn {
    (
        $(#[$meta:meta])*
        $vis:vis const fn $name:ident($($args:tt)*) -> $ret:ty $body:block
    ) => {
        $(#[$meta])*
        #[cfg(not(loom))]
        $vis const fn $name($($args)*) -> $ret $body

        $(#[$meta])*
        #[cfg(loom)]
        $vis fn $name($($args)*) -> $ret $body
    };
}

#[allow(unused_imports)]
pub(crate) use loom_const_fn;

#[cfg(loom)]
mod inner {
    #[cfg(test)]
    pub(crate) use loom::{model, thread};

    pub(crate) mod sync {
        pub(crate) mod atomic {
            pub(crate) use loom::sync::atomic::*;
        }
    }
}

#[cfg(not(loom))]
mod inner {
    pub(crate) mod sync {
        pub(crate) mod atomic {
            pub(crate) use portable_atomic::{AtomicBool, AtomicUsize};
        }
    }
}
