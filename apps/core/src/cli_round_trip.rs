//! `vibeface client` and `vibeface call` against a real Node: every governance event the CLI
//! signs must pass the Node's own checks, and the local operator must never sign one.

use crate::{crypto::LocalIdentity, runtime::Runtime};
use anyhow::{Context, Result};
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};
use vibeface_control::client::{self, ClientKey, Enrollment, Entry, Progress};

const PASSPHRASE: &str = "synthetic client passphrase";
const PASSWORD: &str = "synthetic recovery password";

fn key(dir: &Path, name: &str) -> Result<ClientKey> {
    ClientKey::create(&dir.join(name), PASSPHRASE)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_client_governs_a_node_that_checks_every_signature() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vibeface-cli-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut config = crate::config::Config::local(public.local_addr()?, vec![], false);
    config.local_listen = local.local_addr()?;
    drop((public, local));
    let node_dir = dir.join("node");
    crate::config::private_dir(&node_dir)?;
    config.save(&node_dir)?;
    crate::config::private_write(
        &node_dir.join("identity.json"),
        &serde_json::to_vec(&LocalIdentity::generate())?,
    )?;
    crate::config::private_write(
        &node_dir.join("data-epoch.json"),
        &crate::config::data_epoch_marker(),
    )?;
    let capability =
        crate::store::Store::open(node_dir.join("node.sqlite3"))?.bootstrap_capability()?;
    let node = Arc::new(Runtime::open(&node_dir).await?);
    let entry = Entry {
        origin: node.config.public_url.clone(),
        node_id: node.identity.device_id()?,
    };
    let server = tokio::spawn(crate::api::serve(node.clone(), None));
    let result: Result<()> = async {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(node.config.listen).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let view = || async { node.membership_state().await?.project() };

        // The local operator reads and configures, but governance needs a client signature.
        let refused = crate::client::operator_call(&node, "invite", json!({}))
            .await
            .expect_err("the Node never signs governance");
        assert!(refused.to_string().contains("vibeface client enroll"));
        assert!(crate::client::operator_call(&node, "subscribe", json!({})).await.is_err());

        let mut admin = key(&dir, "admin")?;
        let admin_id = admin.device_id().to_owned();
        let recovery = dir.join("recovery.json");
        let created = client::create_network(
            &mut admin,
            entry.clone(),
            &capability.capability,
            "admin",
            Some("node"),
            &recovery,
            PASSWORD,
        )
        .await?;
        assert_eq!(created["principal"]["can_manage"], true);
        assert!(view().await?.member(&admin_id)?.manage);
        let settings = crate::client::operator_call(&node, "node_settings", json!({})).await?;
        let root = std::fs::canonicalize(&dir)?;
        crate::client::operator_call(
            &node,
            "configure_node",
            json!({"roots":[root],"manager":settings["settings"]["manager"]}),
        )
        .await?;
        let settings = crate::client::operator_call(&node, "node_settings", json!({})).await?;
        assert_eq!(settings["settings"]["roots"], json!([root]));

        // A second CLI enrolls through an invite; both sides see the same short code.
        let phone = key(&dir, "phone")?;
        let phone_id = phone.device_id().to_owned();
        let mut session = admin.connect(None).await?;
        let link = session.invite().await?;
        let mut enrollment = Enrollment::start(&phone, &entry.origin, &link, "phone").await?;
        assert!(matches!(enrollment.poll().await?, Progress::Waiting));
        let join_id = enrollment.join_id.clone();
        session.confirm_join(&join_id).await?;
        let Progress::Code(joiner_code) = enrollment.poll().await? else {
            anyhow::bail!("the joiner sees a code once confirmed")
        };
        let admin_code = session.join_code(&join_id).await?.context("revealed")?;
        assert_eq!(admin_code, joiner_code);
        session.grant_join(&join_id, false).await?;
        assert!(matches!(enrollment.poll().await?, Progress::Joined));
        let (network, _) = enrollment.finish()?;
        assert_eq!(network, view().await?.network_id);
        assert!(!view().await?.member(&phone_id)?.manage);

        // The user's own writes carry the client's attestation, as `vibeface bots` signs them.
        let (bot_id, scope) = ("b1".to_owned(), "c1".to_owned());
        let body = json!({"type":"bot_created","bot_id":bot_id,"name":"Docs","instructions":""});
        let owner = json!({"type":"conversation_owner","scheduler_node":entry.node_id,"epoch":1});
        session
            .rpc(
                "create_bot",
                json!({"bot_id":bot_id,"conversation_id":scope,"name":"Docs","instructions":"",
                    "scheduler_node":entry.node_id,"owner_attestation":session.attest(&scope, &owner)?,
                    "attestation":session.attest(&scope, &body)?}),
            )
            .await?;
        let primary = json!({"type":"primary_bot_set","conversation_id":scope});
        session
            .rpc(
                "set_primary_bot",
                json!({"conversation_id":scope,"attestation":session.attest("bots", &primary)?}),
            )
            .await?;
        let bots = crate::client::operator_call(&node, "rpc", json!({"method":"bots"})).await?;
        assert_eq!(bots["bots"][0]["bot_id"], "b1");
        assert!(!bots["primary"].is_null());
        let forged = json!({"type":"bot_deleted"});
        let mut attestation = session.attest(&scope, &forged)?;
        attestation["device_id"] = json!(phone_id);
        assert!(
            session
                .rpc("delete_bot", json!({"conversation_id":scope,"attestation":attestation}))
                .await
                .is_err(),
            "another device's name on this client's signature"
        );

        session.grant(&phone_id, Some("tablet"), Some(true)).await?;
        let member = view().await?.member(&phone_id)?;
        assert_eq!((member.name.as_str(), member.manage), ("tablet", true));
        session.revoke(&phone_id).await?;
        assert!(view().await?.revocation(&phone_id).is_some());

        // A recovery file starts a recovery; a manager stops one and approves the next.
        let mut rescuer = key(&dir, "rescuer")?;
        let rescuer_id = rescuer.device_id().to_owned();
        client::recover(
            &mut rescuer,
            entry.clone(),
            &recovery,
            PASSWORD,
            &dir.join("recovery-2.json"),
            "rescuer",
            None,
        )
        .await?;
        assert!(view().await?.pending.is_some());
        let mut session = admin.connect(None).await?;
        session.settle_recovery(false).await?;
        assert!(view().await?.pending.is_none());
        client::recover(
            &mut rescuer,
            entry.clone(),
            &recovery,
            PASSWORD,
            &dir.join("recovery-3.json"),
            "rescuer",
            None,
        )
        .await?;
        session.settle_recovery(true).await?;
        let view = view().await?;
        assert!(view.pending.is_none());
        assert!(view.member(&rescuer_id)?.manage);
        assert!(view.member(&admin_id)?.manage);
        Ok(())
    }
    .await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
