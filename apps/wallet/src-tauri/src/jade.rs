//! Blockstream Jade: relaying PIN unlocks to Jade's blind oracle (PIN server).
//!
//! When Jade unlocks with a PIN it asks the host to forward an encrypted
//! blob to its PIN server and hand the reply back. The server never sees the
//! PIN, seed or keys. The same relay serves QR unlock (the request arrives as
//! a `jade-pin` QR) and, later, cable/Bluetooth unlock.

use serde_json::{json, Value};
use sessions_core::{Error, Result};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The `http_request` Jade sent, from a full RPC reply message.
fn http_request(message: &Value) -> Result<&Value> {
    message
        .get("result")
        .and_then(|r| r.get("http_request"))
        .ok_or_else(|| Error::Invalid("That isn't Jade's unlock QR. On Jade, choose QR PIN Unlock and scan the first code it shows.".into()))
}

/// First clearnet URL; onion URLs need Tor, which this app doesn't run.
fn pick_url(params: &Value) -> Result<String> {
    params
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|u| !u.is_empty() && !u.contains(".onion"))
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Invalid("Jade didn't give a PIN server address this app can reach".into())
        })
}

/// Forward Jade's request to its PIN server. Returns `(on_reply, body)`.
pub async fn relay(request: &Value) -> Result<(String, Value)> {
    let params = request
        .get("params")
        .ok_or_else(|| Error::Invalid("Jade's unlock request is incomplete".into()))?;
    let on_reply = request
        .get("on-reply")
        .and_then(Value::as_str)
        .unwrap_or("pin")
        .to_owned();
    let url = pick_url(params)?;
    if params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        != "POST"
    {
        return Err(Error::Invalid(
            "Jade asked for an unsupported request".into(),
        ));
    }
    let data = params.get("data").cloned().unwrap_or(Value::Null);
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let res = client
        .post(&url)
        .header("content-type", "application/json")
        .body(data.to_string())
        .send()
        .await
        .map_err(|_| {
            Error::Invalid(
                "Couldn't reach Jade's PIN server. Check your internet connection and try again."
                    .into(),
            )
        })?;
    if !res.status().is_success() {
        return Err(Error::Invalid(format!(
            "Jade's PIN server refused the unlock ({}). Start again on Jade.",
            res.status().as_u16()
        )));
    }
    let body: Value = res
        .json()
        .await
        .map_err(|_| Error::Invalid("Jade's PIN server sent an unexpected reply".into()))?;
    Ok((on_reply, body))
}

/// The RPC message to send back to Jade with the server's reply.
pub fn reply_message(on_reply: &str, body: Value) -> Value {
    let id = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() % 1_000_000)
            .unwrap_or(1)
    );
    json!({ "id": id, "method": on_reply, "params": body })
}

fn from_cbor(bytes: &[u8]) -> Result<Value> {
    ciborium::de::from_reader(bytes)
        .map_err(|_| Error::Invalid("Couldn't read Jade's QR code".into()))
}

fn to_cbor(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("serialising JSON values to CBOR can't fail");
    out
}

/// QR unlock, step 2: take the CBOR from Jade's first `jade-pin` QR, ask the
/// PIN server, and return the CBOR for the `jade-pin` QR Jade scans next.
pub async fn qr_pin_step(request_cbor: &[u8]) -> Result<Vec<u8>> {
    let message = from_cbor(request_cbor)?;
    let (on_reply, body) = relay(http_request(&message)?).await?;
    Ok(to_cbor(&reply_message(&on_reply, body)))
}

/// Tauri command for QR unlock: bytes of Jade's QR in, bytes of the reply QR out.
#[tauri::command]
pub async fn jade_qr_pin(request: Vec<u8>) -> Result<Vec<u8>> {
    qr_pin_step(&request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jade_request_and_builds_reply() {
        let msg = json!({
            "id": "0",
            "result": { "http_request": {
                "params": { "urls": ["https://j8d.io/get_pin", "http://abc.onion/get_pin"], "method": "POST", "accept": "json", "data": { "data": "xyz" } },
                "on-reply": "pin"
            }}
        });
        let back = from_cbor(&to_cbor(&msg)).unwrap();
        let req = http_request(&back).unwrap();
        assert_eq!(pick_url(&req["params"]).unwrap(), "https://j8d.io/get_pin");
        assert_eq!(req["on-reply"], "pin");

        let reply = reply_message("pin", json!({ "data": "server" }));
        assert_eq!(reply["method"], "pin");
        assert_eq!(reply["params"]["data"], "server");
        assert!(!reply["id"].as_str().unwrap().is_empty());
    }

    #[test]
    fn rejects_other_qr_codes() {
        assert!(http_request(&json!({ "id": "1", "result": true })).is_err());
        assert!(pick_url(&json!({ "urls": ["http://x.onion/get_pin"] })).is_err());
    }
}
