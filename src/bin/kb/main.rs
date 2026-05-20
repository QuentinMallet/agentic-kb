//! Main entry point for kb

#![deny(warnings, trivial_casts, unused_qualifications)]
#![forbid(unsafe_code)]

use kb::application::APP;

/// Boot kb
fn main() {
    abscissa_core::boot(&APP);
}
