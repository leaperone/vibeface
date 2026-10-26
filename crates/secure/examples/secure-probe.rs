use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use vibeface_secure::{Channel, Handshake, Identity};

type Result<T> = std::result::Result<T, vibeface_secure::Error>;

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}").into())
}
fn bytes(input: &Value) -> Result<Vec<u8>> {
    hex::decode(text(input, "data")?).map_err(|_| "invalid hexadecimal data".into())
}

fn main() -> Result<()> {
    let identity = Identity::generate()?;
    let mut handshake = None::<Handshake>;
    let mut channel = None::<Channel>;
    for line in io::stdin().lock().lines() {
        let result = (|| -> Result<Value> {
            let input: Value = serde_json::from_str(&line.map_err(|_| "probe input failed")?)
                .map_err(|_| "invalid probe JSON")?;
            Ok(match text(&input, "op")? {
                "identity" => json!({"device_id":identity.device_id()}),
                "recovery_seal" => json!(hex::encode(vibeface_secure::recovery_seal(
                    &bytes(&input)?,
                    text(&input, "password")?
                )?)),
                "recovery_open" => json!(hex::encode(vibeface_secure::recovery_open(
                    &bytes(&input)?,
                    text(&input, "password")?
                )?)),
                "sign" => json!(identity.sign(text(&input, "domain")?, &bytes(&input)?)?),
                "handshake" => {
                    handshake = Some(Handshake::signed(
                        input
                            .get("initiator")
                            .and_then(Value::as_bool)
                            .ok_or("missing initiator")?,
                        &identity,
                        input
                            .get("expected_device_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        text(&input, "context")?.as_bytes(),
                    )?);
                    Value::Null
                }
                "write" => json!(hex::encode(
                    handshake
                        .as_mut()
                        .ok_or("no handshake")?
                        .write(&bytes(&input)?)?
                )),
                "read" => json!(hex::encode(
                    handshake
                        .as_mut()
                        .ok_or("no handshake")?
                        .read(&bytes(&input)?)?
                )),
                "remote" => json!(handshake.as_ref().ok_or("no handshake")?.remote_id()),
                "finish" => {
                    channel = Some(handshake.as_mut().ok_or("no handshake")?.finish()?);
                    Value::Null
                }
                "seal" => json!(hex::encode(
                    channel
                        .as_mut()
                        .ok_or("no channel")?
                        .seal(&bytes(&input)?)?
                )),
                "open" => json!(hex::encode(
                    channel
                        .as_mut()
                        .ok_or("no channel")?
                        .open(&bytes(&input)?)?
                )),
                "seal_message" => json!(
                    channel
                        .as_mut()
                        .ok_or("no channel")?
                        .seal_message(&bytes(&input)?)?
                        .iter()
                        .map(hex::encode)
                        .collect::<Vec<_>>()
                ),
                "open_record" => json!(
                    channel
                        .as_mut()
                        .ok_or("no channel")?
                        .open_record(&bytes(&input)?)?
                        .map(hex::encode)
                ),
                _ => return Err("unknown probe operation".into()),
            })
        })();
        let response = match result {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        };
        println!("{response}");
        io::stdout().flush().map_err(|_| "probe output failed")?;
    }
    Ok(())
}
