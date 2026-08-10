//! Stand up an autonomous Wi-Fi Direct group owner on Windows and hang our own WFD
//! information element off it.
//!
//! This is the spike named in `docs/miracast-protocol-notes.md` §7.7(c). The two
//! supported sink APIs are both dead ends — `WFDStartDisplaySink` ended client support
//! at Windows 10, and `Windows.Media.Miracast.MiracastReceiver` wants a
//! `CoreApplicationView` that desktop apps do not have. Path (c) says we can skip both,
//! build the group ourselves through `WiFiDirectAdvertisementPublisher`, and run
//! *castaway's own* RTSP/RTP sink over it — the same architecture as the Linux backend.
//!
//! The bytes come from [`WfdInformationElement`] — the same builder `backend_linux`
//! feeds to `WFD_SUBELEM_SET` — so a result here is evidence about *our* IE, not a
//! hand-rolled stand-in.
//!
//! # What it found — path (c) is dead
//!
//! Run against the AX211 deploy box on 2026-08-01. The good half: an **unpackaged**
//! Win32 process (`GetCurrentPackageFullName` → 15700), with the machine's Location
//! consent set to `Deny` and no manifest capability, stands up an autonomous group
//! owner and reaches `Started`. Windows even runs DHCP on the group itself, handing the
//! virtual adapter 192.168.137.1/24, and the station interface stays associated
//! throughout — STA + P2P GO concurrency is real on this radio.
//!
//! The fatal half is the IE. Varying one field at a time:
//!
//! | OUI | type | `Start()` |
//! |---|---|---|
//! | *(no IE)* | — | `Started` |
//! | `02:00:00` | 0x01, 0x0a | `Started`, read back byte-intact |
//! | `50:6f:9a` (WFA) | 0x09, 0x0a, 0x0b | **`Aborted`** |
//! | `00:50:f2` (Microsoft/WSC) | 0x04 | **`Aborted`** |
//!
//! So the gate is the **OUI, not the OUI type**, and the abort carries
//! `WiFiDirectError::Success` — no reason given. Windows reserves the WFA and Microsoft
//! OUIs to its own P2P/WPS stack. A Miracast sink is *defined* by beaconing the WFD IE
//! under `50:6F:9A` type `0x0A`, so we can build the group and inject IEs, just never
//! the one IE that matters. `--no-go` aborts identically, so group ownership is not the
//! trigger.
//!
//! Two things this did **not** establish: whether package identity (MSIX) lifts the
//! restriction — untested, and the difference between a capability check and a blanket
//! reservation; and whether an accepted IE truly reaches the air, since the alt-OUI
//! group never showed a `DIRECT-` SSID in a scan from a second machine. Both are moot
//! for the WFD OUI, which is refused before it can be transmitted at all.
//!
//! See issue #17 and `docs/miracast-protocol-notes.md` §7.7.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    imp::run()
}

#[cfg(windows)]
mod imp {
    use proto_miracast::ie::{DeviceInformation, ExtendedCapability, WfdInformationElement};
    use std::time::Duration;
    use windows::Devices::WiFiDirect::{
        WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatus,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs, WiFiDirectError,
        WiFiDirectInformationElement,
    };
    use windows::Foundation::TypedEventHandler;
    use windows::Security::Cryptography::CryptographicBuffer;

    /// The RTSP control port every WFD sink listens on.
    const CONTROL_PORT: u16 = 7236;
    /// Advertised ceiling, in Mbps. Informational — no source is known to police it.
    const MAX_THROUGHPUT_MBPS: u16 = 200;

    /// Which knob a run is testing. `Start()` can abort with `WiFiDirectError::Success`,
    /// so the only way to learn what Windows objects to is to vary one thing at a time.
    struct Options {
        hold: u64,
        /// Attach a vendor IE at all.
        ie: bool,
        /// The OUI to attach it under. The WFD OUI may be reserved to the system;
        /// a locally-administered OUI distinguishes "no IEs allowed" from "not that IE".
        oui: [u8; 3],
        oui_type: u8,
        /// Ask to be the group owner. Autonomous GO is what a sink needs, but it is
        /// also the more privileged request.
        autonomous_go: bool,
    }

    impl Options {
        fn from_args() -> Self {
            let args: Vec<String> = std::env::args().skip(1).collect();
            let has = |f: &str| args.iter().any(|a| a == f);
            Self {
                hold: args
                    .iter()
                    .find_map(|a| a.strip_prefix("--hold="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60),
                ie: !has("--no-ie"),
                oui: args
                    .iter()
                    .find_map(|a| a.strip_prefix("--oui="))
                    .and_then(parse_oui)
                    // Locally administered, unassigned: nothing can call it reserved.
                    .unwrap_or(if has("--alt-oui") {
                        [0x02, 0x00, 0x00]
                    } else {
                        [0x50, 0x6F, 0x9A]
                    }),
                oui_type: args
                    .iter()
                    .find_map(|a| a.strip_prefix("--type="))
                    .and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                    .unwrap_or(if has("--alt-oui") { 0x01 } else { 0x0A }),
                autonomous_go: !has("--no-go"),
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let opts = Options::from_args();

        // Exactly what backend_linux advertises: primary sink, session available, our
        // control port, and the UIBC bit so a source bothers to negotiate touch.
        let ie = WfdInformationElement::sink(
            DeviceInformation::sink(CONTROL_PORT, MAX_THROUGHPUT_MBPS),
            ExtendedCapability {
                uibc: true,
                ..ExtendedCapability::default()
            },
        );
        let subelements = ie.to_subelements();
        println!(
            "CONFIG ie={} oui={} type={:#04x} autonomous_go={}",
            opts.ie,
            hex(&opts.oui),
            opts.oui_type,
            opts.autonomous_go
        );
        println!("wfd subelements = {}", hex(&subelements));

        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let advertisement = publisher.Advertisement()?;
        advertisement.SetIsAutonomousGroupOwnerEnabled(opts.autonomous_go)?;
        advertisement.SetListenStateDiscoverability(
            WiFiDirectAdvertisementListenStateDiscoverability::Normal,
        )?;

        if opts.ie {
            // The question. `Value` carries the subelements only: Windows prepends the
            // 0xDD element id, the length, and the OUI + OUI type from the fields beside it.
            let element = WiFiDirectInformationElement::new()?;
            element.SetOui(&CryptographicBuffer::CreateFromByteArray(&opts.oui)?)?;
            element.SetOuiType(opts.oui_type)?;
            element.SetValue(&CryptographicBuffer::CreateFromByteArray(&subelements)?)?;

            match advertisement.InformationElements()?.Append(&element) {
                Ok(()) => println!("OK    appended IE"),
                Err(e) => {
                    println!("FAIL  appending IE: {e}  hresult={:#010x}", e.code().0);
                    return Err(e.into());
                }
            }
        }

        publisher.StatusChanged(&TypedEventHandler::<
            WiFiDirectAdvertisementPublisher,
            WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
        >::new(|_, args| {
            if let Some(args) = args.as_ref() {
                let status = args.Status().map(status_name).unwrap_or("?");
                let error = match args.Error() {
                    Ok(WiFiDirectError::Success) => "Success",
                    Ok(WiFiDirectError::RadioNotAvailable) => "RadioNotAvailable",
                    Ok(WiFiDirectError::ResourceInUse) => "ResourceInUse",
                    _ => "?",
                };
                println!("EVENT status={status} error={error}");
            }
            Ok(())
        }))?;

        println!(">>>   Start()");
        if let Err(e) = publisher.Start() {
            println!("FAIL  Start(): {e}  hresult={:#010x}", e.code().0);
            return Err(e.into());
        }
        std::thread::sleep(Duration::from_secs(3));

        let status = publisher.Status()?;
        println!("      status = {} ({status:?})", status_name(status));
        if status != WiFiDirectAdvertisementPublisherStatus::Started {
            println!("FAIL  publisher never reached Started");
            let _ = publisher.Stop();
            return Err("publisher did not start".into());
        }

        // Read the vector back — the API may have normalised or dropped what we set.
        for (i, e) in advertisement.InformationElements()?.into_iter().enumerate() {
            let mut oui = windows::core::Array::<u8>::new();
            let mut value = windows::core::Array::<u8>::new();
            CryptographicBuffer::CopyToByteArray(&e.Oui()?, &mut oui)?;
            CryptographicBuffer::CopyToByteArray(&e.Value()?, &mut value)?;
            println!(
                "      readback[{i}] oui={} type={:#04x} value={}",
                hex(&oui),
                e.OuiType()?,
                hex(&value)
            );
        }

        println!(
            "GOUP  advertising for {}s — scan for OUI {} type {:#04x} now",
            opts.hold,
            hex(&opts.oui),
            opts.oui_type
        );
        std::thread::sleep(Duration::from_secs(opts.hold));

        publisher.Stop()?;
        println!(
            "      stopped; status = {}",
            status_name(publisher.Status()?)
        );
        Ok(())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `--oui=506f9a` — six hex digits, no separators.
    fn parse_oui(text: &str) -> Option<[u8; 3]> {
        if text.len() != 6 {
            return None;
        }
        let mut oui = [0u8; 3];
        for (i, byte) in oui.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(oui)
    }

    /// The projection derives no `Debug` names for these, and "3" is not a diagnosis.
    fn status_name(status: WiFiDirectAdvertisementPublisherStatus) -> &'static str {
        match status {
            WiFiDirectAdvertisementPublisherStatus::Created => "Created",
            WiFiDirectAdvertisementPublisherStatus::Started => "Started",
            WiFiDirectAdvertisementPublisherStatus::Stopped => "Stopped",
            WiFiDirectAdvertisementPublisherStatus::Aborted => "Aborted",
            other => {
                let _ = other;
                "unknown"
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        Err("wfd-probe only does anything on Windows; build it with `nix develop .#windows`".into())
    }
}
