//! Telling a pad that macOS already drives from one it has no driver for.
//!
//! macOS binds its own DriverKit driver to official Xbox controllers (and some others). The
//! IOUSBHostDevice of such a pad carries the registry property `UsbExclusiveOwner`, which names the
//! driver class that holds it (for a Microsoft Xbox Series pad: `XboxUSBDevice`). While it is held,
//! the descriptors can still be read, but resetting the device or claiming its interface fails with
//! `kIOReturnExclusiveAccess` (0xe00002c5). User-space programs that hold a device show up in the
//! same property as `pid <n>, <name>`, so the two cases can be told apart.
//!
//! The decisions are plain functions of the property and the error, so they can be tested without
//! hardware; only [`exclusive_owner`] talks to IOKit.

use std::fmt;

use nusb::{DeviceInfo, ErrorKind};

/// `kIOReturnExclusiveAccess`, the IOReturn macOS gives when the device or interface is already
/// open exclusively elsewhere.
pub const IO_RETURN_EXCLUSIVE_ACCESS: u32 = 0xE000_02C5;

/// Who holds a device open exclusively, from the `UsbExclusiveOwner` registry property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A driver class of the system, such as `XboxUSBDevice`.
    Driver(String),
    /// A user-space process, as macOS writes it: `pid 123, name`.
    Process(String),
}

impl Owner {
    /// Reads the property's text; an absent or empty property means nobody holds the device.
    pub fn parse(property: Option<&str>) -> Option<Self> {
        let text = property?.trim();
        if text.is_empty() {
            None
        } else if text.starts_with("pid ") {
            Some(Self::Process(text.to_string()))
        } else {
            Some(Self::Driver(text.to_string()))
        }
    }
}

/// Why opening or claiming a pad failed, as far as it can be told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// A macOS driver holds the pad; the name is the driver class.
    SystemDriver(String),
    /// A user-space process holds it (`pid 123, name`).
    Process(String),
    /// The pad is open exclusively somewhere, but nothing names the holder: a driver or a program.
    ExclusiveUnknown,
    /// Something else; no hint applies.
    Other,
}

/// Whether an error is macOS saying the device or interface is already open exclusively.
pub fn is_exclusive_access(kind: ErrorKind, os_error: Option<u32>) -> bool {
    kind == ErrorKind::Busy || os_error == Some(IO_RETURN_EXCLUSIVE_ACCESS)
}

/// Decides what is in the way, from the device's owner (if it has one) and the error.
///
/// A named system driver is reported whatever the error was: that is what holds the pad. A bare
/// exclusive-access error, with no owner to name, could equally be a driver or another program, so it
/// is not blamed on either.
pub fn blocker(owner: Option<&Owner>, kind: ErrorKind, os_error: Option<u32>) -> Blocker {
    match owner {
        Some(Owner::Driver(name)) => Blocker::SystemDriver(name.clone()),
        Some(Owner::Process(who)) => Blocker::Process(who.clone()),
        None if is_exclusive_access(kind, os_error) => Blocker::ExclusiveUnknown,
        None => Blocker::Other,
    }
}

impl Blocker {
    /// The parenthesised hint to add to an open/claim/reset error, or empty for none. A system
    /// driver gets no hint here: it is reported as [`SystemOwned`] instead.
    pub fn hint(&self) -> String {
        match self {
            Self::SystemDriver(_) | Self::Other => String::new(),
            Self::Process(who) => format!(" (held by {who})"),
            Self::ExclusiveUnknown => {
                " (open exclusively elsewhere: is another program, e.g. Chrome, holding it?)".into()
            }
        }
    }
}

/// A name for messages from the USB strings: "Microsoft Controller" rather than just "Controller".
pub fn display_name(manufacturer: Option<&str>, product: Option<&str>) -> Option<String> {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    match (clean(manufacturer), clean(product)) {
        (Some(m), Some(p)) if p.to_lowercase().contains(&m.to_lowercase()) => Some(p.to_string()),
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (None, Some(p)) => Some(p.to_string()),
        (Some(m), None) => Some(m.to_string()),
        (None, None) => None,
    }
}

/// A GIP pad that a macOS driver already holds, so gip-pad has nothing to do for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemOwned {
    pub vendor: u16,
    pub product: u16,
    /// The USB product string, if it has one
    pub name: Option<String>,
    /// The owning driver class from `UsbExclusiveOwner`
    pub owner: String,
}

impl fmt::Display for SystemOwned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let who = if self.owner.to_ascii_lowercase().contains("xbox") {
            format!("its own Xbox controller driver ({})", self.owner)
        } else {
            format!("a driver of its own ({})", self.owner)
        };
        write!(
            f,
            "found {} ({:04x}:{:04x}), but macOS already drives it with {who}",
            self.name.as_deref().unwrap_or("a controller"),
            self.vendor,
            self.product,
        )?;
        if self.owner.to_ascii_lowercase().contains("xbox") {
            write!(
                f,
                ": games see it through Game Controller, so gip-pad is not needed for it."
            )?;
        } else {
            write!(
                f,
                ", which holds it open exclusively, so gip-pad cannot use it and is not needed for it."
            )?;
        }
        write!(f, " gip-pad is for wired pads macOS has no driver for.")
    }
}

impl std::error::Error for SystemOwned {}

/// The device's `UsbExclusiveOwner` registry property, read from the IORegistry; `None` if it has
/// none (nobody holds the device) or the registry entry cannot be read.
#[cfg(target_os = "macos")]
pub fn exclusive_owner(info: &DeviceInfo) -> Option<Owner> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::string::CFString;
    use core_foundation_sys::base::kCFAllocatorDefault;
    use core_foundation_sys::dictionary::CFDictionaryRef;
    use io_kit_sys::{
        kIOMasterPortDefault, IOObjectRelease, IORegistryEntryCreateCFProperty,
        IORegistryEntryIDMatching, IOServiceGetMatchingService,
    };

    // SAFETY: plain IOKit calls. The matching dictionary is created here and consumed by
    // IOServiceGetMatchingService (which releases it); the service and the property are owned
    // references that are released or wrapped (create rule) so they are freed exactly once.
    let text = unsafe {
        let matching = IORegistryEntryIDMatching(info.registry_entry_id());
        if matching.is_null() {
            return None;
        }
        let service =
            IOServiceGetMatchingService(kIOMasterPortDefault, matching as CFDictionaryRef);
        if service == 0 {
            return None;
        }
        let key = CFString::from_static_string("UsbExclusiveOwner");
        let property = IORegistryEntryCreateCFProperty(
            service,
            key.as_concrete_TypeRef(),
            kCFAllocatorDefault,
            0,
        );
        IOObjectRelease(service);
        if property.is_null() {
            return None;
        }
        CFType::wrap_under_create_rule(property)
            .downcast_into::<CFString>()
            .map(|s| s.to_string())
    };
    Owner::parse(text.as_deref())
}

/// Other platforms have no such registry property.
#[cfg(not(target_os = "macos"))]
pub fn exclusive_owner(_info: &DeviceInfo) -> Option<Owner> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(owner: &str) -> SystemOwned {
        SystemOwned {
            vendor: 0x045e,
            product: 0x0b12,
            name: Some("Microsoft Controller".into()),
            owner: owner.into(),
        }
    }

    #[test]
    fn owner_property_is_classified() {
        assert_eq!(Owner::parse(None), None);
        assert_eq!(Owner::parse(Some("")), None);
        assert_eq!(Owner::parse(Some("  ")), None);
        assert_eq!(
            Owner::parse(Some("XboxUSBDevice")),
            Some(Owner::Driver("XboxUSBDevice".into()))
        );
        assert_eq!(
            Owner::parse(Some("pid 73617, usbaudiod")),
            Some(Owner::Process("pid 73617, usbaudiod".into()))
        );
    }

    #[test]
    fn exclusive_access_is_recognised_by_code_or_kind() {
        assert!(is_exclusive_access(ErrorKind::Other, Some(0xe00002c5)));
        assert!(is_exclusive_access(ErrorKind::Busy, Some(0xe00002c5)));
        assert!(is_exclusive_access(ErrorKind::Busy, None));
        assert!(!is_exclusive_access(ErrorKind::Other, Some(0xe00002c0)));
        assert!(!is_exclusive_access(ErrorKind::Disconnected, None));
        assert_eq!(IO_RETURN_EXCLUSIVE_ACCESS, 0xe00002c5);
    }

    #[test]
    fn blocker_follows_the_owner_first_then_the_error() {
        let driver = Owner::Driver("XboxUSBDevice".into());
        let chrome = Owner::Process("pid 42, Google Chrome Helper".into());
        // A system driver wins whatever the error was, so Chrome's hint is never shown for it
        for (kind, code) in [
            (ErrorKind::Busy, Some(0xe00002c5)),
            (ErrorKind::Other, Some(0xe00002c5)),
            (ErrorKind::Other, None),
        ] {
            assert_eq!(
                blocker(Some(&driver), kind, code),
                Blocker::SystemDriver("XboxUSBDevice".into())
            );
        }
        assert_eq!(
            blocker(Some(&chrome), ErrorKind::Busy, None),
            Blocker::Process("pid 42, Google Chrome Helper".into())
        );
        // No owner named: an exclusive-access error is not blamed on anyone
        assert_eq!(
            blocker(None, ErrorKind::Busy, Some(0xe00002c5)),
            Blocker::ExclusiveUnknown
        );
        assert_eq!(
            blocker(None, ErrorKind::Other, Some(0xe00002c5)),
            Blocker::ExclusiveUnknown
        );
        assert_eq!(blocker(None, ErrorKind::Other, None), Blocker::Other);
        assert_eq!(
            blocker(None, ErrorKind::Disconnected, Some(0xe00002ed)),
            Blocker::Other
        );
    }

    #[test]
    fn hints_only_when_the_holder_is_not_a_system_driver() {
        assert_eq!(Blocker::SystemDriver("XboxUSBDevice".into()).hint(), "");
        assert_eq!(Blocker::Other.hint(), "");
        assert!(Blocker::ExclusiveUnknown.hint().contains("Chrome"));
        assert_eq!(
            Blocker::Process("pid 42, Chrome".into()).hint(),
            " (held by pid 42, Chrome)"
        );
    }

    #[test]
    fn names_combine_manufacturer_and_product() {
        assert_eq!(
            display_name(Some("Microsoft"), Some("Controller")).as_deref(),
            Some("Microsoft Controller")
        );
        assert_eq!(
            display_name(Some("PDP"), Some("PDP Afterglow Wave")).as_deref(),
            Some("PDP Afterglow Wave")
        );
        assert_eq!(display_name(None, Some("Pad")).as_deref(), Some("Pad"));
        assert_eq!(display_name(Some(" "), Some("Pad")).as_deref(), Some("Pad"));
        assert_eq!(display_name(Some("Acme"), None).as_deref(), Some("Acme"));
        assert_eq!(display_name(None, None), None);
    }

    #[test]
    fn xbox_driver_message() {
        assert_eq!(
            owned("XboxUSBDevice").to_string(),
            "found Microsoft Controller (045e:0b12), but macOS already drives it with its own \
             Xbox controller driver (XboxUSBDevice): games see it through Game Controller, so \
             gip-pad is not needed for it. gip-pad is for wired pads macOS has no driver for."
        );
    }

    #[test]
    fn other_driver_message_makes_no_claim_about_games() {
        let text = owned("SomeVendorDriver").to_string();
        assert!(text.contains("a driver of its own (SomeVendorDriver)"));
        assert!(text.contains("holds it open exclusively"));
        assert!(!text.contains("Game Controller"));
        let nameless = SystemOwned {
            name: None,
            ..owned("XboxUSBDevice")
        };
        assert!(nameless
            .to_string()
            .starts_with("found a controller (045e:0b12), but"));
    }
}
