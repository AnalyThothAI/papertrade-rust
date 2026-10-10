//! EIP-712 intents: what the relayer takes at `POST /intents`.

use alloy::primitives::{Address, B256, Signature, U256, keccak256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{Eip712Domain, SolStruct, eip712_domain};
use rand::Rng;
use serde_json::{Map, Value, json};

use super::deployment::{CHAIN_ID, EXCHANGE};

sol! {
    #[derive(Debug, PartialEq, Eq)]
    struct RegisterSessionKey {
        address user;
        address key;
        uint64 expiry;
        address feeAddress;
        uint256 nonce;
        uint64 deadline;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct OpenPosition {
        address user;
        uint32 instrumentId;
        bool isLong;
        /// Margin in 18-decimal USD.
        uint128 size;
        uint32 leverage;
        bool useDebt;
        uint256 nonce;
        uint64 deadline;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Close {
        address user;
        uint256[] positionIds;
        uint256 nonce;
        uint64 deadline;
    }
}

/// EIP-712 domain of the Exchange contract. The name really is "String".
pub fn exchange_domain() -> Eip712Domain {
    eip712_domain! {
        name: "String",
        version: "1",
        chain_id: CHAIN_ID,
        verifying_contract: EXCHANGE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    RegisterSessionKey,
    OpenPosition,
    ClosePositions,
}

impl Action {
    /// The action name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RegisterSessionKey => "registerSessionKey",
            Self::OpenPosition => "openPosition",
            Self::ClosePositions => "closePositions",
        }
    }
}

/// An intent ready for `POST /intents`: the relayer takes `{ action, signed }`.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedIntent {
    pub action: Action,
    /// Stable identity of this exact signed request; the relayer reports outcomes under it.
    pub intent_id: B256,
    pub user: Address,
    /// Unix seconds after which the contract rejects the intent.
    pub deadline: u64,
    pub signed: Value,
}

impl SignedIntent {
    /// The request body, exactly as it is sent and journaled.
    pub fn body(&self) -> Value {
        json!({ "action": self.action.as_str(), "signed": self.signed })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("signing failed: {0}")]
    Signer(#[from] alloy::signers::Error),
    #[error("signature does not recover: {0}")]
    Recover(#[from] alloy::primitives::SignatureError),
    #[error("signed by {actual}, expected {expected}")]
    WrongSigner { actual: Address, expected: Address },
}

pub fn sign_open_position(
    signer: &PrivateKeySigner,
    message: &OpenPosition,
) -> Result<SignedIntent, SealError> {
    let digest = message.eip712_signing_hash(&exchange_domain());
    let fields = json!({
        "user": lower(message.user),
        "instrumentId": message.instrumentId.to_string(),
        "isLong": message.isLong,
        "size": message.size.to_string(),
        "leverage": message.leverage.to_string(),
        "useDebt": message.useDebt,
        "nonce": message.nonce.to_string(),
        "deadline": message.deadline.to_string(),
    });
    let signature = signer.sign_hash_sync(&digest)?;
    seal(Action::OpenPosition, fields, message.user, message.deadline, digest, signature, signer.address())
}

pub fn sign_close(signer: &PrivateKeySigner, message: &Close) -> Result<SignedIntent, SealError> {
    let digest = message.eip712_signing_hash(&exchange_domain());
    let ids: Vec<String> = message.positionIds.iter().map(U256::to_string).collect();
    let fields = json!({
        "user": lower(message.user),
        "positionIds": ids,
        "nonce": message.nonce.to_string(),
        "deadline": message.deadline.to_string(),
    });
    let signature = signer.sign_hash_sync(&digest)?;
    seal(Action::ClosePositions, fields, message.user, message.deadline, digest, signature, signer.address())
}

/// Seals a registration signed elsewhere; the master key never has to touch this process.
pub fn seal_register_session_key(
    message: &RegisterSessionKey,
    signature: Signature,
) -> Result<SignedIntent, SealError> {
    let digest = message.eip712_signing_hash(&exchange_domain());
    let fields = json!({
        "user": lower(message.user),
        "key": lower(message.key),
        "expiry": message.expiry.to_string(),
        "feeAddress": lower(message.feeAddress),
        "nonce": message.nonce.to_string(),
        "deadline": message.deadline.to_string(),
    });
    seal(Action::RegisterSessionKey, fields, message.user, message.deadline, digest, signature, message.user)
}

/// The registration as the JSON document wallets take for `eth_signTypedData_v4`.
pub fn register_session_key_document(message: &RegisterSessionKey) -> Value {
    json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" },
                { "name": "verifyingContract", "type": "address" },
            ],
            "RegisterSessionKey": [
                { "name": "user", "type": "address" },
                { "name": "key", "type": "address" },
                { "name": "expiry", "type": "uint64" },
                { "name": "feeAddress", "type": "address" },
                { "name": "nonce", "type": "uint256" },
                { "name": "deadline", "type": "uint64" },
            ],
        },
        "primaryType": "RegisterSessionKey",
        "domain": {
            "name": "String",
            "version": "1",
            "chainId": CHAIN_ID.to_string(),
            "verifyingContract": lower(EXCHANGE),
        },
        "message": {
            "user": lower(message.user),
            "key": lower(message.key),
            "expiry": message.expiry.to_string(),
            "feeAddress": lower(message.feeAddress),
            "nonce": message.nonce.to_string(),
            "deadline": message.deadline.to_string(),
        },
    })
}

/// Reads back the message of a document written by [`register_session_key_document`].
pub fn register_session_key_from_document(document: &Value) -> Option<RegisterSessionKey> {
    let message = document.get("message")?;
    let text = |field: &str| message.get(field)?.as_str();
    Some(RegisterSessionKey {
        user: text("user")?.parse().ok()?,
        key: text("key")?.parse().ok()?,
        expiry: text("expiry")?.parse().ok()?,
        feeAddress: text("feeAddress")?.parse().ok()?,
        nonce: text("nonce")?.parse().ok()?,
        deadline: text("deadline")?.parse().ok()?,
    })
}

/// Nonces are client-chosen and unordered. Like the official client, start at a
/// random 24-bit word and count up, so a fresh process cannot collide with the
/// nonces of an earlier one.
#[derive(Debug)]
pub struct NonceSequence(u64);

impl NonceSequence {
    pub fn random() -> Self {
        Self(rand::rng().random_range(1..1u64 << 24) * 256)
    }

    pub fn allocate(&mut self) -> U256 {
        let nonce = self.0;
        self.0 += 1;
        U256::from(nonce)
    }
}

fn seal(
    action: Action,
    fields: Value,
    user: Address,
    deadline: u64,
    digest: B256,
    signature: Signature,
    expected_signer: Address,
) -> Result<SignedIntent, SealError> {
    let signer = signature.recover_address_from_prehash(&digest)?;
    if signer != expected_signer {
        return Err(SealError::WrongSigner { actual: signer, expected: expected_signer });
    }
    let Value::Object(mut signed) = fields else { unreachable!("intent fields are an object") };
    extend_with_signature(&mut signed, &signature);
    let tag = keccak256("papertrade:intent-id:v2");
    let intent_id = keccak256([tag.as_slice(), digest.as_slice(), signer.as_slice()].concat());
    Ok(SignedIntent { action, intent_id, user, deadline, signed: Value::Object(signed) })
}

/// The relayer takes the signature split, with `v` as a decimal string.
fn extend_with_signature(signed: &mut Map<String, Value>, signature: &Signature) {
    signed.insert("v".into(), Value::String((27 + u8::from(signature.v())).to_string()));
    signed.insert("r".into(), Value::String(format!("{:#066x}", signature.r())));
    signed.insert("s".into(), Value::String(format!("{:#066x}", signature.s())));
}

/// Addresses travel lowercase.
fn lower(address: Address) -> String {
    format!("{address:#x}")
}

#[cfg(test)]
mod tests {
    use alloy::dyn_abi::TypedData;
    use alloy::primitives::address;

    use super::*;

    // Well-known Anvil development keys; they hold nothing anywhere.
    const SESSION_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const MASTER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const MASTER: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    /// Signed by the TypeScript implementation, whose registration encoding the
    /// production relayer accepted. The two implementations must agree byte for byte.
    fn golden(name: &str) -> (B256, Value) {
        let all: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/intents-golden.json")).unwrap();
        let entry = &all[name];
        (entry["intentId"].as_str().unwrap().parse().unwrap(), entry["body"].clone())
    }

    fn signer(key: &str) -> PrivateKeySigner {
        key.parse().unwrap()
    }

    fn registration() -> RegisterSessionKey {
        RegisterSessionKey {
            user: MASTER,
            key: signer(SESSION_KEY).address(),
            expiry: 1_794_212_392,
            feeAddress: Address::ZERO,
            nonce: U256::from(256),
            deadline: 1_791_625_000,
        }
    }

    #[test]
    fn open_matches_the_reference_implementation() {
        let message = OpenPosition {
            user: MASTER,
            instrumentId: 0,
            isLong: true,
            size: 500 * 10u128.pow(18),
            leverage: 20,
            useDebt: false,
            nonce: U256::from(3_912_476_160u64),
            deadline: 1_791_621_690,
        };
        let intent = sign_open_position(&signer(SESSION_KEY), &message).unwrap();
        let (intent_id, body) = golden("open");
        assert_eq!(intent.body(), body);
        assert_eq!(intent.intent_id, intent_id);
        assert_eq!((intent.user, intent.deadline), (MASTER, 1_791_621_690));
    }

    #[test]
    fn close_matches_the_reference_implementation() {
        let message = Close {
            user: MASTER,
            positionIds: vec![U256::from(42), U256::from(1u128 << 64)],
            nonce: U256::from(3_912_476_161u64),
            deadline: 1_791_621_690,
        };
        let intent = sign_close(&signer(SESSION_KEY), &message).unwrap();
        let (intent_id, body) = golden("close");
        assert_eq!(intent.body(), body);
        assert_eq!(intent.intent_id, intent_id);
    }

    #[test]
    fn registration_matches_the_reference_implementation() {
        let message = registration();
        let digest = message.eip712_signing_hash(&exchange_domain());
        let signature = signer(MASTER_KEY).sign_hash_sync(&digest).unwrap();
        let intent = seal_register_session_key(&message, signature).unwrap();
        let (intent_id, body) = golden("register");
        assert_eq!(intent.body(), body);
        assert_eq!(intent.intent_id, intent_id);
    }

    #[test]
    fn a_registration_signed_by_any_other_key_is_refused() {
        let message = registration();
        let digest = message.eip712_signing_hash(&exchange_domain());
        let signature = signer(SESSION_KEY).sign_hash_sync(&digest).unwrap();
        let error = seal_register_session_key(&message, signature).unwrap_err();
        assert!(matches!(error, SealError::WrongSigner { expected, .. } if expected == MASTER));
    }

    #[test]
    fn the_registration_document_is_exactly_what_gets_sealed() {
        let message = registration();
        let document = register_session_key_document(&message);
        // A wallet hashing the document signs the same digest we seal.
        let typed: TypedData = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(typed.eip712_signing_hash().unwrap(), message.eip712_signing_hash(&exchange_domain()));
        assert_eq!(register_session_key_from_document(&document), Some(message));
    }

    #[test]
    fn a_changed_request_is_a_different_intent() {
        let open = |nonce: u64| {
            let message = OpenPosition {
                user: MASTER,
                instrumentId: 0,
                isLong: true,
                size: 1,
                leverage: 20,
                useDebt: false,
                nonce: U256::from(nonce),
                deadline: 1,
            };
            sign_open_position(&signer(SESSION_KEY), &message).unwrap().intent_id
        };
        assert_eq!(open(7), open(7));
        assert_ne!(open(7), open(8));
    }

    #[test]
    fn nonces_count_up_from_a_random_word_boundary() {
        let mut nonces = NonceSequence::random();
        let first = nonces.allocate();
        assert_eq!(first % U256::from(256), U256::ZERO);
        assert!(first >= U256::from(256));
        assert_eq!(nonces.allocate(), first + U256::from(1));
        let starts: std::collections::BTreeSet<U256> =
            (0..20).map(|_| NonceSequence::random().allocate()).collect();
        assert!(starts.len() > 1);
    }
}
