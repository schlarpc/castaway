//! User Directed Commissioning — the half of Matter Casting that is not in `rs-matter`.
//!
//! UDC is how a phone asks a TV to commission it. It is the *only* Matter exchange that
//! runs with no session, no encryption and no acknowledgement: five identical datagrams
//! to UDP 5550, 100 ms apart, and whatever the receiver does about it is a matter for the
//! person standing in front of the screen. Core spec §5.3.
//!
//! Two messages, and they share an opcode — direction is what tells them apart:
//!
//! - [`IdentificationDeclaration`], client → player: "I am `<instance name>`, I am called
//!   `<device name>`, and these are the apps I want to cast into."
//! - [`CommissionerDeclaration`], player → client: "I have put a passcode on the screen"
//!   (or "no such app", or "that failed").
//!
//! Both are sans-I/O here (ground rule 3): bytes in, typed values out, no socket. The
//! actor that owns the socket is [`crate::server`].
//!
//! ## Where this came from
//!
//! `connectedhomeip`'s `src/protocols/user_directed_commissioning/` — the TLV tag numbers
//! carry a `// TODO: update spec per the latest tags` comment in that header, so the
//! *implementation* is the specification here and the spec text is the secondary source.
//! Read as a wire-behaviour spec, not vendored (ground rule 9).

use rs_matter::tlv::{TLVElement, TLVTag, TLVWrite};
use rs_matter::utils::storage::WriteBuf;

use crate::error::UdcError;

/// The UDP port a Casting Player listens on for `IdentificationDeclaration`s, and the
/// default port a client listens on for the reply (`CHIP_UDC_PORT`).
pub const UDC_PORT: u16 = 5550;

/// Matter protocol id for User Directed Commissioning.
const PROTOCOL_ID: u16 = 0x0009;

/// The one opcode. Both messages use it; the direction disambiguates, which is why this
/// module has no `MsgType` enum — there is nothing to choose.
const OPCODE_DECLARATION: u8 = 0x00;

/// Unencrypted Matter message header: flags, session id, security flags, counter.
const PLAIN_HEADER_LEN: usize = 8;

/// Exchange flags, opcode, exchange id, protocol id.
const PROTO_HEADER_LEN: usize = 6;

/// `Dnssd::Commission::kInstanceNameMaxLength` — 16 hex chars of a 64-bit value.
const INSTANCE_NAME_MAX: usize = 16;

/// The `IdentificationDeclaration` payload opens with a *fixed-length* NUL-padded block,
/// not a TLV field: `char mInstanceName[kInstanceNameMaxLength + 1]`. The TLV body starts
/// at exactly this offset, whatever the name's real length.
const INSTANCE_NAME_BLOCK: usize = INSTANCE_NAME_MAX + 1;

/// `IdentificationDeclaration::kUdcTLVDataMaxBytes`.
const TLV_MAX: usize = 500;

/// `Dnssd::kMaxDeviceNameLen`.
const DEVICE_NAME_MAX: usize = 32;

/// `Dnssd::kMaxPairingInstructionLen`.
const PAIRING_INSTRUCTION_MAX: usize = 128;

/// `Dnssd::kMaxRotatingIdLen`.
const ROTATING_ID_MAX: usize = 50;

/// `IdentificationDeclaration::kMaxTargetAppInfos`.
const TARGET_APPS_MAX: usize = 10;

/// TLV context tags of an `IdentificationDeclaration`.
///
/// The whole table, including the tags neither side reads: it is the reference's own
/// enum, and a gap in it would read as a tag that does not exist rather than as one we
/// chose to ignore.
#[allow(dead_code)]
mod id_tag {
    pub const VENDOR_ID: u8 = 1;
    pub const PRODUCT_ID: u8 = 2;
    pub const DEVICE_NAME: u8 = 3;
    pub const DEVICE_TYPE: u8 = 4;
    pub const PAIRING_INSTRUCTION: u8 = 5;
    pub const PAIRING_HINT: u8 = 6;
    pub const ROTATING_ID: u8 = 7;
    pub const CD_PORT: u8 = 8;
    pub const TARGET_APP_LIST: u8 = 9;
    pub const TARGET_APP: u8 = 10;
    pub const APP_VENDOR_ID: u8 = 11;
    pub const APP_PRODUCT_ID: u8 = 12;
    pub const NO_PASSCODE: u8 = 13;
    pub const CD_UPON_PASSCODE_DIALOG: u8 = 14;
    pub const COMMISSIONER_PASSCODE: u8 = 15;
    pub const COMMISSIONER_PASSCODE_READY: u8 = 16;
    pub const CANCEL_PASSCODE: u8 = 17;
    pub const PASSCODE_LENGTH: u8 = 18;
}

/// TLV context tags of a `CommissionerDeclaration`.
mod cd_tag {
    pub const ERROR_CODE: u8 = 1;
    pub const NEEDS_PASSCODE: u8 = 2;
    pub const NO_APPS_FOUND: u8 = 3;
    pub const PASSCODE_DIALOG_DISPLAYED: u8 = 4;
    pub const COMMISSIONER_PASSCODE: u8 = 5;
    pub const QR_CODE_DISPLAYED: u8 = 6;
    pub const CANCEL_PASSCODE: u8 = 7;
    pub const PASSCODE_LENGTH: u8 = 8;
}

/// A commissionable node's DNS-SD instance name: 16 hex characters.
///
/// A newtype because it is the join key for the entire flow — the client puts it in its
/// UDC message *and* in the `_matterc._udp` instance it starts advertising, and the two
/// have to be compared to find the node we were asked to commission. A `String` that is
/// sometimes a name and sometimes a friendly label would make that comparison a guess.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceName(String);

impl InstanceName {
    /// Parse an instance name.
    ///
    /// # Errors
    /// [`UdcError::InstanceName`] if empty or longer than 16 characters. The character
    /// set is deliberately *not* checked: the spec says uppercase hex, senders in the
    /// wild have been observed lowercase, and a case mismatch that rejects the message
    /// outright would be a worse failure than one that fails to match an mDNS record.
    pub fn new(raw: impl Into<String>) -> Result<Self, UdcError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(UdcError::InstanceName("empty"));
        }
        if raw.len() > INSTANCE_NAME_MAX {
            return Err(UdcError::InstanceName("longer than 16 characters"));
        }
        Ok(Self(raw))
    }

    /// The name as it appears on the wire and in the mDNS instance label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstanceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry of a client's `targetAppList`: an app it intends to cast into.
///
/// The player answers with `noAppsFound` when it hosts none of them, which is what makes
/// a phone say "this TV can't play that" instead of hanging on a passcode prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetApp {
    /// CSA vendor id of the content app.
    pub vendor_id: u16,
    /// Product id, or 0 for "any product of this vendor".
    pub product_id: u16,
}

/// What the client is actually asking for.
///
/// Three booleans on the wire that are really one three-way choice: the reference server
/// tests them in this order and dispatches to three unrelated handlers. Deriving the
/// choice once, at parse time, is what stops a caller from reading a cancellation as a
/// commissioning request because it checked the flags in a different order (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdcRequest {
    /// The user backed out on the phone. Drop any pending prompt for this instance.
    Cancel,
    /// The user has typed the passcode we displayed, and the client is now advertising
    /// itself as commissionable with a verifier derived from it. Commission it.
    PasscodeReady,
    /// A fresh request: put a passcode on the screen.
    Commission,
}

/// The client's half of the exchange, sent to UDP 5550.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentificationDeclaration {
    /// The commissionable instance name the client is (or will be) advertising.
    pub instance_name: InstanceName,
    /// The client's CSA vendor id, if it said.
    pub vendor_id: Option<u16>,
    /// The client's product id, if it said.
    pub product_id: Option<u16>,
    /// A human-readable name for the *phone*, for the prompt: "Chaz's iPhone wants to cast".
    pub device_name: Option<String>,
    /// Where to send the [`CommissionerDeclaration`]. Absent or 0 means the client does
    /// not want one.
    pub cd_port: Option<u16>,
    /// Pairing hint bitmap, echoed from the client's own commissionable advertisement.
    pub pairing_hint: Option<u16>,
    /// Free text the client wants shown alongside the passcode prompt.
    pub pairing_instruction: Option<String>,
    /// The client's rotating device id, used by the Account Login passcode flow to let a
    /// content app's cloud recognise a returning user.
    pub rotating_id: Option<Vec<u8>>,
    /// Apps the client wants to cast into. Empty means "anything you have".
    pub target_apps: Vec<TargetApp>,
    /// The client cannot show a passcode entry field; commission it without one.
    pub no_passcode: bool,
    /// Send a `CommissionerDeclaration` as soon as the passcode dialog is up.
    pub cd_upon_passcode_dialog: bool,
    /// The client wants *us* to generate and display the passcode (the flow Amazon's
    /// senders use) rather than showing its own.
    pub commissioner_passcode: bool,
    /// The user has entered the passcode we displayed.
    pub commissioner_passcode_ready: bool,
    /// The user dismissed the prompt on the phone.
    pub cancel_passcode: bool,
    /// How many digits the client expects the passcode to have.
    pub passcode_length: Option<u8>,
}

impl IdentificationDeclaration {
    /// What this message is asking for, resolved from the flags once.
    #[must_use]
    pub fn request(&self) -> UdcRequest {
        // Order matters and is the reference server's: cancel wins over ready, ready wins
        // over a fresh request. A client that sets both cancel and ready has changed its
        // mind twice and the later intent is to stop.
        if self.cancel_passcode {
            UdcRequest::Cancel
        } else if self.commissioner_passcode_ready {
            UdcRequest::PasscodeReady
        } else {
            UdcRequest::Commission
        }
    }

    /// Whether the client wants a [`CommissionerDeclaration`] back, and where to send it.
    ///
    /// A port of 0 is how the reference client says "don't bother" — it is the default
    /// value of the field, so an older sender that never sets it reads as declining.
    #[must_use]
    pub fn reply_port(&self) -> Option<u16> {
        self.cd_port.filter(|p| *p != 0)
    }

    /// Parse a whole datagram: Matter framing, the fixed instance-name block, then TLV.
    ///
    /// # Errors
    /// [`UdcError`] for a short, encrypted, misdirected or malformed datagram.
    pub fn decode(datagram: &[u8]) -> Result<Self, UdcError> {
        let payload = decode_frame(datagram)?;

        if payload.len() < INSTANCE_NAME_BLOCK {
            return Err(UdcError::Truncated {
                got: datagram.len(),
                need: PLAIN_HEADER_LEN + PROTO_HEADER_LEN + INSTANCE_NAME_BLOCK,
            });
        }

        let (name_block, tlv) = payload.split_at(INSTANCE_NAME_BLOCK);
        let name_len = name_block
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(INSTANCE_NAME_MAX);
        let instance_name = std::str::from_utf8(&name_block[..name_len])
            .map_err(|_| UdcError::InstanceName("not UTF-8"))?;
        let instance_name = InstanceName::new(instance_name)?;

        let mut id = Self {
            instance_name,
            vendor_id: None,
            product_id: None,
            device_name: None,
            cd_port: None,
            pairing_hint: None,
            pairing_instruction: None,
            rotating_id: None,
            target_apps: Vec::new(),
            no_passcode: false,
            cd_upon_passcode_dialog: false,
            commissioner_passcode: false,
            commissioner_passcode_ready: false,
            cancel_passcode: false,
            passcode_length: None,
        };

        // A declaration with no TLV at all is legal — the reference reader returns early
        // on a payload that is exactly the name block. It means "commission me, I have
        // nothing else to say".
        if tlv.is_empty() {
            return Ok(id);
        }

        let root = TLVElement::new(tlv);
        let fields = root
            .structure()
            .map_err(|_| UdcError::Tlv("not a struct"))?;

        for field in fields.iter() {
            let field = field.map_err(|_| UdcError::Tlv("truncated element"))?;
            // Unknown *context* tags are skipped rather than rejected: the tag list has
            // grown twice already (`commissionerPasscode` in 1.3, `passcodeLength` after
            // it) and a newer phone must not be unable to cast to an older panel.
            let Some(tag) = field.try_ctx().map_err(|_| UdcError::Tlv("bad tag"))? else {
                return Err(UdcError::Tlv("non-context tag in declaration"));
            };

            match tag {
                id_tag::VENDOR_ID => id.vendor_id = Some(u16_field(&field, "vendorId")?),
                id_tag::PRODUCT_ID => id.product_id = Some(u16_field(&field, "productId")?),
                id_tag::DEVICE_NAME => {
                    id.device_name = Some(str_field(&field, "deviceName", DEVICE_NAME_MAX)?);
                }
                id_tag::PAIRING_INSTRUCTION => {
                    id.pairing_instruction = Some(str_field(
                        &field,
                        "pairingInstruction",
                        PAIRING_INSTRUCTION_MAX,
                    )?);
                }
                id_tag::PAIRING_HINT => id.pairing_hint = Some(u16_field(&field, "pairingHint")?),
                id_tag::ROTATING_ID => {
                    let bytes = field.octets().map_err(|_| UdcError::Field {
                        what: "rotatingId",
                        expected: "an octet string",
                    })?;
                    if bytes.len() > ROTATING_ID_MAX {
                        return Err(UdcError::Field {
                            what: "rotatingId",
                            expected: "at most 50 bytes",
                        });
                    }
                    id.rotating_id = Some(bytes.to_vec());
                }
                id_tag::CD_PORT => id.cd_port = Some(u16_field(&field, "cdPort")?),
                id_tag::TARGET_APP_LIST => id.target_apps = decode_target_apps(&field)?,
                id_tag::NO_PASSCODE => id.no_passcode = bool_field(&field, "noPasscode")?,
                id_tag::CD_UPON_PASSCODE_DIALOG => {
                    id.cd_upon_passcode_dialog = bool_field(&field, "cdUponPasscodeDialog")?;
                }
                id_tag::COMMISSIONER_PASSCODE => {
                    id.commissioner_passcode = bool_field(&field, "commissionerPasscode")?;
                }
                id_tag::COMMISSIONER_PASSCODE_READY => {
                    id.commissioner_passcode_ready =
                        bool_field(&field, "commissionerPasscodeReady")?;
                }
                id_tag::CANCEL_PASSCODE => {
                    id.cancel_passcode = bool_field(&field, "cancelPasscode")?;
                }
                id_tag::PASSCODE_LENGTH => {
                    id.passcode_length = Some(field.u8().map_err(|_| UdcError::Field {
                        what: "passcodeLength",
                        expected: "a u8",
                    })?);
                }
                // `deviceType` (tag 4) is in the reference's tag enum but neither written
                // nor read by it. Dropped here, like anything else we do not know.
                _ => {}
            }
        }

        Ok(id)
    }

    /// Build a whole datagram. Used by the tests that play the phone, and by nothing in
    /// the receiver — a Casting Player never sends one of these.
    ///
    /// # Errors
    /// [`UdcError::TooLong`] if the TLV body exceeds `kUdcTLVDataMaxBytes`.
    pub fn encode(&self) -> Result<Vec<u8>, UdcError> {
        let mut tlv = [0u8; TLV_MAX];
        let mut wb = WriteBuf::new(&mut tlv);

        let write = |wb: &mut WriteBuf| -> Result<(), rs_matter::error::Error> {
            wb.start_struct(&TLVTag::Anonymous)?;
            if let Some(v) = self.vendor_id {
                wb.u16(&TLVTag::Context(id_tag::VENDOR_ID), v)?;
            }
            if let Some(v) = self.product_id {
                wb.u16(&TLVTag::Context(id_tag::PRODUCT_ID), v)?;
            }
            if let Some(v) = &self.device_name {
                wb.utf8(&TLVTag::Context(id_tag::DEVICE_NAME), v)?;
            }
            if let Some(v) = &self.pairing_instruction {
                wb.utf8(&TLVTag::Context(id_tag::PAIRING_INSTRUCTION), v)?;
            }
            if let Some(v) = self.pairing_hint {
                wb.u16(&TLVTag::Context(id_tag::PAIRING_HINT), v)?;
            }
            if let Some(v) = self.cd_port {
                wb.u16(&TLVTag::Context(id_tag::CD_PORT), v)?;
            }
            if let Some(v) = &self.rotating_id {
                wb.str(&TLVTag::Context(id_tag::ROTATING_ID), v)?;
            }
            if !self.target_apps.is_empty() {
                wb.start_list(&TLVTag::Context(id_tag::TARGET_APP_LIST))?;
                for app in &self.target_apps {
                    wb.start_struct(&TLVTag::Context(id_tag::TARGET_APP))?;
                    wb.u16(&TLVTag::Context(id_tag::APP_VENDOR_ID), app.vendor_id)?;
                    wb.u16(&TLVTag::Context(id_tag::APP_PRODUCT_ID), app.product_id)?;
                    wb.end_container()?;
                }
                wb.end_container()?;
            }
            wb.bool(&TLVTag::Context(id_tag::NO_PASSCODE), self.no_passcode)?;
            wb.bool(
                &TLVTag::Context(id_tag::CD_UPON_PASSCODE_DIALOG),
                self.cd_upon_passcode_dialog,
            )?;
            wb.bool(
                &TLVTag::Context(id_tag::COMMISSIONER_PASSCODE),
                self.commissioner_passcode,
            )?;
            wb.bool(
                &TLVTag::Context(id_tag::COMMISSIONER_PASSCODE_READY),
                self.commissioner_passcode_ready,
            )?;
            wb.bool(
                &TLVTag::Context(id_tag::CANCEL_PASSCODE),
                self.cancel_passcode,
            )?;
            wb.u8(
                &TLVTag::Context(id_tag::PASSCODE_LENGTH),
                self.passcode_length.unwrap_or(0),
            )?;
            wb.end_container()
        };

        write(&mut wb).map_err(|_| UdcError::TooLong(TLV_MAX))?;

        let mut payload = vec![0u8; INSTANCE_NAME_BLOCK];
        payload[..self.instance_name.as_str().len()]
            .copy_from_slice(self.instance_name.as_str().as_bytes());
        payload.extend_from_slice(wb.as_slice());

        Ok(encode_frame(&payload))
    }
}

/// Why a commissioning attempt is not proceeding. Core spec §5.3.4, and the reference
/// implementation's `CdError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u16)]
#[non_exhaustive]
pub enum CdError {
    /// Nothing is wrong; the other flags carry the message.
    #[default]
    None = 0,
    /// The player could not find the client's commissionable advertisement.
    CommissionableDiscoveryFailed = 1,
    /// PASE could not be established.
    PaseConnectionFailed = 2,
    /// PASE was established but the passcode did not match.
    PaseAuthFailed = 3,
    /// The client's device attestation certificate did not validate.
    DacValidationFailed = 4,
    /// The client is already commissioned onto this fabric.
    AlreadyOnFabric = 5,
    /// The client did not come back on operational discovery after commissioning.
    OperationalDiscoveryFailed = 6,
    /// CASE could not be established.
    CaseConnectionFailed = 7,
    /// CASE was established but authentication failed.
    CaseAuthFailed = 8,
    /// Post-commissioning configuration failed.
    ConfigurationFailed = 9,
    /// Writing the client's bindings failed.
    BindingConfigurationFailed = 10,
    /// The client asked us to generate the passcode and we cannot.
    CommissionerPasscodeNotSupported = 11,
    /// The declaration's flags were mutually contradictory.
    InvalidIdentificationDeclarationParams = 12,
    /// The requested app is not installed and the user has been asked whether to install it.
    AppInstallConsentPending = 13,
    /// The requested app is being installed.
    AppInstalling = 14,
    /// The requested app failed to install.
    AppInstallFailed = 15,
    /// The app installed; ask again.
    AppInstalledRetryNeeded = 16,
    /// Commissioner-generated passcodes are supported but switched off.
    CommissionerPasscodeDisabled = 17,
    /// The client said its passcode was ready when we never displayed one.
    UnexpectedCommissionerPasscodeReady = 18,
}

impl CdError {
    /// Parse a wire value.
    ///
    /// # Errors
    /// [`UdcError::UnknownErrorCode`] for a value outside the spec's enum.
    pub fn from_wire(value: u16) -> Result<Self, UdcError> {
        Ok(match value {
            0 => Self::None,
            1 => Self::CommissionableDiscoveryFailed,
            2 => Self::PaseConnectionFailed,
            3 => Self::PaseAuthFailed,
            4 => Self::DacValidationFailed,
            5 => Self::AlreadyOnFabric,
            6 => Self::OperationalDiscoveryFailed,
            7 => Self::CaseConnectionFailed,
            8 => Self::CaseAuthFailed,
            9 => Self::ConfigurationFailed,
            10 => Self::BindingConfigurationFailed,
            11 => Self::CommissionerPasscodeNotSupported,
            12 => Self::InvalidIdentificationDeclarationParams,
            13 => Self::AppInstallConsentPending,
            14 => Self::AppInstalling,
            15 => Self::AppInstallFailed,
            16 => Self::AppInstalledRetryNeeded,
            17 => Self::CommissionerPasscodeDisabled,
            18 => Self::UnexpectedCommissionerPasscodeReady,
            other => return Err(UdcError::UnknownErrorCode(other)),
        })
    }
}

/// The player's half of the exchange: what is on the screen, or why nothing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommissionerDeclaration {
    /// Why commissioning is not proceeding. [`CdError::None`] is the ordinary case and
    /// is not wrapped in an `Option`: "no error code" and "the error code that means no
    /// error" are the same state, and having two spellings of it invites code that
    /// handles one and not the other.
    pub error_code: CdError,
    /// The player needs a passcode from the user and the client should prompt for one.
    pub needs_passcode: bool,
    /// None of the client's `targetAppList` entries is hosted here.
    pub no_apps_found: bool,
    /// A passcode is on the screen right now.
    pub passcode_dialog_displayed: bool,
    /// That passcode was generated by us, not by the client.
    pub commissioner_passcode: bool,
    /// A commissioning QR code is on the screen.
    pub qr_code_displayed: bool,
    /// The prompt was dismissed at the panel.
    pub cancel_passcode: bool,
    /// How many digits the displayed passcode has.
    pub passcode_length: u8,
}

impl CommissionerDeclaration {
    /// Build a whole datagram, ready for the client's `cdPort`.
    ///
    /// # Errors
    /// [`UdcError::TooLong`] if the body exceeds `kUdcTLVDataMaxBytes`.
    pub fn encode(&self) -> Result<Vec<u8>, UdcError> {
        let mut tlv = [0u8; TLV_MAX];
        let mut wb = WriteBuf::new(&mut tlv);

        let write = |wb: &mut WriteBuf| -> Result<(), rs_matter::error::Error> {
            wb.start_struct(&TLVTag::Anonymous)?;
            // Always written, including the zero: the reference writes every field
            // unconditionally, and a client that reads by position rather than by tag
            // (they exist) breaks on a sparse struct.
            wb.u16(&TLVTag::Context(cd_tag::ERROR_CODE), self.error_code as u16)?;
            wb.bool(
                &TLVTag::Context(cd_tag::NEEDS_PASSCODE),
                self.needs_passcode,
            )?;
            wb.bool(&TLVTag::Context(cd_tag::NO_APPS_FOUND), self.no_apps_found)?;
            wb.bool(
                &TLVTag::Context(cd_tag::PASSCODE_DIALOG_DISPLAYED),
                self.passcode_dialog_displayed,
            )?;
            wb.bool(
                &TLVTag::Context(cd_tag::COMMISSIONER_PASSCODE),
                self.commissioner_passcode,
            )?;
            wb.bool(
                &TLVTag::Context(cd_tag::QR_CODE_DISPLAYED),
                self.qr_code_displayed,
            )?;
            wb.bool(
                &TLVTag::Context(cd_tag::CANCEL_PASSCODE),
                self.cancel_passcode,
            )?;
            wb.u8(
                &TLVTag::Context(cd_tag::PASSCODE_LENGTH),
                self.passcode_length,
            )?;
            wb.end_container()
        };

        write(&mut wb).map_err(|_| UdcError::TooLong(TLV_MAX))?;

        Ok(encode_frame(wb.as_slice()))
    }

    /// Parse a datagram. The receiver never needs this; the tests that play the phone do.
    ///
    /// # Errors
    /// [`UdcError`] for a malformed datagram.
    pub fn decode(datagram: &[u8]) -> Result<Self, UdcError> {
        let payload = decode_frame(datagram)?;

        let root = TLVElement::new(payload);
        let fields = root
            .structure()
            .map_err(|_| UdcError::Tlv("not a struct"))?;

        let mut cd = Self::default();
        for field in fields.iter() {
            let field = field.map_err(|_| UdcError::Tlv("truncated element"))?;
            let Some(tag) = field.try_ctx().map_err(|_| UdcError::Tlv("bad tag"))? else {
                return Err(UdcError::Tlv("non-context tag in declaration"));
            };

            match tag {
                cd_tag::ERROR_CODE => {
                    cd.error_code = CdError::from_wire(u16_field(&field, "errorCode")?)?;
                }
                cd_tag::NEEDS_PASSCODE => {
                    cd.needs_passcode = bool_field(&field, "needsPasscode")?;
                }
                cd_tag::NO_APPS_FOUND => cd.no_apps_found = bool_field(&field, "noAppsFound")?,
                cd_tag::PASSCODE_DIALOG_DISPLAYED => {
                    cd.passcode_dialog_displayed = bool_field(&field, "passcodeDialogDisplayed")?;
                }
                cd_tag::COMMISSIONER_PASSCODE => {
                    cd.commissioner_passcode = bool_field(&field, "commissionerPasscode")?;
                }
                cd_tag::QR_CODE_DISPLAYED => {
                    cd.qr_code_displayed = bool_field(&field, "qrCodeDisplayed")?;
                }
                cd_tag::CANCEL_PASSCODE => {
                    cd.cancel_passcode = bool_field(&field, "cancelPasscode")?;
                }
                cd_tag::PASSCODE_LENGTH => {
                    cd.passcode_length = field.u8().map_err(|_| UdcError::Field {
                        what: "passcodeLength",
                        expected: "a u8",
                    })?;
                }
                _ => {}
            }
        }

        Ok(cd)
    }
}

/// Strip the unsecured Matter framing, returning the payload.
///
/// The header is 14 fixed bytes here and cannot be anything else: a UDC datagram carries
/// no source or destination node id (nothing has an operational identity yet) and is
/// never encrypted (there is no session to encrypt under), so every optional field of the
/// general message format is absent by construction.
fn decode_frame(datagram: &[u8]) -> Result<&[u8], UdcError> {
    const HEADER_LEN: usize = PLAIN_HEADER_LEN + PROTO_HEADER_LEN;

    if datagram.len() < HEADER_LEN {
        return Err(UdcError::Truncated {
            got: datagram.len(),
            need: HEADER_LEN,
        });
    }

    // Message flags: version in the high nibble, S and DSIZ in the low bits. Any node id
    // present means this is not the unsecured message UDC is defined over.
    let msg_flags = datagram[0];
    if msg_flags & 0b0000_0111 != 0 {
        return Err(UdcError::Tlv("node ids present on a UDC message"));
    }

    // A non-zero session id, or the session-type bits of the security flags, would make
    // this an encrypted message.
    let session_id = u16::from_le_bytes([datagram[1], datagram[2]]);
    let sec_flags = datagram[3];
    if session_id != 0 || sec_flags & 0b0000_0011 != 0 {
        return Err(UdcError::Encrypted);
    }

    // Exchange flags occupy datagram[8]; the initiator bit is set on both directions and
    // carries no information for us. The opcode and protocol id do.
    let opcode = datagram[9];
    let protocol_id = u16::from_le_bytes([datagram[12], datagram[13]]);

    if protocol_id != PROTOCOL_ID {
        return Err(UdcError::WrongProtocol {
            got: protocol_id,
            want: PROTOCOL_ID,
        });
    }
    if opcode != OPCODE_DECLARATION {
        return Err(UdcError::UnknownOpcode(opcode));
    }

    Ok(&datagram[HEADER_LEN..])
}

/// Wrap a payload in the same 14 bytes.
fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PLAIN_HEADER_LEN + PROTO_HEADER_LEN + payload.len());

    // Plain header: version 0, no node ids, session 0, unencrypted, counter 0.
    //
    // Counter 0 is the reference implementation's, not an oversight of ours: it never
    // calls `SetMessageCounter` for UDC, and it sends the same datagram five times, so a
    // peer that deduplicated on the counter would drop four retransmits of a message that
    // has no acknowledgement.
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

    // Payload header: initiator, no ack requested; opcode 0; exchange id 0; protocol 9.
    out.push(0x01);
    out.push(OPCODE_DECLARATION);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&PROTOCOL_ID.to_le_bytes());

    out.extend_from_slice(payload);
    out
}

fn decode_target_apps(field: &TLVElement<'_>) -> Result<Vec<TargetApp>, UdcError> {
    let list = field
        .container()
        .map_err(|_| UdcError::Tlv("targetAppList is not a container"))?;

    let mut apps = Vec::new();
    for entry in list.iter() {
        let entry = entry.map_err(|_| UdcError::Tlv("truncated targetAppList entry"))?;
        let Some(tag) = entry.try_ctx().map_err(|_| UdcError::Tlv("bad tag"))? else {
            return Err(UdcError::Tlv("non-context tag in targetAppList"));
        };
        if tag != id_tag::TARGET_APP {
            continue;
        }

        let app = entry
            .structure()
            .map_err(|_| UdcError::Tlv("targetApp is not a struct"))?;
        let mut vendor_id = 0u16;
        let mut product_id = 0u16;
        for f in app.iter() {
            let f = f.map_err(|_| UdcError::Tlv("truncated targetApp field"))?;
            match f.try_ctx().map_err(|_| UdcError::Tlv("bad tag"))? {
                Some(id_tag::APP_VENDOR_ID) => vendor_id = u16_field(&f, "app vendorId")?,
                Some(id_tag::APP_PRODUCT_ID) => product_id = u16_field(&f, "app productId")?,
                _ => {}
            }
        }

        // Vendor 0 is not a vendor. The reference drops these entries silently and so do
        // we, rather than letting a zero match every content app's vendor check.
        if vendor_id != 0 && apps.len() < TARGET_APPS_MAX {
            apps.push(TargetApp {
                vendor_id,
                product_id,
            });
        }
    }

    Ok(apps)
}

fn u16_field(field: &TLVElement<'_>, what: &'static str) -> Result<u16, UdcError> {
    field.u16().map_err(|_| UdcError::Field {
        what,
        expected: "a u16",
    })
}

fn bool_field(field: &TLVElement<'_>, what: &'static str) -> Result<bool, UdcError> {
    field.bool().map_err(|_| UdcError::Field {
        what,
        expected: "a bool",
    })
}

fn str_field(field: &TLVElement<'_>, what: &'static str, max: usize) -> Result<String, UdcError> {
    let s = field.utf8().map_err(|_| UdcError::Field {
        what,
        expected: "a UTF-8 string",
    })?;
    if s.len() > max {
        return Err(UdcError::Field {
            what,
            expected: "a string within the spec's length limit",
        });
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// An `IdentificationDeclaration` byte for byte, as `connectedhomeip`'s
    /// `UserDirectedCommissioningClient` lays one out. Written by hand from the encoder
    /// rather than round-tripped through ours, so the test disagrees with us when we are
    /// wrong — a fixture generated from the code under test cannot (D6, and the lesson
    /// recorded in STATUS: a fixture agrees with whatever you send it).
    const CHIP_IDENTIFICATION: &[u8] = &[
        // Plain header. Message flags 0x00: version 0, no source node id, DSIZ 0.
        0x00, // Session id 0 — there is no session.
        0x00, 0x00, // Security flags 0x00: unicast, unencrypted.
        0x00, // Message counter 0, which is what the reference sends.
        0x00, 0x00, 0x00,
        0x00, // Payload header. Exchange flags 0x01: initiator, no ack requested.
        0x01, // Opcode 0x00 — both directions use it.
        0x00, // Exchange id 0.
        0x00, 0x00, // Protocol id 0x0009, User Directed Commissioning.
        0x09, 0x00,
        // Instance name: 16 characters, then the NUL that pads the fixed block to 17.
        b'B', b'C', b'5', b'C', b'0', b'1', b'A', b'6', b'1', b'C', b'4', b'8', b'8', b'9', b'2',
        b'F', 0x00, // TLV body: an anonymous structure.
        0x15, // Context tag 1 (vendorId), u16 = 0xFFF1 (the test vendor).
        0x25, 0x01, 0xF1, 0xFF, // Context tag 2 (productId), u16 = 0x8001.
        0x25, 0x02, 0x01, 0x80, // Context tag 3 (deviceName), UTF-8 string, 5 bytes: "Phone".
        0x2C, 0x03, 0x05, b'P', b'h', b'o', b'n', b'e',
        // Context tag 6 (pairingHint), u8 = 0x21 — the reference's `Put` narrows to the
        // smallest unsigned type that fits, so a u16 field below 256 is one byte here.
        0x24, 0x06, 0x21, // Context tag 8 (cdPort), u16 = 5550.
        0x25, 0x08, 0xAE, 0x15, // Context tag 9 (targetAppList), a TLV list.
        0x37, 0x09, // Context tag 10 (targetApp), a structure.
        0x35, 0x0A, // Context tag 11 (app vendorId), u16 = 4996. An illustrative value, not a
        // looked-up one: CSA vendor ids live in the distributed compliance ledger and
        // only the test range (0xFFF1-4) is safe to assert from memory.
        0x25, 0x0B, 0x84, 0x13, // Context tag 12 (app productId), u8 = 1.
        0x24, 0x0C, 0x01, // End of the targetApp structure.
        0x18, // End of the targetAppList list.
        0x18, // Context tag 13 (noPasscode) = false.
        0x28, 0x0D, // Context tag 14 (cdUponPasscodeDialog) = true.
        0x29, 0x0E, // Context tag 15 (commissionerPasscode) = true.
        0x29, 0x0F, // Context tag 16 (commissionerPasscodeReady) = false.
        0x28, 0x10, // Context tag 17 (cancelPasscode) = false.
        0x28, 0x11, // Context tag 18 (passcodeLength), u8 = 6.
        0x24, 0x12, 0x06, // End of the anonymous structure.
        0x18,
    ];

    fn chip_fixture() -> IdentificationDeclaration {
        IdentificationDeclaration::decode(CHIP_IDENTIFICATION).unwrap()
    }

    #[test]
    fn reads_the_reference_encoding() {
        let id = chip_fixture();
        assert_eq!(id.instance_name.as_str(), "BC5C01A61C48892F");
        assert_eq!(id.vendor_id, Some(0xFFF1));
        assert_eq!(id.product_id, Some(0x8001));
        assert_eq!(id.device_name.as_deref(), Some("Phone"));
        assert_eq!(id.pairing_hint, Some(0x21));
        assert_eq!(id.cd_port, Some(5550));
        assert_eq!(id.passcode_length, Some(6));
        assert!(id.commissioner_passcode);
        assert!(id.cd_upon_passcode_dialog);
        assert!(!id.no_passcode);
    }

    #[test]
    fn reads_the_target_app_list() {
        let id = chip_fixture();
        assert_eq!(
            id.target_apps,
            vec![TargetApp {
                vendor_id: 4996,
                product_id: 1
            }]
        );
    }

    /// The whole point of the flag: this client wants *us* to put a passcode on screen.
    #[test]
    fn a_fresh_declaration_asks_to_be_commissioned() {
        assert_eq!(chip_fixture().request(), UdcRequest::Commission);
    }

    /// Cancel outranks ready. A client that set both changed its mind twice, and the
    /// second thought was to stop — reading these in the other order would commission a
    /// phone whose user had just dismissed the prompt.
    #[test]
    fn cancel_outranks_passcode_ready() {
        let mut id = chip_fixture();
        id.commissioner_passcode_ready = true;
        assert_eq!(id.request(), UdcRequest::PasscodeReady);
        id.cancel_passcode = true;
        assert_eq!(id.request(), UdcRequest::Cancel);
    }

    /// `cdPort` of zero is the field's default, so an older client that never sets it
    /// reads as declining the reply rather than as asking for one on port 0.
    #[test]
    fn a_zero_reply_port_means_no_reply() {
        let mut id = chip_fixture();
        assert_eq!(id.reply_port(), Some(5550));
        id.cd_port = Some(0);
        assert_eq!(id.reply_port(), None);
        id.cd_port = None;
        assert_eq!(id.reply_port(), None);
    }

    /// The reference reader returns early on a payload that is exactly the name block.
    #[test]
    fn a_declaration_may_carry_no_tlv_at_all() {
        let mut datagram = CHIP_IDENTIFICATION[..PLAIN_HEADER_LEN + PROTO_HEADER_LEN].to_vec();
        datagram.extend_from_slice(b"BC5C01A61C48892F\0");
        let id = IdentificationDeclaration::decode(&datagram).unwrap();
        assert_eq!(id.instance_name.as_str(), "BC5C01A61C48892F");
        assert_eq!(id.request(), UdcRequest::Commission);
        assert!(id.target_apps.is_empty());
    }

    /// A short instance name still leaves the TLV at offset 17: the block is fixed-length,
    /// not NUL-terminated-and-packed. Getting this wrong reads the TLV from the padding.
    #[test]
    fn the_instance_name_block_is_fixed_length() {
        let short = IdentificationDeclaration {
            instance_name: InstanceName::new("AB12").unwrap(),
            ..chip_fixture()
        };
        let encoded = short.encode().unwrap();
        let payload = &encoded[PLAIN_HEADER_LEN + PROTO_HEADER_LEN..];
        assert_eq!(&payload[..4], b"AB12");
        assert_eq!(&payload[4..INSTANCE_NAME_BLOCK], &[0u8; 13]);
        assert_eq!(payload[INSTANCE_NAME_BLOCK], 0x15, "TLV starts at 17");
        assert_eq!(IdentificationDeclaration::decode(&encoded).unwrap(), short);
    }

    #[test]
    fn round_trips_every_field() {
        let id = chip_fixture();
        let encoded = id.encode().unwrap();
        assert_eq!(IdentificationDeclaration::decode(&encoded).unwrap(), id);
    }

    #[test]
    fn rejects_an_encrypted_message() {
        let mut datagram = CHIP_IDENTIFICATION.to_vec();
        datagram[1] = 0x01; // a session id makes it a secured message
        assert_eq!(
            IdentificationDeclaration::decode(&datagram),
            Err(UdcError::Encrypted)
        );
    }

    #[test]
    fn rejects_another_protocol_on_the_udc_port() {
        let mut datagram = CHIP_IDENTIFICATION.to_vec();
        datagram[12] = 0x00; // 0x0000: the secure channel protocol
        assert_eq!(
            IdentificationDeclaration::decode(&datagram),
            Err(UdcError::WrongProtocol {
                got: 0x0000,
                want: 0x0009
            })
        );
    }

    #[test]
    fn rejects_a_truncated_datagram() {
        assert!(matches!(
            IdentificationDeclaration::decode(&CHIP_IDENTIFICATION[..10]),
            Err(UdcError::Truncated { .. })
        ));
        // Long enough for the header, too short for the instance-name block.
        assert!(matches!(
            IdentificationDeclaration::decode(&CHIP_IDENTIFICATION[..20]),
            Err(UdcError::Truncated { .. })
        ));
    }

    /// A `CommissionerDeclaration` is pure TLV — no instance-name block. Encoding one
    /// with the prefix would put the client's parser 17 bytes into our error code.
    #[test]
    fn the_reply_carries_no_instance_name_block() {
        let cd = CommissionerDeclaration {
            passcode_dialog_displayed: true,
            commissioner_passcode: true,
            passcode_length: 6,
            ..CommissionerDeclaration::default()
        };
        let encoded = cd.encode().unwrap();
        assert_eq!(
            &encoded[..PLAIN_HEADER_LEN + PROTO_HEADER_LEN],
            &CHIP_IDENTIFICATION[..PLAIN_HEADER_LEN + PROTO_HEADER_LEN],
            "same framing in both directions"
        );
        assert_eq!(
            encoded[PLAIN_HEADER_LEN + PROTO_HEADER_LEN],
            0x15,
            "TLV starts immediately"
        );
        assert_eq!(CommissionerDeclaration::decode(&encoded).unwrap(), cd);
    }

    /// Every field is written even when it is zero or false, because the reference does
    /// and because "absent" and "false" are the same answer to a client reading by tag.
    #[test]
    fn the_reply_writes_every_field() {
        let encoded = CommissionerDeclaration::default().encode().unwrap();
        let body = &encoded[PLAIN_HEADER_LEN + PROTO_HEADER_LEN..];
        assert_eq!(
            body,
            &[
                0x15, // anonymous struct
                0x24, 0x01, 0x00, // errorCode = 0
                0x28, 0x02, // needsPasscode = false
                0x28, 0x03, // noAppsFound = false
                0x28, 0x04, // passcodeDialogDisplayed = false
                0x28, 0x05, // commissionerPasscode = false
                0x28, 0x06, // qrCodeDisplayed = false
                0x28, 0x07, // cancelPasscode = false
                0x24, 0x08, 0x00, // passcodeLength = 0
                0x18, // end
            ]
        );
    }

    #[test]
    fn reply_error_codes_survive_the_wire() {
        for code in [
            CdError::None,
            CdError::PaseAuthFailed,
            CdError::CommissionerPasscodeNotSupported,
            CdError::UnexpectedCommissionerPasscodeReady,
        ] {
            let cd = CommissionerDeclaration {
                error_code: code,
                ..CommissionerDeclaration::default()
            };
            let decoded = CommissionerDeclaration::decode(&cd.encode().unwrap()).unwrap();
            assert_eq!(decoded.error_code, code);
        }
    }

    #[test]
    fn an_unknown_error_code_is_rejected_rather_than_guessed() {
        assert_eq!(CdError::from_wire(19), Err(UdcError::UnknownErrorCode(19)));
    }

    /// Tags arrive that this build has never heard of the moment a phone updates. Skipping
    /// them is what keeps an old panel castable; rejecting the message is not.
    #[test]
    fn unknown_context_tags_are_ignored() {
        let mut datagram = CHIP_IDENTIFICATION.to_vec();
        // Splice a context tag 99 carrying a u8 in just after the struct header.
        let tlv_start = PLAIN_HEADER_LEN + PROTO_HEADER_LEN + INSTANCE_NAME_BLOCK + 1;
        datagram.splice(tlv_start..tlv_start, [0x24, 99, 0x07]);
        assert_eq!(
            IdentificationDeclaration::decode(&datagram).unwrap(),
            chip_fixture()
        );
    }

    #[test]
    fn an_instance_name_is_bounded() {
        assert!(InstanceName::new("").is_err());
        assert!(InstanceName::new("0123456789ABCDEF").is_ok());
        assert!(InstanceName::new("0123456789ABCDEF0").is_err());
    }
}
