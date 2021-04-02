use std::convert::TryInto;
use std::time::Duration;

use ledger_apdu::{APDUCommand, APDUAnswer, APDUErrorCodes, map_apdu_error_description};
use ledger::{TransportNativeHID, LedgerHIDError};

use types::{PUBLIC_KEY_LEN, PublicKey, ImplicitAddress};

mod retry_request;
pub use retry_request::RetryRequest;

mod reconnect_request;
pub use reconnect_request::ReconnectRequest;

mod run_app_request;
pub use run_app_request::RunAppRequest;

mod ledger_request;
use ledger_request::{LedgerRequest, LedgerRequestData, ResultHandler};

mod ledger_response;
pub use ledger_response::LedgerResponse;

const TEZOS_APP_NAME: &'static str = "Tezos Wallet";
const TEZOS_CLA: u8 = 0x80;

#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub enum LedgerError {
    Transport(#[from] LedgerHIDError),
    #[error("{}", map_apdu_error_description(*.0))]
    APDU(u16),
    RunApp(#[from] RunAppError),
    #[error("invalid data length received from ledger")]
    InvalidDataLength,
}

#[derive(thiserror::Error, Debug)]
pub enum RunAppErrorKind {
    #[error("{0}")]
    Transport(#[from] LedgerHIDError),
    #[error("app name is too large! Length must be <= 255")]
    NameTooLarge,
    #[error("{}", map_apdu_error_description(*.0))]
    APDU(u16),
    #[error("failed to reconnect after opening an app. {0}")]
    Reconnect(LedgerHIDError),
}

#[derive(thiserror::Error, Debug)]
#[error("running app with name \"{name}\" on ledger failed! Reason: {kind}")]
pub struct RunAppError {
    name: String,
    kind: RunAppErrorKind,
}

pub struct Ledger {
    transport: TransportNativeHID,
}

impl Ledger {
    fn encode_path(path: &Vec<u32>) -> Vec<u8> {
        path.iter()
            .flat_map(|x| x.to_be_bytes().to_vec())
            .collect()
    }

    /// Connect to first Ledger device.
    pub fn connect() -> Result<Self, LedgerError> {
        let transport = TransportNativeHID::new()?;

        Ok(Self {
            transport,
        })
    }

    /// Tries to reconnect to Ledger device every 200 millis for a
    /// number of `attempts`.
    pub(crate) fn reconnect(&mut self, attempts: usize) -> Result<(), LedgerHIDError> {
        for i in 1..=attempts {
            std::thread::sleep(Duration::from_millis(200));
            match TransportNativeHID::new() {
                Ok(transport) => {
                    self.transport = transport;
                    return Ok(());
                }
                Err(err) => {
                    if i == attempts {
                        return Err(err);
                    }
                }
            }
        }

        return Ok(());
    }

    fn should_reconnect(&self, ledger_err: &LedgerError) -> bool {
        let transport_err = match &ledger_err {
            LedgerError::Transport(err) => err,
            _ => { return false; }
        };

        match &transport_err {
            LedgerHIDError::Hid(hid_err) => {
                match &hid_err {
                    hidapi::HidError::HidApiError { message } => {
                        if message == "No such device" {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        false
    }

    pub(crate) fn raw_call(
        &mut self,
        command: &APDUCommand,
    ) -> Result<APDUAnswer, LedgerError> {
        Ok(self.transport.exchange(command)?)
    }

    pub(crate) fn call<'a, T: 'static>(
        &'a mut self,
        command: APDUCommand,
        handler: Box<ResultHandler<T>>,
    ) -> LedgerResponse<'a, T>
    {
        let answer = match self.raw_call(&command) {
            Ok(x) => x,
            Err(ledger_err) => {
                if self.should_reconnect(&ledger_err) {
                    return ReconnectRequest::new(self, LedgerRequestData { command, handler }).into();
                } else {
                    return ledger_err.into();
                }
            }
        };

        if answer.retcode != APDUErrorCodes::NoError as u16 {
            if answer.retcode == APDUErrorCodes::ClaNotSupported as u16 {
                if command.cla == TEZOS_CLA {
                    return RunAppRequest::new(
                        self,
                        LedgerRequestData { command, handler },
                        TEZOS_APP_NAME,
                    ).into();
                }
            }
            return LedgerError::APDU(answer.retcode).into();
        }

        handler(answer.data).into()
    }

    /// Run an application on a Ledger device.
    ///
    /// ## Warning
    ///
    /// Might disconnect and reconnect Ledger from PC.
    pub fn run_app(&mut self, name: &str) -> Result<APDUAnswer, RunAppError> {
        let name_bytes = name.as_bytes().to_vec();

        if name_bytes.len() > u8::MAX as usize {
            return Err(RunAppError {
                name: name.to_string(),
                kind: RunAppErrorKind::NameTooLarge,
            });
        }

        let command = APDUCommand {
            cla: 0xE0,
            ins: 0xD8,
            p1: 0x00,
            p2: 0x00,
            data: name_bytes,
        };

        let result = self.transport
            .exchange(&command)
            .map_err(|err| RunAppError {
                name: name.to_string(),
                kind: err.into(),
            })?;

        if result.retcode == APDUErrorCodes::NoError as u16 {
            Ok(result)
        } else {
            Err(RunAppError {
                name: name.to_string(),
                kind: RunAppErrorKind::APDU(result.retcode),
            })
        }
    }


    fn public_key_request<'a>(
        &'a mut self,
        path: Vec<u32>,
        prompt: bool,
    ) -> LedgerRequest<'a, PublicKey>
    {
        let path_bytes = Self::encode_path(&path);
        let command = APDUCommand {
            cla: TEZOS_CLA,
            ins: if prompt { 0x03 } else { 0x02 },
            p1: 0x00,
            p2: 0x00,
            data: [vec![path.len() as u8], path_bytes].concat(),
        };

        LedgerRequest::new(self, command)
            .map(|bytes| {
                let len = bytes[0] as usize;

                // len also counts in first byte, which specifies "curve".
                if len > bytes.len() + 1 || len - 1 != PUBLIC_KEY_LEN {
                    return Err(LedgerError::InvalidDataLength);
                }

                // TODO: implement for other curves.
                Ok(PublicKey::edpk(
                    // remove 2 byte prefix from the actual key.
                    // - first byte is for length of following public key.
                    // - second byte is curve type, right now we ignore it
                    //   as we only support edpk.
                    bytes[2..(PUBLIC_KEY_LEN + 2)].try_into()
                        .map_err(|_| LedgerError::InvalidDataLength)?
                ))
            })
    }

    /// Get Tezos address from Ledger for a given `path`(key derivation path).
    ///
    /// In reality we get public key from Ledger and then hash it.
    ///
    /// To avoid extra call to the ledger if public key is needed too,
    /// one can simply do:
    /// ```rust
    /// let public_key = ledger.get_public_key(path, prompt).unwrap();
    /// let address = public_key.hash();
    /// ```
    ///
    /// If `prompt` = `true`, user will be prompted on Ledger device, whether
    /// he/she wants to share public key for a given address to us.
    /// This functionality can be used to get address first without prompting,
    /// then verifying it by asking the user if address is same as shown in Ledger.
    pub fn get_address<'a>(
        &'a mut self,
        path: Vec<u32>,
        prompt: bool,
    ) -> LedgerResponse<'a, ImplicitAddress>
    {
        self.public_key_request(path, prompt)
            .map(|pub_key| Ok(pub_key.hash()))
            .send()
    }

    /// Get Tezos public key from Ledger for a given `path`(key derivation path).
    ///
    /// If `prompt` = `true`, user will be prompted on Ledger device, whether
    /// he/she wants to share public key for a given address to us.
    pub fn get_public_key<'a>(
        &'a mut self,
        path: Vec<u32>,
        prompt: bool,
    ) -> LedgerResponse<'a, PublicKey>
    {
        self.public_key_request(path, prompt).send()
    }
}