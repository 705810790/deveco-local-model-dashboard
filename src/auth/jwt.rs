//! JWT 解码与过期判定。
//!
//! 从 deveco-local-model-dashboard/src/auth-login.ts 的 parseJwt / userInfoFromJwt 移植。

use base64::Engine;

#[derive(Debug, Clone, Default)]
pub struct JwtPayload {
    pub exp: Option<i64>,
    pub user_name: Option<String>,
    pub user_id: Option<String>,
    pub site_id: Option<String>,
    /// Raw claims for ad-hoc field access.
    pub raw: serde_json::Value,
}

/// Decode a JWT token (header.payload.signature) and return the payload claims.
/// Does NOT verify the signature — we trust the server-issued token.
///
/// JWT uses URL-safe base64 without padding: `-`/`_` instead of `+`/`/`,
/// no trailing `=`. We convert to standard base64 with padding before decoding,
/// mirroring the TS implementation in auth-login.ts parseJwt().
pub fn parse_jwt(token: &str) -> Result<JwtPayload, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("JWT must have 3 parts".to_string());
    }
    // URL-safe base64 → standard base64 + padding (matches TS parseJwt)
    let b64url = parts[1];
    let b64std: String = b64url
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    let pad = (4 - b64std.len() % 4) % 4;
    let b64_padded = format!("{}{}", b64std, "=".repeat(pad));
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(&b64_padded)
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    let payload: serde_json::Value =
        serde_json::from_slice(&payload_bytes).map_err(|e| format!("JSON parse failed: {e}"))?;

    Ok(JwtPayload {
        exp: payload.get("exp").and_then(|v| v.as_i64()),
        user_name: payload
            .get("userName")
            .or_else(|| payload.get("user_name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        user_id: payload
            .get("userId")
            .or_else(|| payload.get("user_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        site_id: payload
            .get("siteId")
            .or_else(|| payload.get("site_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        raw: payload,
    })
}

/// Check if a JWT token has expired (compares exp claim to current time).
pub fn is_expired(token: &str) -> bool {
    match parse_jwt(token) {
        Ok(payload) => match payload.exp {
            Some(exp) => {
                let now = chrono::Utc::now().timestamp();
                now >= exp
            }
            None => false, // no exp claim → treat as not expired
        },
        Err(_) => true, // unparseable → treat as expired
    }
}

/// Extract user info from a JWT token + refresh result.
/// Matches TS UserInfo: userId, userName, accessToken, refreshToken, jwtToken,
/// countryCode, language, isRealName.
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub user_id: String,
    pub user_name: String,
    pub access_token: String,
    pub refresh_token: String,
    pub jwt_token: String,
    pub country_code: String,
    pub language: String,
    pub is_real_name: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshResult {
    pub access_token: String,
    pub refresh_token: String,
}

pub fn user_info_from_jwt(token: &str, refresh: &RefreshResult) -> Option<UserInfo> {
    let payload = parse_jwt(token).ok()?;
    let info = UserInfo {
        user_id: payload.user_id.unwrap_or_default(),
        user_name: payload.user_name.unwrap_or_default(),
        access_token: refresh.access_token.clone(),
        refresh_token: refresh.refresh_token.clone(),
        jwt_token: token.to_string(),
        country_code: payload
            .raw
            .get("nationalCode")
            .and_then(|v| v.as_str())
            .unwrap_or("CN")
            .to_string(),
        language: "zh_CN".to_string(),
        is_real_name: payload
            .raw
            .get("isRealName")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };
    if info.user_name.is_empty() && info.user_id.is_empty() {
        None
    } else {
        Some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(exp: i64) -> String {
        let header = base64::engine::general_purpose::STANDARD.encode(b"{\"alg\":\"HS256\"}");
        let payload = format!("{{\"exp\":{exp},\"userName\":\"testuser\",\"userId\":\"123\",\"siteId\":\"1\"}}");
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(b"sig");
        format!("{header}.{payload_b64}.{sig}")
    }

    #[test]
    fn parse_valid_jwt() {
        let jwt = make_jwt(9999999999);
        let payload = parse_jwt(&jwt).unwrap();
        assert_eq!(payload.user_name.as_deref(), Some("testuser"));
        assert_eq!(payload.exp, Some(9999999999));
    }

    #[test]
    fn expired_token() {
        let jwt = make_jwt(1);
        assert!(is_expired(&jwt));
    }

    #[test]
    fn not_expired_token() {
        let jwt = make_jwt(9999999999);
        assert!(!is_expired(&jwt));
    }

    #[test]
    fn invalid_token_is_expired() {
        assert!(is_expired("not-a-jwt"));
    }
}
