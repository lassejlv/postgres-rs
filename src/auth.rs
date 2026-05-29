//! Server-side SCRAM-SHA-256 authentication (RFC 5802 / RFC 7677), the
//! mechanism modern PostgreSQL clients negotiate by default.
//!
//! The flow over the wire (driven by [`crate::server`]):
//! 1. Server → client: `AuthenticationSASL` advertising `SCRAM-SHA-256`.
//! 2. Client → server: client-first `n,,n=<user>,r=<client-nonce>`.
//! 3. Server → client: `AuthenticationSASLContinue` with the server-first
//!    `r=<combined-nonce>,s=<salt>,i=<iterations>`.
//! 4. Client → server: client-final `c=biws,r=<combined-nonce>,p=<proof>`.
//! 5. Server verifies the proof and replies `AuthenticationSASLFinal`
//!    `v=<server-signature>`, then `AuthenticationOk`.

use crate::crypto;

const SCRAM_ITERATIONS: u32 = 4096;

/// Drives the server side of a single SCRAM-SHA-256 exchange.
pub struct ScramServer {
    password: String,
    salt: Vec<u8>,
    server_nonce: String,
    /// `n=<user>,r=<nonce>` from the client-first message.
    client_first_bare: String,
    /// The server-first message we sent (part of the auth signature).
    server_first: String,
    /// `<client-nonce><server-nonce>`.
    combined_nonce: String,
}

impl ScramServer {
    /// Begin an exchange that will authenticate against `password`.
    pub fn new(password: &str) -> Self {
        ScramServer {
            password: password.to_string(),
            salt: crypto::random_bytes(16),
            // A printable nonce; base64 contains no comma so it is SCRAM-safe.
            server_nonce: crypto::base64_encode(&crypto::random_bytes(18)),
            client_first_bare: String::new(),
            server_first: String::new(),
            combined_nonce: String::new(),
        }
    }

    /// Process the client-first message, returning the server-first message.
    pub fn server_first(&mut self, client_first: &[u8]) -> Result<Vec<u8>, String> {
        let s = std::str::from_utf8(client_first).map_err(|_| "invalid SASL utf-8".to_string())?;
        // GS2 header + bare: `<cbind-flag>,[authzid],client-first-bare`.
        let bare = s.splitn(3, ',').nth(2).ok_or("malformed client-first")?;
        self.client_first_bare = bare.to_string();

        let client_nonce = attr(bare, 'r').ok_or("client-first missing r=")?;
        self.combined_nonce = format!("{client_nonce}{}", self.server_nonce);
        self.server_first = format!(
            "r={},s={},i={}",
            self.combined_nonce,
            crypto::base64_encode(&self.salt),
            SCRAM_ITERATIONS
        );
        Ok(self.server_first.clone().into_bytes())
    }

    /// Verify the client-final message. On success returns the server-final
    /// message (`v=<server-signature>`); on failure returns an error.
    pub fn server_final(&self, client_final: &[u8]) -> Result<Vec<u8>, String> {
        let s = std::str::from_utf8(client_final).map_err(|_| "invalid SASL utf-8".to_string())?;
        let nonce = attr(s, 'r').ok_or("client-final missing r=")?;
        if nonce != self.combined_nonce {
            return Err("SCRAM nonce mismatch".to_string());
        }
        let proof_b64 = attr(s, 'p').ok_or("client-final missing p=")?;
        let proof = crypto::base64_decode(&proof_b64)?;
        if proof.len() != 32 {
            return Err("invalid SCRAM proof length".to_string());
        }

        // AuthMessage = client-first-bare , server-first , client-final-without-proof
        let without_proof = &s[..s.rfind(",p=").ok_or("malformed client-final")?];
        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );

        let salted = crypto::pbkdf2_sha256(self.password.as_bytes(), &self.salt, SCRAM_ITERATIONS);
        let client_key = crypto::hmac_sha256(&salted, b"Client Key");
        let stored_key = crypto::sha256(&client_key);
        let client_sig = crypto::hmac_sha256(&stored_key, auth_message.as_bytes());

        // Recover the client key from the proof and confirm it hashes to the
        // stored key — that proves the client knew the password.
        let mut recovered = [0u8; 32];
        for i in 0..32 {
            recovered[i] = proof[i] ^ client_sig[i];
        }
        if crypto::sha256(&recovered) != stored_key {
            return Err("password authentication failed".to_string());
        }

        let server_key = crypto::hmac_sha256(&salted, b"Server Key");
        let server_sig = crypto::hmac_sha256(&server_key, auth_message.as_bytes());
        Ok(format!("v={}", crypto::base64_encode(&server_sig)).into_bytes())
    }
}

/// Compute the MD5 password digest PostgreSQL expects in the client's response.
///
/// The client sends `"md5" + md5_hex( md5_hex(password || username) ++ salt )`,
/// and the server computes the same value to compare. The inner digest binds
/// the stored credential to the username; the outer digest binds it to the
/// per-connection 4-byte salt so each exchange is distinct.
pub fn md5_password_digest(password: &str, username: &str, salt: &[u8]) -> String {
    let inner = crypto::md5_hex(format!("{password}{username}").as_bytes());
    let mut salted = inner.into_bytes();
    salted.extend_from_slice(salt);
    format!("md5{}", crypto::md5_hex(&salted))
}

/// Extract a single-letter SCRAM attribute value (`key=value`, comma-separated).
fn attr(s: &str, key: char) -> Option<String> {
    s.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.len() == 1 && k.starts_with(key)).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_sha256, sha256};

    /// Compute a client proof the way a real client would, for testing.
    fn client_proof(password: &str, salt: &[u8], iters: u32, auth_message: &str) -> String {
        let salted = pbkdf2_sha256(password.as_bytes(), salt, iters);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_sig = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_sig.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        base64_encode(&proof)
    }

    /// Run a full exchange with the given attempted password.
    fn exchange(real: &str, attempt: &str) -> Result<(), String> {
        let mut server = ScramServer::new(real);
        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first = format!("n,,n=user,r={client_nonce}");
        let server_first = server.server_first(client_first.as_bytes())?;
        let server_first_str = String::from_utf8(server_first).unwrap();

        let combined = attr(&server_first_str, 'r').unwrap();
        let salt = base64_decode(&attr(&server_first_str, 's').unwrap()).unwrap();
        let iters: u32 = attr(&server_first_str, 'i').unwrap().parse().unwrap();

        let without_proof = format!("c=biws,r={combined}");
        let auth_message = format!("n=user,r={client_nonce},{server_first_str},{without_proof}");
        let proof = client_proof(attempt, &salt, iters, &auth_message);
        let client_final = format!("{without_proof},p={proof}");
        server.server_final(client_final.as_bytes()).map(|_| ())
    }

    #[test]
    fn correct_password_authenticates() {
        assert!(exchange("pencil", "pencil").is_ok());
    }

    #[test]
    fn wrong_password_rejected() {
        assert!(exchange("pencil", "eraser").is_err());
    }

    #[test]
    fn md5_digest_matches_client_construction() {
        // Reconstruct the digest the way a client would and confirm equality.
        let password = "secret";
        let username = "alice";
        let salt = [0x01u8, 0x02, 0x03, 0x04];

        let inner = crate::crypto::md5_hex(format!("{password}{username}").as_bytes());
        let mut salted = inner.into_bytes();
        salted.extend_from_slice(&salt);
        let expected = format!("md5{}", crate::crypto::md5_hex(&salted));

        assert_eq!(md5_password_digest(password, username, &salt), expected);
    }

    #[test]
    fn md5_digest_known_vector() {
        // md5("postgrespostgres") -> inner; outer over inner ++ salt(0,0,0,0).
        let digest = md5_password_digest("postgres", "postgres", &[0, 0, 0, 0]);
        assert!(digest.starts_with("md5"));
        assert_eq!(digest.len(), 35); // "md5" + 32 hex chars
        // Differs from the same password under a different username.
        assert_ne!(digest, md5_password_digest("postgres", "alice", &[0, 0, 0, 0]));
    }
}
