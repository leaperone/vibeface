mod support;

use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};
use vibeface_delivery::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExperimentContext {
    escrow: Address,
    asset: Address,
    payer: Address,
    provider_wallet: Address,
    base_timestamp: u64,
}

fn read(path: &str, limit: u64) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("input must be a bounded regular file".into());
    }
    Ok(std::fs::read(path)?)
}

fn write_new(path: impl AsRef<Path>, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn experiment_fixture(
    context: ExperimentContext,
    original: Vec<u8>,
    profile: ProofProfile,
) -> std::result::Result<support::Fixture, Box<dyn std::error::Error>> {
    profile.validate()?;
    if original.len() != 4096
        || context.base_timestamp == 0
        || [
            context.escrow,
            context.asset,
            context.payer,
            context.provider_wallet,
        ]
        .contains(&[0; 20])
        || context.payer == context.provider_wallet
    {
        return Err("invalid small experiment context or file".into());
    }
    let mut terms = support::fixture_with_profile(&[4096], profile.clone())
        .order
        .unsigned
        .terms;
    let time = context.base_timestamp;
    terms.binding.escrow = context.escrow;
    terms.binding.valid_from = time;
    terms.binding.valid_until = time.checked_add(700).ok_or("timestamp overflow")?;
    terms.payer = context.payer;
    terms.provider_wallet = context.provider_wallet;
    terms.asset = context.asset;
    terms.whole_order_bonus = Amount::from_u64(5);
    terms.escrow_amount = Amount::from_u64(17);
    terms.quote_expires_at = time + 200;
    terms.hold_deadline = time + 300;
    terms.key_deadline = time + 400;
    terms.open_deadline = time + 500;
    terms.retain_until = time + 600;
    terms.priority_deadline = time + 350;
    terms.validate()?;
    Ok(support::fixture_from_terms(
        profile,
        terms,
        vec![SecretBytes::new(original)],
        Amount::from_u64(10),
        Amount::from_u64(2),
    ))
}

fn recover(hold: &ProofInput, capsule: &[u8]) -> vibeface_delivery::Result<RecoveredWindow> {
    verify_relation(&hold.profile, &hold.statement, &hold.witness)?;
    if let Witness::Hold {
        order,
        index,
        receiving_secret,
        ciphertext,
        ..
    } = &hold.witness
    {
        recover_window(order, *index, receiving_secret, ciphertext, capsule)
    } else {
        Err(Error("offline recovery requires a Hold witness"))
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|value| value == "experiment") {
        if args.len() != 5 {
            return Err("usage: fixture experiment <context.json> <4096.bin> <new-private-dir> <profile.json>".into());
        }
        let fixture = experiment_fixture(
            serde_json::from_slice(&read(&args[1], 4096)?)?,
            read(&args[2], 4096)?,
            serde_json::from_slice(&read(&args[4], 16384)?)?,
        )?;
        let output = Path::new(&args[3]);
        std::fs::DirBuilder::new().mode(0o700).create(output)?;
        for relation in [
            Relation::KeyBinding,
            Relation::Source,
            Relation::Hold,
            Relation::Open,
        ] {
            let input = fixture.input(relation, 0);
            verify_relation(&input.profile, &input.statement, &input.witness)?;
            write_new(
                output.join(format!("relation-{}.private.json", relation as u8)),
                &serde_json::to_vec(&input)?,
            )?;
        }
        write_new(
            output.join("vectors.json"),
            &serde_json::to_vec(&public_vectors(&fixture)?)?,
        )?;
        return Ok(());
    }
    if args.first().is_some_and(|value| value == "recover") {
        if args.len() != 4 {
            return Err(
                "usage: fixture recover <hold.json> <observed-E.bin> <new-output.bin>".into(),
            );
        }
        let hold: ProofInput = serde_json::from_slice(&read(&args[1], 1024 * 1024)?)?;
        let recovered = recover(&hold, &read(&args[2], CAPSULE_BYTES as u64)?)?;
        write_new(&args[3], &recovered.plaintext)?;
        println!(
            "{}",
            json!({"bytes":recovered.plaintext.len(),"sha256":hex::encode(recovered.sha256)})
        );
        return Ok(());
    }
    if args.len() < 3 {
        return Err("usage: fixture public <length> <count> [image_hex] [profile.json] | input <length> <count> [image_hex] <relation> <index> <private-output> [profile.json]".into());
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
        println!(
            "{}",
            serde_json::to_string_pretty(&public_vectors(&fixture)?)?
        );
    } else {
        return Err("unknown fixture command".into());
    }
    Ok(())
}

fn public_vectors(fixture: &support::Fixture) -> vibeface_delivery::Result<serde_json::Value> {
    let windows: Vec<_> = (0..fixture.encrypted.len()).map(|index| {
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
    Ok(json!({
        "schemaHash":hex::encode(schema_hash()),"profile":fixture.profile,"profileAbi":hex::encode(fixture.profile.abi_bytes()?),
        "profileId":hex::encode(fixture.profile.digest()?),"order":fixture.order,
        "orderId":hex::encode(order.terms.order_id()?),"preTermsHash":hex::encode(order.terms.digest()?),
        "manifestRoot":hex::encode(order.manifest_root()?),"manifestAbi":hex::encode(order.manifest_abi_bytes()?),"quoteDigest":hex::encode(order.quote_digest()?),
        "keyBindingPayload":hex::encode(order.terms.binding.signing_payload()?),
        "quotePayload":hex::encode(order.quote_signing_payload()?),"sourcePayload":hex::encode(order.source_signing_payload()?),
        "keyJournal":hex::encode(key.statement.abi_bytes()),"windows":windows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_file_context_and_hold_only_recovery() {
        let profile = support::fixture(&[1], [0x91; 32]).profile;
        let context = ExperimentContext {
            escrow: [0x81; 20],
            asset: [0x82; 20],
            payer: [0x83; 20],
            provider_wallet: [0x84; 20],
            base_timestamp: 1000,
        };
        let original = vec![0xa5; 4096];
        let fixture = experiment_fixture(context, original.clone(), profile).unwrap();
        for relation in [
            Relation::KeyBinding,
            Relation::Source,
            Relation::Hold,
            Relation::Open,
        ] {
            let input = fixture.input(relation, 0);
            verify_relation(&input.profile, &input.statement, &input.witness).unwrap();
            let journal = input.statement.abi_bytes();
            assert_eq!(Statement::from_bytes(journal).unwrap(), input.statement);
            let mut wrong_journal = journal.to_vec();
            let word_index = if relation == Relation::KeyBinding {
                5
            } else {
                30
            };
            wrong_journal[word_index * 32] ^= 1;
            assert_ne!(
                Statement::from_bytes(&wrong_journal).unwrap(),
                input.statement
            );
            let mut wrong_image = journal.to_vec();
            wrong_image[5 * 32] ^= 1;
            assert_eq!(
                Statement::from_bytes(&wrong_image).is_ok(),
                relation == Relation::KeyBinding
            );
        }
        let hold = fixture.input(Relation::Hold, 0);
        let capsule = &fixture.encrypted[0].capsule;
        assert_eq!(&*recover(&hold, capsule).unwrap().plaintext, &original);
        assert!(recover(&hold, &capsule[..79]).is_err());
        let mut corrupt = capsule.clone();
        corrupt[0] ^= 1;
        assert!(recover(&hold, &corrupt).is_err());
        assert!(recover(&fixture.input(Relation::Open, 0), capsule).is_err());
        assert_eq!(fixture.order.unsigned.terms.binding.escrow, [0x81; 20]);
        assert_eq!(fixture.encrypted[0].ciphertext.len(), 4164);
        assert_eq!(capsule.len(), 80);
    }
}
