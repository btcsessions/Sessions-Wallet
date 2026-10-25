//! The client side of a connection: build requests from a URI and read the
//! responses. Used by the tests; also handy for a "test this connection"
//! self-check in the app.

use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use serde_json::Value;
use sessions_backup::keys::NostrKey;
use sessions_backup::nostr::Event;

use crate::connection::ConnectionUri;
use crate::crypto::{secret_key_from_hex, xonly_from_hex};
use crate::handler::{decrypt, encrypt};
use crate::protocol::{Encryption, ResponseBody};
use crate::{Error, KIND_REQUEST};

pub struct Client {
    sk: SecretKey,
    key: NostrKey,
    wallet_pk: XOnlyPublicKey,
    pub uri: ConnectionUri,
}

impl Client {
    pub fn from_uri(uri: &str) -> Result<Self, Error> {
        let uri = ConnectionUri::parse(uri)?;
        let sk = secret_key_from_hex(uri.secret.expose())?;
        let key = NostrKey::from_secret_bytes(&sk.secret_bytes())
            .map_err(|e| Error::Key(e.to_string()))?;
        let wallet_pk = xonly_from_hex(&uri.wallet_pubkey)?;
        Ok(Client {
            sk,
            key,
            wallet_pk,
            uri,
        })
    }

    pub fn pubkey_hex(&self) -> String {
        self.key.public_hex()
    }

    /// A signed request. `encryption: None` sends no `encryption` tag (legacy
    /// NIP-04 clients).
    pub fn request(
        &self,
        method: &str,
        params: Value,
        encryption: Option<Encryption>,
        created_at: u64,
        extra_tags: Vec<Vec<String>>,
    ) -> Result<Event, Error> {
        let body = serde_json::json!({ "method": method, "params": params }).to_string();
        let enc = encryption.unwrap_or(Encryption::Nip04);
        let content = encrypt(enc, &self.sk, &self.wallet_pk, &body)?;
        let mut tags = vec![vec!["p".to_string(), self.uri.wallet_pubkey.clone()]];
        if let Some(e) = encryption {
            tags.push(vec!["encryption".into(), e.as_str().into()]);
        }
        tags.extend(extra_tags);
        Ok(Event::sign(
            &self.key,
            created_at,
            KIND_REQUEST,
            tags,
            content,
        ))
    }

    /// Decrypt any event from the wallet service (response or notification),
    /// telling NIP-04 from NIP-44 by the payload.
    pub fn decrypt(&self, event: &Event) -> Result<String, Error> {
        if event.pubkey != self.uri.wallet_pubkey || !event.verify() {
            return Err(Error::Invalid("not from this wallet service".into()));
        }
        let enc = if event.content.contains("?iv=") {
            Encryption::Nip04
        } else {
            Encryption::Nip44V2
        };
        decrypt(enc, &self.sk, &self.wallet_pk, &event.content)
    }

    pub fn response(&self, event: &Event) -> Result<ResponseBody, Error> {
        serde_json::from_str(&self.decrypt(event)?).map_err(|e| Error::Invalid(e.to_string()))
    }
}
