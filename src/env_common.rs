#[cfg(feature = "mesalock_sgx")]
use std::prelude::v1::*;

// FIXME:
//use std::thread;
use std::time;
#[cfg(feature = "mesalock_sgx")]
use std::untrusted::time::SystemTimeEx;

pub fn micros() -> u64 {
    loop {
        let now = time::SystemTime::now().duration_since(time::UNIX_EPOCH);

        match now {
            Err(_) => continue,
            Ok(dur) => return dur.as_secs() * 1000000 + (dur.subsec_nanos() / 1000) as u64,
        }
    }
}

// FIXME::
pub fn sleep_for(micros: u32) {
    //thread::sleep(time::Duration::new(0, micros * 1000));
}
