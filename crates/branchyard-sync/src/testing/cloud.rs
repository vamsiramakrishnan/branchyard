//! Stand-ins for the three KMS APIs (Cloud KMS `encrypt`/`decrypt`, AWS
//! KMS `TrentService.Encrypt`/`Decrypt` with SigV4, Key Vault
//! `wrapkey`/`unwrapkey`) and for the token sources (the GCE metadata
//! server, Google's OAuth token endpoint checking the service account's
//! RS256 JWT, AWS IMDSv2, and an Azure managed identity endpoint).

use branchyard_support::LockExt as _;
use std::sync::{Arc, Mutex};

use ring::signature::{KeyPair, RsaKeyPair, UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

use super::server::{MockRequest, MockResponse, MockServer};
use crate::auth::sigv4;
use crate::kms::{aead_open, aead_seal};
use crate::util::{b64, b64url, unb64, unb64url};

pub const KMS_TOKEN: &str = "kms-standin-token";

/// One server answering all three KMS APIs, sealing with a key of its own.
pub struct MockKms {
    pub server: MockServer,
    /// Calls by API: `gcp:encrypt`, `aws:Decrypt`, `azure:wrapkey`, ...
    pub calls: Arc<Mutex<Vec<String>>>,
}

fn kms_error(status: u16, why: &str) -> MockResponse {
    MockResponse::json(status, &serde_json::json!({"error": why}))
}

impl MockKms {
    pub fn start() -> MockKms {
        let master = crate::util::random_bytes(32).expect("random");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = {
            let calls = calls.clone();
            Arc::new(move |r: &MockRequest| {
                let body: serde_json::Value =
                    serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null);
                let note = |what: &str| calls.lock_recovering("calls").push(what.to_owned());
                // AWS: JSON 1.1 with X-Amz-Target, SigV4 for service kms.
                if let Some(target) = r.header("x-amz-target") {
                    let hash = sigv4::sha256_hex(&r.body);
                    if sigv4::verify(&r.as_request(), super::s3::SECRET_KEY, &hash).is_err() {
                        return kms_error(403, "signature");
                    }
                    let key_id = body["KeyId"].as_str().unwrap_or("");
                    return match target {
                        "TrentService.Encrypt" => {
                            note("aws:Encrypt");
                            let plain =
                                unb64(body["Plaintext"].as_str().unwrap_or("")).unwrap_or_default();
                            let sealed = aead_seal(&master, key_id.as_bytes(), &plain).unwrap();
                            MockResponse::json(
                                200,
                                &serde_json::json!({"CiphertextBlob": b64(&sealed), "KeyId": key_id}),
                            )
                        }
                        "TrentService.Decrypt" => {
                            note("aws:Decrypt");
                            let sealed = unb64(body["CiphertextBlob"].as_str().unwrap_or(""))
                                .unwrap_or_default();
                            match aead_open(&master, key_id.as_bytes(), &sealed) {
                                Ok(plain) => MockResponse::json(
                                    200,
                                    &serde_json::json!({"Plaintext": b64(&plain)}),
                                ),
                                Err(_) => kms_error(400, "InvalidCiphertextException"),
                            }
                        }
                        _ => kms_error(400, "UnknownOperation"),
                    };
                }
                if r.header("authorization") != Some(&format!("Bearer {KMS_TOKEN}")) {
                    return kms_error(401, "unauthenticated");
                }
                let path = r.decoded_path();
                // Cloud KMS: /v1/<key name>:encrypt
                if let Some(rest) = path.strip_prefix("/v1/") {
                    let (name, verb) = rest.rsplit_once(':').unwrap_or((rest, ""));
                    return match verb {
                        "encrypt" => {
                            note("gcp:encrypt");
                            let plain =
                                unb64(body["plaintext"].as_str().unwrap_or("")).unwrap_or_default();
                            let sealed = aead_seal(&master, name.as_bytes(), &plain).unwrap();
                            MockResponse::json(
                                200,
                                &serde_json::json!({"name": name, "ciphertext": b64(&sealed)}),
                            )
                        }
                        "decrypt" => {
                            note("gcp:decrypt");
                            let sealed = unb64(body["ciphertext"].as_str().unwrap_or(""))
                                .unwrap_or_default();
                            match aead_open(&master, name.as_bytes(), &sealed) {
                                Ok(plain) => MockResponse::json(
                                    200,
                                    &serde_json::json!({"plaintext": b64(&plain)}),
                                ),
                                Err(_) => kms_error(400, "Decryption failed"),
                            }
                        }
                        _ => kms_error(404, "unknown method"),
                    };
                }
                // Key Vault: /keys/<name>[/<version>]/wrapkey
                if let Some(rest) = path.strip_prefix("/keys/") {
                    let parts: Vec<&str> = rest.split('/').collect();
                    let name = parts[0];
                    let verb = *parts.last().unwrap_or(&"");
                    let version = if parts.len() == 3 { parts[1] } else { "v1" };
                    if r.param("api-version").is_none() {
                        return kms_error(400, "api-version required");
                    }
                    let aad = format!("{name}/{version}");
                    return match verb {
                        "wrapkey" => {
                            note("azure:wrapkey");
                            let plain =
                                unb64url(body["value"].as_str().unwrap_or("")).unwrap_or_default();
                            let sealed = aead_seal(&master, aad.as_bytes(), &plain).unwrap();
                            MockResponse::json(
                                200,
                                &serde_json::json!({
                                    "kid": format!("https://vault.example/keys/{name}/{version}"),
                                    "value": b64url(&sealed)
                                }),
                            )
                        }
                        "unwrapkey" => {
                            note("azure:unwrapkey");
                            let sealed =
                                unb64url(body["value"].as_str().unwrap_or("")).unwrap_or_default();
                            match aead_open(&master, aad.as_bytes(), &sealed) {
                                Ok(plain) => MockResponse::json(
                                    200,
                                    &serde_json::json!({"value": b64url(&plain)}),
                                ),
                                Err(_) => kms_error(400, "BadParameter"),
                            }
                        }
                        _ => kms_error(404, "unknown operation"),
                    };
                }
                kms_error(404, "not found")
            })
        };
        MockKms {
            server: MockServer::start(handler),
            calls,
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock_recovering("calls").clone()
    }
}

/// Token sources: the GCE metadata server, Google's OAuth token endpoint,
/// AWS IMDSv2 and an Azure identity endpoint, on one server.
pub struct MockMetadata {
    pub server: MockServer,
}

pub const METADATA_TOKEN: &str = "ya29.from-metadata";
pub const OAUTH_TOKEN: &str = "ya29.from-service-account";
pub const IDENTITY_TOKEN: &str = "eyJ.from-identity";
pub const IMDS_SESSION: &str = "imds-session-token";
pub const IDENTITY_HEADER: &str = "identity-secret";

impl MockMetadata {
    /// `account_key` is the PEM private key whose JWTs the OAuth endpoint
    /// accepts.
    pub fn start(account_key: &str) -> MockMetadata {
        let der = crate::auth::google::pem_der(account_key).expect("PEM");
        let public = RsaKeyPair::from_pkcs8(&der)
            .expect("an RSA key")
            .public_key()
            .as_ref()
            .to_vec();
        let handler = Arc::new(
            move |r: &MockRequest| match (r.method.as_str(), r.path.as_str()) {
                ("GET", "/computeMetadata/v1/instance/service-accounts/default/token") => {
                    if r.header("metadata-flavor") != Some("Google") {
                        return MockResponse::new(403);
                    }
                    MockResponse::json(
                        200,
                        &serde_json::json!({"access_token": METADATA_TOKEN, "expires_in": 3599, "token_type": "Bearer"}),
                    )
                }
                ("POST", "/token") => {
                    let form = String::from_utf8_lossy(&r.body).into_owned();
                    let pairs = crate::util::query_pairs(&form);
                    let grant = pairs
                        .iter()
                        .find(|(k, _)| k == "grant_type")
                        .map(|(_, v)| v.as_str());
                    let jwt = pairs
                        .iter()
                        .find(|(k, _)| k == "assertion")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    if grant != Some("urn:ietf:params:oauth:grant-type:jwt-bearer") {
                        return MockResponse::json(
                            400,
                            &serde_json::json!({"error": "unsupported_grant_type"}),
                        );
                    }
                    let (input, sig) = jwt.rsplit_once('.').unwrap_or(("", ""));
                    let ok = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public)
                        .verify(input.as_bytes(), &unb64url(sig).unwrap_or_default())
                        .is_ok();
                    if !ok {
                        return MockResponse::json(
                            400,
                            &serde_json::json!({"error": "invalid_grant"}),
                        );
                    }
                    MockResponse::json(
                        200,
                        &serde_json::json!({"access_token": OAUTH_TOKEN, "expires_in": 3599}),
                    )
                }
                ("PUT", "/latest/api/token") => MockResponse::new(200).body(IMDS_SESSION),
                ("GET", "/latest/meta-data/iam/security-credentials/") => {
                    if r.header("x-aws-ec2-metadata-token") != Some(IMDS_SESSION) {
                        return MockResponse::new(401);
                    }
                    MockResponse::new(200).body("sync-role")
                }
                ("GET", "/latest/meta-data/iam/security-credentials/sync-role") => {
                    if r.header("x-aws-ec2-metadata-token") != Some(IMDS_SESSION) {
                        return MockResponse::new(401);
                    }
                    MockResponse::json(
                        200,
                        &serde_json::json!({
                            "AccessKeyId": super::s3::ACCESS_KEY,
                            "SecretAccessKey": super::s3::SECRET_KEY,
                            "Token": "imds-session",
                            "Expiration": "2099-01-01T00:00:00Z"
                        }),
                    )
                }
                ("GET", "/identity") => {
                    if r.header("x-identity-header") != Some(IDENTITY_HEADER) {
                        return MockResponse::new(401);
                    }
                    let resource = r.param("resource").unwrap_or_default();
                    MockResponse::json(
                        200,
                        &serde_json::json!({
                            "access_token": format!("{IDENTITY_TOKEN}:{resource}"),
                            "expires_on": "4102444800"
                        }),
                    )
                }
                _ => MockResponse::new(404),
            },
        );
        MockMetadata {
            server: MockServer::start(handler),
        }
    }
}
