//! Signatures used by the rendezvous protocol, shared by client and server.
//!
//! Every signed message starts with its own label (domain separation), so a
//! signature for one purpose can never be reused for another.

use kanodesk_crypto::{DeviceIdentity, DevicePublicKey};
use kanodesk_proto::v1::{AdminAuth, ChangeId, DeviceRecord, Register, ServerHello, SignedRecord};
use prost::Message;

use crate::id::DeviceId;
use crate::{ClientError, unix_now};

/// Label of the server's proof of identity.
pub const SERVER_HELLO_LABEL: &[u8] = b"kanodesk/v1 rendezvous server";
/// Label of a device's registration proof.
pub const REGISTER_LABEL: &[u8] = b"kanodesk/v1 rendezvous register";
/// Label of a device's request to change its ID.
pub const CHANGE_ID_LABEL: &[u8] = b"kanodesk/v1 rendezvous change id";
/// Label of an operator's proof of holding the server key.
pub const ADMIN_LABEL: &[u8] = b"kanodesk/v1 rendezvous admin";
/// Label of a signed device record.
pub const RECORD_LABEL: &[u8] = b"kanodesk/v1 device record";

fn message(label: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut out = label.to_vec();
    for p in parts {
        out.extend_from_slice(&(p.len() as u32).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

fn key_from(bytes: &[u8]) -> Option<DevicePublicKey> {
    DevicePublicKey::from_bytes(bytes.try_into().ok()?).ok()
}

fn sig_from(bytes: &[u8]) -> Option<[u8; 64]> {
    bytes.try_into().ok()
}

/// Server: proves it holds `server`'s key on this TLS session.
pub fn sign_server_hello(server: &DeviceIdentity, channel_binding: &[u8]) -> ServerHello {
    ServerHello {
        server_key: server.public_key().as_bytes().to_vec(),
        signature: server
            .sign(&message(SERVER_HELLO_LABEL, &[channel_binding]))
            .to_vec(),
    }
}

/// Client: checks the server's proof against the pinned key.
pub fn verify_server_hello(
    hello: &ServerHello,
    pinned: &DevicePublicKey,
    channel_binding: &[u8],
) -> Result<(), ClientError> {
    let ok = key_from(&hello.server_key).is_some_and(|k| k == *pinned)
        && sig_from(&hello.signature).is_some_and(|sig| {
            pinned.verify(&message(SERVER_HELLO_LABEL, &[channel_binding]), &sig)
        });
    if ok {
        Ok(())
    } else {
        Err(ClientError::ServerNotTrusted)
    }
}

/// Device: builds a registration that proves it owns its key on this TLS session.
pub fn sign_register(
    identity: &DeviceIdentity,
    channel_binding: &[u8],
    id: Option<DeviceId>,
) -> Register {
    let id = id.map(|i| i.to_string()).unwrap_or_default();
    Register {
        device_key: identity.public_key().as_bytes().to_vec(),
        signature: identity
            .sign(&message(REGISTER_LABEL, &[channel_binding, id.as_bytes()]))
            .to_vec(),
        id,
    }
}

/// Why a registration is not acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterProblem {
    /// Key or signature malformed, or the signature does not verify.
    BadSignature,
    /// `id` is not a 9-digit ID.
    InvalidId,
}

/// Server: checks a registration. Returns the device key and requested ID.
pub fn verify_register(
    register: &Register,
    channel_binding: &[u8],
) -> Result<(DevicePublicKey, Option<DeviceId>), RegisterProblem> {
    let key = key_from(&register.device_key).ok_or(RegisterProblem::BadSignature)?;
    let sig = sig_from(&register.signature).ok_or(RegisterProblem::BadSignature)?;
    if !key.verify(
        &message(REGISTER_LABEL, &[channel_binding, register.id.as_bytes()]),
        &sig,
    ) {
        return Err(RegisterProblem::BadSignature);
    }
    let id = if register.id.is_empty() {
        None
    } else {
        Some(
            register
                .id
                .parse::<DeviceId>()
                .map_err(|_| RegisterProblem::InvalidId)?,
        )
    };
    Ok((key, id))
}

/// Operator: proves it holds the server's signing key on this TLS session.
pub fn sign_admin_auth(server: &DeviceIdentity, channel_binding: &[u8]) -> AdminAuth {
    AdminAuth {
        signature: server
            .sign(&message(ADMIN_LABEL, &[channel_binding]))
            .to_vec(),
    }
}

/// Server: checks an operator's proof against its own key.
pub fn verify_admin_auth(
    auth: &AdminAuth,
    server_key: &DevicePublicKey,
    channel_binding: &[u8],
) -> bool {
    sig_from(&auth.signature)
        .is_some_and(|sig| server_key.verify(&message(ADMIN_LABEL, &[channel_binding]), &sig))
}

/// Device: asks to move to `new_id`, proving it owns `identity`'s key.
pub fn sign_change_id(
    identity: &DeviceIdentity,
    channel_binding: &[u8],
    new_id: DeviceId,
) -> ChangeId {
    let new_id = new_id.to_string();
    ChangeId {
        signature: identity
            .sign(&message(
                CHANGE_ID_LABEL,
                &[channel_binding, new_id.as_bytes()],
            ))
            .to_vec(),
        new_id,
    }
}

/// Server: checks a change-ID request against the key registered on this
/// session. Returns the requested ID.
pub fn verify_change_id(
    change: &ChangeId,
    key: &DevicePublicKey,
    channel_binding: &[u8],
) -> Result<DeviceId, RegisterProblem> {
    let sig = sig_from(&change.signature).ok_or(RegisterProblem::BadSignature)?;
    if !key.verify(
        &message(
            CHANGE_ID_LABEL,
            &[channel_binding, change.new_id.as_bytes()],
        ),
        &sig,
    ) {
        return Err(RegisterProblem::BadSignature);
    }
    change
        .new_id
        .parse()
        .map_err(|_| RegisterProblem::InvalidId)
}

/// A device record whose server signature has been checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedRecord {
    /// The device's ID.
    pub id: DeviceId,
    /// Its ed25519 public key: use it as the Noise IK remote key.
    pub key: DevicePublicKey,
    /// Unix seconds when the server signed it.
    pub issued_at: u64,
}

/// Server: signs `(id, key)` as of now.
pub fn sign_record(server: &DeviceIdentity, id: DeviceId, key: &DevicePublicKey) -> SignedRecord {
    sign_record_at(server, id, key, unix_now())
}

/// Server: signs `(id, key)` with an explicit timestamp.
pub fn sign_record_at(
    server: &DeviceIdentity,
    id: DeviceId,
    key: &DevicePublicKey,
    issued_at: u64,
) -> SignedRecord {
    let record = DeviceRecord {
        id: id.to_string(),
        device_key: key.as_bytes().to_vec(),
        issued_at,
    }
    .encode_to_vec();
    let signature = server.sign(&message(RECORD_LABEL, &[&record])).to_vec();
    SignedRecord { record, signature }
}

/// Client: checks a record's signature with the pinned server key.
pub fn verify_record(
    signed: &SignedRecord,
    server_key: &DevicePublicKey,
) -> Result<VerifiedRecord, ClientError> {
    let sig = sig_from(&signed.signature).ok_or(ClientError::BadRecord("signature length"))?;
    if !server_key.verify(&message(RECORD_LABEL, &[&signed.record]), &sig) {
        return Err(ClientError::BadRecord("signature does not verify"));
    }
    let record = DeviceRecord::decode(signed.record.as_slice())
        .map_err(|_| ClientError::BadRecord("undecodable record"))?;
    Ok(VerifiedRecord {
        id: record
            .id
            .parse()
            .map_err(|_| ClientError::BadRecord("invalid ID"))?,
        key: key_from(&record.device_key).ok_or(ClientError::BadRecord("invalid key"))?,
        issued_at: record.issued_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINDING: &[u8] = &[5u8; 32];

    #[test]
    fn server_hello_verifies_only_with_the_pinned_key_and_binding() {
        let server = DeviceIdentity::generate().unwrap();
        let hello = sign_server_hello(&server, BINDING);
        assert!(verify_server_hello(&hello, &server.public_key(), BINDING).is_ok());

        let other = DeviceIdentity::generate().unwrap();
        assert!(verify_server_hello(&hello, &other.public_key(), BINDING).is_err());
        assert!(verify_server_hello(&hello, &server.public_key(), &[6u8; 32]).is_err());
        // An impostor that copies the real key but signs with its own.
        let mut forged = sign_server_hello(&other, BINDING);
        forged.server_key = server.public_key().as_bytes().to_vec();
        assert!(verify_server_hello(&forged, &server.public_key(), BINDING).is_err());
    }

    #[test]
    fn registration_proves_key_ownership() {
        let device = DeviceIdentity::generate().unwrap();
        let id: DeviceId = "123456789".parse().unwrap();
        let reg = sign_register(&device, BINDING, Some(id));
        assert_eq!(
            verify_register(&reg, BINDING).unwrap(),
            (device.public_key(), Some(id))
        );
        assert_eq!(
            verify_register(&sign_register(&device, BINDING, None), BINDING).unwrap(),
            (device.public_key(), None)
        );

        // Replayed on another connection.
        assert_eq!(
            verify_register(&reg, &[0u8; 32]),
            Err(RegisterProblem::BadSignature)
        );
        // Someone else's key, or a changed ID.
        let mut stolen = reg.clone();
        stolen.device_key = DeviceIdentity::generate()
            .unwrap()
            .public_key()
            .as_bytes()
            .to_vec();
        assert_eq!(
            verify_register(&stolen, BINDING),
            Err(RegisterProblem::BadSignature)
        );
        let mut changed = reg.clone();
        changed.id = "987654321".into();
        assert_eq!(
            verify_register(&changed, BINDING),
            Err(RegisterProblem::BadSignature)
        );
        // Correctly signed but not a valid ID.
        let mut bad = Register {
            id: "12".into(),
            ..reg
        };
        bad.signature = device
            .sign(&message(REGISTER_LABEL, &[BINDING, b"12"]))
            .to_vec();
        assert_eq!(
            verify_register(&bad, BINDING),
            Err(RegisterProblem::InvalidId)
        );
    }

    #[test]
    fn change_id_is_bound_to_key_binding_and_id() {
        let device = DeviceIdentity::generate().unwrap();
        let id: DeviceId = "222333444".parse().unwrap();
        let change = sign_change_id(&device, BINDING, id);
        assert_eq!(
            verify_change_id(&change, &device.public_key(), BINDING),
            Ok(id)
        );
        let other = DeviceIdentity::generate().unwrap().public_key();
        assert!(verify_change_id(&change, &other, BINDING).is_err());
        assert!(verify_change_id(&change, &device.public_key(), &[0u8; 32]).is_err());
        let mut retargeted = change.clone();
        retargeted.new_id = "222333445".into();
        assert!(verify_change_id(&retargeted, &device.public_key(), BINDING).is_err());
        // A registration signature cannot be reused as a change request.
        let reg = sign_register(&device, BINDING, Some(id));
        let confused = ChangeId {
            new_id: id.to_string(),
            signature: reg.signature,
        };
        assert!(verify_change_id(&confused, &device.public_key(), BINDING).is_err());
    }

    #[test]
    fn admin_auth_needs_the_server_key_and_binding() {
        let server = DeviceIdentity::generate().unwrap();
        let auth = sign_admin_auth(&server, BINDING);
        assert!(verify_admin_auth(&auth, &server.public_key(), BINDING));
        assert!(!verify_admin_auth(&auth, &server.public_key(), &[1u8; 32]));
        let device = DeviceIdentity::generate().unwrap();
        let forged = sign_admin_auth(&device, BINDING);
        assert!(!verify_admin_auth(&forged, &server.public_key(), BINDING));
        // A server hello (same key, same binding) is not an admin proof.
        let hello = sign_server_hello(&server, BINDING);
        let confused = AdminAuth {
            signature: hello.signature,
        };
        assert!(!verify_admin_auth(&confused, &server.public_key(), BINDING));
    }

    #[test]
    fn records_verify_and_detect_tampering() {
        let server = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();
        let id: DeviceId = "555444333".parse().unwrap();
        let signed = sign_record_at(&server, id, &device.public_key(), 1_700_000_000);
        let rec = verify_record(&signed, &server.public_key()).unwrap();
        assert_eq!(
            (rec.id, rec.key, rec.issued_at),
            (id, device.public_key(), 1_700_000_000)
        );

        let mut tampered = signed.clone();
        let last = tampered.record.len() - 1;
        tampered.record[last] ^= 1;
        assert!(verify_record(&tampered, &server.public_key()).is_err());
        let rogue = DeviceIdentity::generate().unwrap();
        assert!(verify_record(&signed, &rogue.public_key()).is_err());
        // A server-hello signature cannot pass as a record signature.
        let hello = sign_server_hello(&server, &signed.record);
        let confused = SignedRecord {
            record: signed.record.clone(),
            signature: hello.signature,
        };
        assert!(verify_record(&confused, &server.public_key()).is_err());
    }
}
