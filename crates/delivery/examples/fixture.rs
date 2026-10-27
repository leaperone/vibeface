mod support;

use serde_json::json;
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
use vibeface_delivery::*;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: fixture public|input <length> <count> [image_hex] [relation] [index] [private_output]".into());
    }
    let length: usize = args[1].parse()?;
    let count: usize = args[2].parse()?;
    if length > MAX_WINDOW_BYTES || count == 0 || count > MAX_WINDOWS {
        return Err("fixture size exceeds protocol caps".into());
    }
    let image: Word = if let Some(value) = args.get(3) {
        hex::decode(value.trim_start_matches("0x"))?
            .try_into()
            .map_err(|_| "image length")?
    } else {
        [0x91; 32]
    };
    let profile_path = args.get(if args[0] == "input" { 7 } else { 4 });
    let fixture = if let Some(path) = profile_path {
        let profile: ProofProfile = serde_json::from_slice(&std::fs::read(path)?)?;
        profile.validate()?;
        if profile.guest_image_id != image {
            return Err("profile image mismatch".into());
        }
        if count > profile.max_windows as usize
            || length > profile.max_window_bytes as usize
            || (count * length) as u64 > profile.max_total_bytes
        {
            return Err("fixture exceeds supplied profile".into());
        }
        support::fixture_with_profile(&vec![length; count], profile)
    } else {
        support::fixture(&vec![length; count], image)
    };
    if args[0] == "input" {
        let relation = Relation::try_from(args.get(4).ok_or("missing relation")?.parse::<u8>()?)?;
        let index: u16 = args.get(5).ok_or("missing index")?.parse()?;
        if index as usize >= count {
            return Err("fixture index".into());
        }
        let output = args.get(6).ok_or("missing private output path")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output)?;
        serde_json::to_writer(&mut file, &fixture.input(relation, index))?;
        file.flush()?;
        file.sync_all()?;
    } else if args[0] == "public" {
        let windows: Vec<_> = (0..count).map(|index| {
            let relations: Vec<_> = [Relation::Source, Relation::Hold, Relation::Open].into_iter().map(|relation| {
                let input = fixture.input(relation, index as u16);
                verify_relation(&input.profile, &input.statement, &input.witness).unwrap();
                json!({"relation":relation as u8,"journal":hex::encode(input.statement.abi_bytes()),"journalHash":hex::encode(input.statement.digest())})
            }).collect();
            json!({"spec":fixture.order.unsigned.windows[index],"leafHash":hex::encode(fixture.order.unsigned.windows[index].digest()),
                "ciphertext":hex::encode(&fixture.encrypted[index].ciphertext),"capsule":hex::encode(&fixture.encrypted[index].capsule),"statements":relations})
        }).collect();
        let key = fixture.input(Relation::KeyBinding, 0);
        let order = &fixture.order.unsigned;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schemaHash":hex::encode(schema_hash()),"profile":fixture.profile,"profileAbi":hex::encode(fixture.profile.abi_bytes()?),
                "profileId":hex::encode(fixture.profile.digest()?),"order":fixture.order,
                "orderId":hex::encode(order.terms.order_id()?),"preTermsHash":hex::encode(order.terms.digest()?),
                "manifestRoot":hex::encode(order.manifest_root()?),"manifestAbi":hex::encode(order.manifest_abi_bytes()?),"quoteDigest":hex::encode(order.quote_digest()?),
                "keyBindingPayload":hex::encode(order.terms.binding.signing_payload()?),
                "quotePayload":hex::encode(order.quote_signing_payload()?),"sourcePayload":hex::encode(order.source_signing_payload()?),
                "keyJournal":hex::encode(key.statement.abi_bytes()),"windows":windows,
            }))?
        );
    } else {
        return Err("unknown fixture command".into());
    }
    Ok(())
}
