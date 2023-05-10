#[cfg(feature = "adc")]
pub mod adc;

#[cfg(feature = "fan")]
pub mod fan;

#[cfg(feature = "hwmon")]
pub mod hwmon;

#[cfg(feature = "peci")]
pub mod peci;

#[cfg(feature = "external")]
pub mod external;
