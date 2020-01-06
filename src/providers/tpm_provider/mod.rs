// Copyright (c) 2019, Arm Limited, All Rights Reserved
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may
// not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//          http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use super::Provide;
use crate::authenticators::ApplicationName;
use crate::key_id_managers::{KeyTriple, ManageKeyIDs};
use log::{error, info};
use parsec_interface::operations::key_attributes::*;
use parsec_interface::operations::ProviderInfo;
use parsec_interface::operations::{OpAsymSign, ResultAsymSign};
use parsec_interface::operations::{OpAsymVerify, ResultAsymVerify};
use parsec_interface::operations::{OpCreateKey, ResultCreateKey};
use parsec_interface::operations::{OpDestroyKey, ResultDestroyKey};
use parsec_interface::operations::{OpExportPublicKey, ResultExportPublicKey};
use parsec_interface::operations::{OpImportKey, ResultImportKey};
use parsec_interface::operations::{OpListOpcodes, ResultListOpcodes};
use parsec_interface::requests::{Opcode, ProviderID, ResponseStatus, Result};
use picky_asn1::wrapper::IntegerAsn1;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};
use tss_esapi::{
    constants::TPM2_ALG_SHA256, response_code::Tss2ResponseCodeKind, utils::AsymSchemeUnion,
    utils::Signature, utils::TpmsContext, Tcti,
};
use uuid::Uuid;

const SUPPORTED_OPCODES: [Opcode; 7] = [
    Opcode::CreateKey,
    Opcode::DestroyKey,
    Opcode::AsymSign,
    Opcode::AsymVerify,
    Opcode::ImportKey,
    Opcode::ExportPublicKey,
    Opcode::ListOpcodes,
];

const ROOT_KEY_SIZE: usize = 2048;
const ROOT_KEY_AUTH_SIZE: usize = 32;

pub struct TpmProvider {
    // The Mutex is needed both because interior mutability is needed to the ESAPI Context
    // structure that is shared between threads and because two threads are not allowed the same
    // ESAPI context simultaneously.
    esapi_context: Mutex<tss_esapi::TransientObjectContext>,
    // The Key ID Manager stores the key context and its associated authValue (a PasswordContext
    // structure).
    key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
}

// Public exponent value for all RSA keys.
const PUBLIC_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];
const AUTH_VAL_LEN: usize = 32;

// The RSA Public Key data are DER encoded with the following representation:
// RSAPublicKey ::= SEQUENCE {
//     modulus            INTEGER,  -- n
//     publicExponent     INTEGER   -- e
// }
#[derive(Serialize, Deserialize, Debug)]
struct RsaPublicKey {
    modulus: IntegerAsn1,
    public_exponent: IntegerAsn1,
}

// The PasswordContext is what is stored by the Key ID Manager.
#[derive(Serialize, Deserialize)]
struct PasswordContext {
    context: TpmsContext,
    auth_value: Vec<u8>,
}

/// Inserts a new mapping in the Key ID manager that stores the PasswordContext.
fn insert_password_context(
    store_handle: &mut dyn ManageKeyIDs,
    key_triple: KeyTriple,
    password_context: PasswordContext,
) -> Result<()> {
    let error_storing = |e| {
        error!("Error storing a mapping: {}.", e);
        Err(ResponseStatus::KeyIDManagerError)
    };
    let error_serializing = |e| {
        error!("Error serializing the PasswordContext: {}.", e);
        Err(ResponseStatus::KeyIDManagerError)
    };

    if store_handle
        .insert(
            key_triple,
            bincode::serialize(&password_context).or_else(error_serializing)?,
        )
        .or_else(error_storing)?
        .is_some()
    {
        error!("Inserting a mapping in the Key ID Manager that would overwrite an existing one.");
        Err(ResponseStatus::KeyAlreadyExists)
    } else {
        Ok(())
    }
}

/// Gets a PasswordContext mapping to the KeyTriple given.
fn get_password_context(
    store_handle: &dyn ManageKeyIDs,
    key_triple: KeyTriple,
) -> Result<PasswordContext> {
    let password_context = store_handle.get(&key_triple).or_else(|e| {
        error!("Error getting a mapping: {}.", e);
        Err(ResponseStatus::KeyIDManagerError)
    })?;
    let password_context = match password_context {
        Some(context) => context,
        None => {
            error!(
                "Key triple \"{}\" does not exist in the Key ID Manager.",
                key_triple
            );
            return Err(ResponseStatus::KeyDoesNotExist);
        }
    };
    Ok(bincode::deserialize(password_context).or_else(|e| {
        error!("Error deserializing the PasswordContext: {}.", e);
        Err(ResponseStatus::KeyIDManagerError)
    })?)
}

impl TpmProvider {
    /// Creates and initialise a new instance of TpmProvider.
    fn new(
        key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
        esapi_context: tss_esapi::TransientObjectContext,
    ) -> Option<TpmProvider> {
        Some(TpmProvider {
            esapi_context: Mutex::new(esapi_context),
            key_id_store,
        })
    }
}

impl Provide for TpmProvider {
    fn list_opcodes(&self, _op: OpListOpcodes) -> Result<ResultListOpcodes> {
        Ok(ResultListOpcodes {
            opcodes: SUPPORTED_OPCODES.iter().copied().collect(),
        })
    }

    fn describe(&self) -> ProviderInfo {
        ProviderInfo {
            // Assigned UUID for this provider: 1e4954a4-ff21-46d3-ab0c-661eeb667e1d
            uuid: Uuid::parse_str("1e4954a4-ff21-46d3-ab0c-661eeb667e1d").expect("UUID parsing failed"),
            description: String::from("TPM provider, interfacing with a library implementing the TCG TSS 2.0 Enhanced System API specification."),
            vendor: String::from("Trusted Computing Group (TCG)"),
            version_maj: 0,
            version_min: 1,
            version_rev: 0,
            id: ProviderID::TpmProvider,
        }
    }

    fn create_key(&self, app_name: ApplicationName, op: OpCreateKey) -> Result<ResultCreateKey> {
        if op.key_attributes.key_type != KeyType::RsaKeypair
            || op.key_attributes.algorithm != Algorithm::sign(SignAlgorithm::RsaPkcs1v15Sign, None)
        {
            error!("The TPM provider currently only supports creating RSA key pairs for signing and verifying.");
            return Err(ResponseStatus::UnsupportedOperation);
        }

        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);
        // This should never panic on 32 bits or more machines.
        let key_size = std::convert::TryFrom::try_from(op.key_attributes.key_size)
            .expect("Conversion to usize failed.");

        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let (key_context, auth_value) = esapi_context
            .create_rsa_signing_key(key_size, AUTH_VAL_LEN)
            .or_else(|e| {
                error!("Error creating a RSA signing key: {}.", e);
                Err(ResponseStatus::PsaErrorHardwareFailure)
            })?;

        insert_password_context(
            &mut *store_handle,
            key_triple,
            PasswordContext {
                context: key_context,
                auth_value,
            },
        )?;

        Ok(ResultCreateKey {})
    }

    fn import_key(&self, app_name: ApplicationName, op: OpImportKey) -> Result<ResultImportKey> {
        if op.key_attributes.key_type != KeyType::RsaPublicKey
            || op.key_attributes.algorithm != Algorithm::sign(SignAlgorithm::RsaPkcs1v15Sign, None)
        {
            error!(
                "The TPM provider currently only supports importing RSA public key for verifying."
            );
            return Err(ResponseStatus::UnsupportedOperation);
        }

        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);
        let key_data = op.key_data;

        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let public_key: RsaPublicKey = picky_asn1_der::from_bytes(&key_data).or_else(|err| {
            error!("Could not deserialise key elements: {}.", err);
            Err(ResponseStatus::PsaErrorCommunicationFailure)
        })?;
        if public_key.public_exponent.as_bytes_be() != PUBLIC_EXPONENT {
            error!("The TPM Provider only supports 0x101 as public exponent for RSA public keys, {:?} given.", public_key.public_exponent.as_bytes_be());
            return Err(ResponseStatus::UnsupportedOperation);
        }
        let key_data = public_key.modulus.as_bytes_be();

        let len = key_data.len();
        if len < 128 {
            error!(
                "The TPM provider only supports 1024 bits or bigger RSA public keys ({} bits given).",
                len * 8
            );
            return Err(ResponseStatus::UnsupportedOperation);
        }

        let pub_key_context = esapi_context
            .load_external_rsa_public_key(&key_data)
            .or_else(|e| {
                error!("Error creating a RSA signing key: {}.", e);
                Err(ResponseStatus::PsaErrorHardwareFailure)
            })?;

        insert_password_context(
            &mut *store_handle,
            key_triple,
            PasswordContext {
                context: pub_key_context,
                auth_value: Vec::new(),
            },
        )?;

        Ok(ResultImportKey {})
    }

    fn export_public_key(
        &self,
        app_name: ApplicationName,
        op: OpExportPublicKey,
    ) -> Result<ResultExportPublicKey> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);

        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let password_context = get_password_context(&*store_handle, key_triple)?;

        let pub_key_data = esapi_context
            .read_public_key(password_context.context)
            .or_else(|e| {
                error!("Error reading a public key: {}.", e);
                Err(ResponseStatus::PsaErrorHardwareFailure)
            })?;

        let key = RsaPublicKey {
            modulus: IntegerAsn1::from_signed_bytes_be(pub_key_data),
            public_exponent: IntegerAsn1::from_signed_bytes_be(PUBLIC_EXPONENT.to_vec()),
        };
        let key_data = picky_asn1_der::to_vec(&key).or_else(|err| {
            error!("Could not serialise key elements: {}.", err);
            Err(ResponseStatus::PsaErrorCommunicationFailure)
        })?;

        Ok(ResultExportPublicKey { key_data })
    }

    fn destroy_key(&self, app_name: ApplicationName, op: OpDestroyKey) -> Result<ResultDestroyKey> {
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);
        let mut store_handle = self.key_id_store.write().expect("Key store lock poisoned");

        let error_closure = |e| {
            error!("Error storing a mapping: {}.", e);
            Err(ResponseStatus::KeyIDManagerError)
        };
        if store_handle
            .remove(&key_triple)
            .or_else(error_closure)?
            .is_none()
        {
            error!(
                "Key triple \"{}\" does not exist in the Key ID Manager.",
                key_triple
            );
            Err(ResponseStatus::KeyDoesNotExist)
        } else {
            Ok(ResultDestroyKey {})
        }
    }

    fn asym_sign(&self, app_name: ApplicationName, op: OpAsymSign) -> Result<ResultAsymSign> {
        let key_name = op.key_name;
        let hash = op.hash;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);

        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let len = hash.len();
        if len > 64 {
            error!("The buffer given to sign is too big. Its length is {} and maximum authorised is 64.", len);
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        let password_context = get_password_context(&*store_handle, key_triple)?;

        let signature = esapi_context
            .sign(
                password_context.context,
                &password_context.auth_value,
                &hash,
            )
            .or_else(|e| {
                error!("Error signing: {}.", e);
                Err(ResponseStatus::PsaErrorHardwareFailure)
            })?;

        Ok(ResultAsymSign {
            signature: signature.signature,
        })
    }

    fn asym_verify(&self, app_name: ApplicationName, op: OpAsymVerify) -> Result<ResultAsymVerify> {
        let key_name = op.key_name;
        let hash = op.hash;
        let signature = op.signature;
        let key_triple = KeyTriple::new(app_name, ProviderID::TpmProvider, key_name);

        let store_handle = self.key_id_store.read().expect("Key store lock poisoned");
        let mut esapi_context = self
            .esapi_context
            .lock()
            .expect("ESAPI Context lock poisoned");

        let len = hash.len();
        if len > 64 {
            error!("The buffer given to sign is too big. Its length is {} and maximum authorised is 64.", len);
            return Err(ResponseStatus::PsaErrorInvalidArgument);
        }

        let signature = Signature {
            scheme: AsymSchemeUnion::RSASSA(TPM2_ALG_SHA256),
            signature,
        };

        let password_context = get_password_context(&*store_handle, key_triple)?;

        esapi_context
            .verify_signature(password_context.context, &hash, signature)
            .or_else(|e| {
                if e.kind() == Some(Tss2ResponseCodeKind::Signature) {
                    error!("The verification failed.");
                    Err(ResponseStatus::PsaErrorInvalidSignature)
                } else {
                    error!("Error verifying: {}.", e);
                    Err(ResponseStatus::PsaErrorHardwareFailure)
                }
            })?;

        Ok(ResultAsymVerify {})
    }
}

impl Drop for TpmProvider {
    fn drop(&mut self) {
        info!("Dropping the TPM Provider.");
    }
}

#[derive(Default)]
pub struct TpmProviderBuilder {
    key_id_store: Option<Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>>,
    tcti: Option<Tcti>,
}

impl TpmProviderBuilder {
    pub fn new() -> TpmProviderBuilder {
        TpmProviderBuilder {
            key_id_store: None,
            tcti: None,
        }
    }

    pub fn with_key_id_store(
        mut self,
        key_id_store: Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>,
    ) -> TpmProviderBuilder {
        self.key_id_store = Some(key_id_store);

        self
    }

    pub fn with_tcti(mut self, tcti: &str) -> TpmProviderBuilder {
        // Convert from a String to the enum.
        self.tcti = match tcti {
            "device" => Some(Tcti::Device),
            "mssim" => Some(Tcti::Mssim),
            _ => {
                error!("The string {} does not match a TCTI device.", tcti);
                None
            }
        };

        self
    }

    pub fn build(self) -> TpmProvider {
        TpmProvider::new(
            self.key_id_store.expect("Missing key ID store."),
            tss_esapi::TransientObjectContext::new(
                self.tcti.expect("Missing TCTI."),
                ROOT_KEY_SIZE,
                ROOT_KEY_AUTH_SIZE,
                &[],
            )
            .expect("Failed to create a new ESAPI transient context"),
        )
        .expect("Failed to initialise TPM Provider")
    }
}
