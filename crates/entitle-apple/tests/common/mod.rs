//! A test CA shaped like Apple's (root → WWDR intermediate → receipt-signing leaf, with
//! Apple's marker OIDs) and helpers to sign App Store JWS payloads with it.
#![allow(dead_code)]

use app_store_server_library::crypto::CryptoProvider;
use app_store_server_library::crypto::jws::{b64url_encode, encode_compact};
use entitle_apple::AppleAdapter;
use entitle_core::AppleCredentials;
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair, date_time_ymd,
};
use serde_json::{Value, json};

pub const BUNDLE_ID: &str = "com.example.app";
pub const APP_APPLE_ID: i64 = 1234567890;

const WWDR_INTERMEDIATE_OID: &[u64] = &[1, 2, 840, 113635, 100, 6, 2, 1];
const RECEIPT_SIGNER_OID: &[u64] = &[1, 2, 840, 113635, 100, 6, 11, 1];

pub struct TestCa {
    pub root: Vec<u8>,
    intermediate: Vec<u8>,
    leaf: Vec<u8>,
    leaf_key: KeyPair,
}

#[derive(Default)]
pub struct CaOptions {
    /// Leaf `notAfter` (year, month, day).
    pub leaf_expires: Option<(i32, u8, u8)>,
    /// Leave Apple's marker OIDs out.
    pub without_oids: bool,
}

impl TestCa {
    pub fn new() -> Self {
        Self::with(CaOptions::default())
    }

    pub fn with(options: CaOptions) -> Self {
        let marker = |oid: &[u64]| {
            if options.without_oids {
                vec![]
            } else {
                // Apple's marker extensions carry an ASN.1 NULL.
                vec![CustomExtension::from_oid_content(oid, vec![0x05, 0x00])]
            }
        };

        let root_key = KeyPair::generate().unwrap();
        let mut root_params = CertificateParams::new(vec![]).unwrap();
        root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let root = root_params.self_signed(&root_key).unwrap();
        let root_issuer = Issuer::new(root_params, root_key);

        let intermediate_key = KeyPair::generate().unwrap();
        let mut intermediate_params = CertificateParams::new(vec![]).unwrap();
        intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        intermediate_params.custom_extensions = marker(WWDR_INTERMEDIATE_OID);
        let intermediate = intermediate_params
            .signed_by(&intermediate_key, &root_issuer)
            .unwrap();
        let intermediate_issuer = Issuer::new(intermediate_params, intermediate_key);

        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec![]).unwrap();
        leaf_params.custom_extensions = marker(RECEIPT_SIGNER_OID);
        if let Some((y, m, d)) = options.leaf_expires {
            leaf_params.not_before = date_time_ymd(2015, 1, 1);
            leaf_params.not_after = date_time_ymd(y, m, d);
        }
        let leaf = leaf_params
            .signed_by(&leaf_key, &intermediate_issuer)
            .unwrap();

        TestCa {
            root: root.der().to_vec(),
            intermediate: intermediate.der().to_vec(),
            leaf: leaf.der().to_vec(),
            leaf_key,
        }
    }

    /// Compact ES256 JWS with the full x5c chain, like the App Store sends.
    pub fn sign(&self, payload: &Value) -> String {
        let x5c: Vec<String> = [&self.leaf, &self.intermediate, &self.root]
            .iter()
            .map(|der| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(der)
            })
            .collect();
        let header =
            b64url_encode(&serde_json::to_vec(&json!({"alg": "ES256", "x5c": x5c})).unwrap());
        let body = b64url_encode(&serde_json::to_vec(payload).unwrap());
        let key = CryptoProvider::default_provider()
            .p256_signing
            .private_key(&self.leaf_key.serialize_pem())
            .unwrap();
        let (raw, _) = key
            .signature(format!("{header}.{body}").as_bytes())
            .unwrap();
        encode_compact(&header, &body, &raw)
    }

    /// A fixture's `data.transaction` / `data.renewal` become `signedTransactionInfo` /
    /// `signedRenewalInfo`, then the whole notification is signed.
    pub fn sign_notification(&self, fixture: &Value) -> String {
        let mut payload = fixture.clone();
        if let Some(data) = payload.get_mut("data").and_then(Value::as_object_mut) {
            if let Some(tx) = data.remove("transaction") {
                data.insert("signedTransactionInfo".into(), self.sign(&tx).into());
            }
            if let Some(renewal) = data.remove("renewal") {
                data.insert("signedRenewalInfo".into(), self.sign(&renewal).into());
            }
        }
        self.sign(&payload)
    }

    pub fn adapter(&self, production_url: &str, sandbox_url: &str) -> AppleAdapter {
        AppleAdapter::with_config(vec![self.root.clone()], production_url, sandbox_url)
    }
}

pub fn credentials() -> AppleCredentials {
    AppleCredentials {
        issuer_id: "57246542-96fe-1a63-e053-0824d011072a".into(),
        key_id: "2X9R4HXF34".into(),
        private_key: KeyPair::generate().unwrap().serialize_pem(),
        bundle_id: BUNDLE_ID.into(),
        app_apple_id: Some(APP_APPLE_ID),
    }
}
