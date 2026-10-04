//! AWS Signature Version 4, for S3 (and S3-compatible stores) and AWS KMS.
//! Written here rather than taken from a crate: no AWS crate is in
//! `Cargo.lock`. Checked against the published vectors (the AWS SigV4 test
//! suite's `get-vanilla` and the S3 developer guide's examples).

use ring::{digest, hmac};

use crate::http::Request;
use crate::util::{query_pairs, uri_encode};
use branchyard_support::time::amz_date;

/// SHA-256 of nothing, the payload hash of an empty body.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What a request is signed with.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    /// When temporary credentials run out, in ms; `None` for long-lived.
    pub expires_ms: Option<u64>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({}, ...)", self.access_key)
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// The canonical query string: pairs decoded, then encoded and sorted.
pub fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query_pairs_raw(query)
        .into_iter()
        .map(|(k, v)| (uri_encode(&k, false), uri_encode(&v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Pairs decoded without treating `+` as a space (SigV4 does not).
fn query_pairs_raw(query: &str) -> Vec<(String, String)> {
    let _ = query_pairs;
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (
                branchyard_client::http::decode(k),
                branchyard_client::http::decode(v),
            ),
            None => (branchyard_client::http::decode(p), String::new()),
        })
        .collect()
}

/// The headers signed: every header the request carries, and `host`.
fn canonical_headers(request: &Request) -> (String, String) {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(n, v)| {
            (
                n.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .filter(|(n, _)| n != "authorization" && n != "user-agent" && n != "content-length")
        .collect();
    if !headers.iter().any(|(n, _)| n == "host") {
        headers.push(("host".into(), request.url.host_header()));
    }
    headers.sort();
    let canonical: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (canonical, signed)
}

/// The canonical request and the signed header names.
pub fn canonical_request(request: &Request, payload_hash: &str) -> (String, String) {
    let (headers, signed) = canonical_headers(request);
    (
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            request.method,
            request.url.path,
            canonical_query(&request.url.query),
            headers,
            signed,
            payload_hash
        ),
        signed,
    )
}

/// The string to sign for `canonical` at `date` (`YYYYMMDDTHHMMSSZ`).
pub fn string_to_sign(canonical: &str, date: &str, region: &str, service: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{date}\n{}/{region}/{service}/aws4_request\n{}",
        &date[..8],
        sha256_hex(canonical.as_bytes())
    )
}

/// The signature of `string_to_sign`.
pub fn signature(secret: &str, date: &str, region: &str, service: &str, to_sign: &str) -> String {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), &date.as_bytes()[..8]);
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    hex::encode(hmac_sha256(&k_signing, to_sign.as_bytes()))
}

/// Sign `request` in place: `x-amz-date`, `x-amz-security-token` when the
/// credentials have one, and `Authorization`. `payload_hash` is the body's
/// SHA-256 in hex, sent as `x-amz-content-sha256` when `content_header`.
pub fn sign(
    request: &mut Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
    now_ms: u64,
    payload_hash: &str,
    content_header: bool,
) {
    let date = amz_date(now_ms);
    request
        .headers
        .retain(|(n, _)| !n.eq_ignore_ascii_case("authorization"));
    if request.find("x-amz-date").is_none() {
        request.headers.push(("x-amz-date".into(), date.clone()));
    }
    if content_header && request.find("x-amz-content-sha256").is_none() {
        request
            .headers
            .push(("x-amz-content-sha256".into(), payload_hash.to_owned()));
    }
    if let Some(token) = &credentials.session_token {
        if request.find("x-amz-security-token").is_none() {
            request
                .headers
                .push(("x-amz-security-token".into(), token.clone()));
        }
    }
    let date = request.find("x-amz-date").unwrap_or(&date).to_owned();
    let (canonical, signed) = canonical_request(request, payload_hash);
    let to_sign = string_to_sign(&canonical, &date, region, service);
    let sig = signature(&credentials.secret_key, &date, region, service, &to_sign);
    request.headers.push((
        "Authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{region}/{service}/aws4_request, SignedHeaders={signed}, Signature={sig}",
            credentials.access_key,
            &date[..8]
        ),
    ));
}

/// Check a received request's `Authorization` against `secret`, as a
/// server would: the stand-ins in `crate::testing` use it.
pub fn verify(request: &Request, secret: &str, payload_hash: &str) -> Result<(), String> {
    let auth = request
        .find("authorization")
        .ok_or("no Authorization header")?
        .to_owned();
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("not AWS4-HMAC-SHA256")?;
    let mut credential = "";
    let mut signed = "";
    let mut given = "";
    for part in rest.split(", ") {
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = v;
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = v;
        } else if let Some(v) = part.strip_prefix("Signature=") {
            given = v;
        }
    }
    let scope: Vec<&str> = credential.split('/').collect();
    if scope.len() != 5 {
        return Err(format!("malformed credential scope {credential:?}"));
    }
    let date = request
        .find("x-amz-date")
        .ok_or("no x-amz-date")?
        .to_owned();
    // Rebuild the request with only the signed headers.
    let names: Vec<&str> = signed.split(';').collect();
    let mut only = request.clone();
    only.headers
        .retain(|(n, _)| names.iter().any(|s| s.eq_ignore_ascii_case(n)));
    let (canonical, _) = canonical_request(&only, payload_hash);
    let to_sign = string_to_sign(&canonical, &date, scope[2], scope[3]);
    let want = signature(secret, &date, scope[2], scope[3], &to_sign);
    match want == given {
        true => Ok(()),
        false => Err("signature does not match".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Url;

    fn creds(access: &str, secret: &str) -> Credentials {
        Credentials {
            access_key: access.into(),
            secret_key: secret.into(),
            session_token: None,
            expires_ms: None,
        }
    }

    fn auth_of(request: &Request) -> String {
        request.find("authorization").unwrap().to_owned()
    }

    /// The AWS SigV4 test suite, `get-vanilla`.
    #[test]
    fn get_vanilla() {
        let mut request =
            Request::new("GET", Url::parse("https://example.amazonaws.com/").unwrap())
                .header("X-Amz-Date", "20150830T123600Z");
        sign(
            &mut request,
            &creds("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            "us-east-1",
            "service",
            0,
            EMPTY_SHA256,
            false,
        );
        assert_eq!(
            auth_of(&request),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    const S3_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const S3_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const S3_DATE: u64 = 1_369_353_600_000; // 2013-05-24T00:00:00Z

    fn signature_of(request: &Request) -> String {
        auth_of(request)
            .rsplit("Signature=")
            .next()
            .unwrap()
            .to_owned()
    }

    /// The S3 developer guide: GET Object with a Range header.
    #[test]
    fn s3_get_object() {
        let mut request = Request::new(
            "GET",
            Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap(),
        )
        .header("Range", "bytes=0-9");
        sign(
            &mut request,
            &creds(S3_KEY, S3_SECRET),
            "us-east-1",
            "s3",
            S3_DATE,
            EMPTY_SHA256,
            true,
        );
        assert!(
            auth_of(&request).contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date")
        );
        assert_eq!(
            signature_of(&request),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        verify(&request, S3_SECRET, EMPTY_SHA256).unwrap();
        assert!(verify(&request, "wrong", EMPTY_SHA256).is_err());
    }

    /// The S3 developer guide: PUT Object.
    #[test]
    fn s3_put_object() {
        let body = b"Welcome to Amazon S3.";
        let hash = sha256_hex(body);
        assert_eq!(
            hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let mut request = Request::new(
            "PUT",
            Url::parse("https://examplebucket.s3.amazonaws.com/test%24file.text").unwrap(),
        )
        .header("Date", "Fri, 24 May 2013 00:00:00 GMT")
        .header("x-amz-storage-class", "REDUCED_REDUNDANCY");
        sign(
            &mut request,
            &creds(S3_KEY, S3_SECRET),
            "us-east-1",
            "s3",
            S3_DATE,
            &hash,
            true,
        );
        assert_eq!(
            signature_of(&request),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    /// The S3 developer guide: GET Bucket lifecycle (a query key with no
    /// value) and GET Bucket (list objects, sorted query).
    #[test]
    fn s3_bucket_queries() {
        let mut request = Request::new(
            "GET",
            Url::parse("https://examplebucket.s3.amazonaws.com/?lifecycle").unwrap(),
        );
        sign(
            &mut request,
            &creds(S3_KEY, S3_SECRET),
            "us-east-1",
            "s3",
            S3_DATE,
            EMPTY_SHA256,
            true,
        );
        assert_eq!(
            signature_of(&request),
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
        let mut request = Request::new(
            "GET",
            Url::parse("https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J").unwrap(),
        );
        sign(
            &mut request,
            &creds(S3_KEY, S3_SECRET),
            "us-east-1",
            "s3",
            S3_DATE,
            EMPTY_SHA256,
            true,
        );
        assert_eq!(
            signature_of(&request),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn session_tokens_are_signed() {
        let mut c = creds(S3_KEY, S3_SECRET);
        c.session_token = Some("token".into());
        let mut request = Request::new("GET", Url::parse("http://127.0.0.1:9000/b/k?x=1").unwrap());
        sign(&mut request, &c, "auto", "s3", S3_DATE, EMPTY_SHA256, true);
        assert!(auth_of(&request).contains("x-amz-security-token"));
        assert_eq!(request.find("host"), None, "the client adds Host itself");
        verify(&request, S3_SECRET, EMPTY_SHA256).unwrap();
    }
}
