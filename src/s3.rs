// s3.rs - minimal AWS Signature V4 *presigned URL* generation for an
// S3-compatible store (MinIO), crate-root scope (included from main.rs).
//
// We deliberately avoid an S3 SDK: DM attachments only need short-lived
// presigned PUT/GET URLs so clients upload/download bytes directly to MinIO and
// nothing transits this server. Path-style addressing (http://host/bucket/key)
// since MinIO is reached by IP. Payload is signed as UNSIGNED-PAYLOAD and the
// only signed header is `host`, so the client may PUT any body/content-type.

// RFC 3986 percent-encoding. AWS requires encoding everything except the
// unreserved set; `/` in an object key path is kept literal when encode_slash
// is false (so key segments stay readable in the canonical URI).
fn s3_uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// Generate a presigned URL for `method` ("PUT" or "GET") on `object_key`, valid
// for `expires_secs`. Returns the full URL the client hits directly.
fn s3_presign(cfg: &S3Config, method: &str, object_key: &str, expires_secs: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    type HmacSha256 = Hmac<Sha256>;
    let hmac = |key: &[u8], data: &[u8]| -> Vec<u8> {
        let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
        m.update(data);
        m.finalize().into_bytes().to_vec()
    };

    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    let scheme_host = cfg.endpoint.trim_end_matches('/');
    let host = scheme_host
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let service = "s3";
    let scope = format!("{}/{}/{}/aws4_request", date_stamp, cfg.region, service);
    let canonical_uri = format!("/{}/{}", cfg.bucket, s3_uri_encode(object_key, false));
    let credential = format!("{}/{}", cfg.access_key, scope);

    // Canonical (sorted, encoded) query string — the X-Amz-* presign params.
    let mut params: Vec<(&str, String)> = vec![
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
        ("X-Amz-Credential", credential),
        ("X-Amz-Date", amz_date.clone()),
        ("X-Amz-Expires", expires_secs.to_string()),
        ("X-Amz-SignedHeaders", "host".to_string()),
    ];
    params.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_query = params
        .iter()
        .map(|(k, v)| format!("{}={}", s3_uri_encode(k, true), s3_uri_encode(v, true)))
        .collect::<Vec<_>>()
        .join("&");

    let canonical_headers = format!("host:{}\n", host);
    let signed_headers = "host";
    let payload_hash = "UNSIGNED-PAYLOAD";
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_query, canonical_headers, signed_headers, payload_hash
    );
    let cr_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, cr_hash);

    let k_date = hmac(format!("AWS4{}", cfg.secret_key).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, cfg.region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    format!("{}{}?{}&X-Amz-Signature={}", scheme_host, canonical_uri, canonical_query, signature)
}
