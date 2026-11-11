//! `vibeface client` and `vibeface call` against a real Node: every governance event the CLI
//! signs must pass the Node's own checks, and the local operator must never sign one.

use crate::{crypto::LocalIdentity, runtime::Runtime};
use anyhow::{Context, Result};
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};
use vibeface_client::{self as client, ClientKey, Enrollment, Entry, Progress};

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
        let plan = client::prepare_create(
            &admin,
            entry.clone(),
            &capability.capability,
            "admin",
            Some("node"),
            &recovery,
            PASSWORD,
        )
        .await?;
        assert!(recovery.exists());
        assert_eq!(plan.state(),client::IdentityOperationState::Prepared);
        assert!(!node.has_membership().await?,"preparing a backup never initializes the Node");
        drop(plan);
        let mut plan=client::IdentityPlan::load(&admin,PASSWORD)?;
        let created=plan.submit(&mut admin).await?;
        plan.reconcile(&mut admin).await?;
        assert_eq!(plan.state(),client::IdentityOperationState::Applied);
        plan.discard()?;
        assert_eq!(created["principal"]["can_manage"], true);
        assert!(view().await?.member(&admin_id)?.manage);
        // The native local viewer uses its own unenrolled key and the private launcher exchange.
        let viewer=Arc::new(key(&dir,"local-viewer")?);
        let local_entry=Entry {origin:format!("http://{}",node.config.local_listen),node_id:entry.node_id.clone()};
        let grant=client::LocalTarget::read(&node_dir,&local_entry)?.grant(viewer.device_id()).await?;
        let local_client=client::Client::open_local(viewer.clone(),grant,dir.join("local-outbox")).await?;
        assert_eq!(local_client.authority(),client::ClientAuthority::LocalSession);
        assert_eq!(viewer.network_id(),None,"local authorization never enrolls the key");
        assert!(local_client.read(client::ReadRequest::Home).await?["bots"].is_array());
        assert!(local_client.next_input_sequence("chat",&client::wire::InputTarget::BotMain {bot_id:"bot".into()}).await.is_err());
        assert!(local_client.submit(client::Action::DeleteBot {conversation_id:"chat".into()}).await.is_err());
        assert!(local_client.management_session().is_err());
        assert!(!local_client.can_control());
        assert!(local_client.submit(client::Action::StopReply {conversation_id:"chat".into(),trigger:"message:original".into()}).await.is_err());
        assert!(local_client.can_configure_node(),"private local owner retains Node settings authority");
        let local_root=std::fs::canonicalize(&dir)?;
        let configured=local_client.submit(client::Action::NodeSettings(client::NodeSettings {roots:vec![local_root.clone()],manager:None})).await?;
        assert_eq!(configured.state,client::OperationState::Accepted);
        assert_eq!(local_client.read(client::ReadRequest::NodeSettings).await?["settings"]["roots"],json!([local_root]));
        local_client.close();drop(local_client);
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
        let mut phone = key(&dir, "phone")?;
        let phone_id = phone.device_id().to_owned();
        let mut session = admin.connect(None).await?;
        let link = session.invite().await?;
        // The invite carries the Node's own node link, the one it shares.
        let shared = crate::client::operator_call(&node, "share_link", json!({})).await?;
        let shared = shared["link"].as_str().context("share link")?;
        assert_eq!(
            vibeface_secure::link::NodeLink::parse(shared)?.device_id(),
            entry.node_id
        );
        assert_eq!(
            vibeface_secure::join::Ticket::from_link(&link)?
                .entry
                .to_string(),
            shared
        );
        let mut enrollment = Enrollment::start(&phone, &entry.origin, &link, "phone").await?;
        assert!(matches!(enrollment.poll().await?, Progress::Waiting));
        let join_id = enrollment.join_id.clone();
        drop(enrollment);
        let mut enrollment=Enrollment::resume(&phone).await?;
        assert_eq!(enrollment.join_id,join_id,"restart retains the original invite and nonce");
        session.confirm_join(&join_id).await?;
        let Progress::Code(joiner_code) = enrollment.poll().await? else {
            anyhow::bail!("the joiner sees a code once confirmed")
        };
        let admin_code = session.join_code(&join_id).await?.context("revealed")?;
        assert_eq!(admin_code, joiner_code);
        drop(enrollment);
        let mut enrollment=Enrollment::resume(&phone).await?;
        let Progress::Code(reopened_code)=enrollment.poll().await? else {anyhow::bail!("restart lost confirmed short code")};
        assert_eq!(reopened_code,joiner_code);
        session.grant_join(&join_id, false).await?;
        assert!(matches!(enrollment.poll().await?, Progress::Joined));
        let (network, phone_entry) = enrollment.finish()?;
        phone.remember(&network,phone_entry)?;
        let phone_client=client::Client::open(Arc::new(phone),client::Endpoint::Gateway(entry.clone()),dir.join("phone-outbox")).await?;
        assert!(phone_client.can_control());
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
        // The actual native producer crosses Noise and Core's signature/shape boundary.
        // Defer proves storage/admission without scheduling a Provider turn.
        assert!(node.settings_snapshot().manager.is_none());
        for (message_id,report_results) in [("native-reports",true),("native-no-reports",false)] {
            let accepted=phone_client.submit(client::Action::Input {
                conversation_id:scope.clone(),
                input:client::wire::InputRequest {
                    message_id:message_id.into(),
                    route:client::wire::InputRoute {target:client::wire::InputTarget::BotMain {bot_id:bot_id.clone()},intent:client::wire::InputIntent::Defer,sequence:0,expected_revision:None},
                    text:"Native input saved without running a Provider".into(),attachments:vec![],report_results,
                },
            }).await?;
            assert_eq!(accepted.state,client::OperationState::Accepted,"real Core must accept native signed input");
            let receipt=accepted.result.context("native input acceptance receipt")?;
            assert_eq!(receipt["accepted"],json!(true));
            assert_eq!(receipt["message_id"],message_id);
            assert_eq!(receipt["state"],"deferred");
            assert_eq!(receipt["receipt"]["state"],"deferred");
            let network=network.clone();let scope=scope.clone();let id=message_id.to_owned();
            let (stored,queued,event,has_job)=node.store.call("native_input_reporting_acceptance",move|store| {
                let stored=store.workspace_input(&network,&id)?.context("native input was not stored")?;
                let queued=store.queued_workspace_input(&network,&id)?.context("native input queue projection missing")?;
                let event=store.event(&network,&stored.source_event)?.context("native signed event missing")?;
                let has_job=store.scheduler_job(&scope,&crate::scheduling::message_trigger(&id))?.is_some();
                Ok((stored,queued,event,has_job))
            }).await?;
            assert_eq!(stored.input.message_id,message_id);
            assert_eq!(stored.input.report_results,report_results);
            assert_eq!(stored.input.route.intent,client::wire::InputIntent::Defer);
            assert!(queued.deferred);assert!(!queued.cancelled);assert!(!has_job,"a deferred input must not create a Provider job");
            let canonical:serde_json::Value=serde_json::from_str(&event.canonical_body)?;
            assert_eq!(canonical["body"]["type"],"input_posted");
            assert_eq!(canonical["body"]["report_results"],json!(report_results));
            assert_eq!(canonical["body"]["intent"],"defer");
        }

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
        assert!(phone_client.reconnect(client::Endpoint::Gateway(entry.clone())).await.is_err());
        assert!(!phone_client.can_control());assert!(!phone_client.can_save_locally());
        assert!(phone_client.submit(client::Action::StopWork {conversation_id:scope.clone(),work_id:"old-work".into()}).await.is_err());
        phone_client.close();drop(phone_client);

        // A recovery file starts a recovery; a manager stops one and approves the next.
        let mut rescuer = key(&dir, "rescuer")?;
        let rescuer_id = rescuer.device_id().to_owned();
        let mut first_recovery=client::prepare_recovery(
            &rescuer,
            entry.clone(),
            &recovery,
            PASSWORD,
            &dir.join("recovery-2.json"),
            "rescuer",
            None,
        )
        .await?;
        assert!(view().await?.pending.is_none(),"saving the new backup does not rotate the root");
        first_recovery.submit(&mut rescuer).await?;
        first_recovery.reconcile(&mut rescuer).await?;
        assert_eq!(first_recovery.state(),client::IdentityOperationState::Recorded);
        let checkpoint_count=node.membership_state().await?.checkpoints.len();
        drop(first_recovery);
        let mut first_recovery=client::IdentityPlan::load(&rescuer,PASSWORD)?;
        first_recovery.submit(&mut rescuer).await?;
        assert_eq!(node.membership_state().await?.checkpoints.len(),checkpoint_count,"reopened recorded operation only reads its receipt");
        assert!(view().await?.pending.is_some());
        let mut session = admin.connect(None).await?;
        session.settle_recovery(false).await?;
        assert!(view().await?.pending.is_none());
        first_recovery.reconcile(&mut rescuer).await?;
        assert_eq!(first_recovery.state(),client::IdentityOperationState::Stopped);
        first_recovery.discard()?;
        let mut second_recovery=client::prepare_recovery(
            &rescuer,
            entry.clone(),
            &recovery,
            PASSWORD,
            &dir.join("recovery-3.json"),
            "rescuer",
            None,
        )
        .await?;
        second_recovery.submit(&mut rescuer).await?;
        session.settle_recovery(true).await?;
        second_recovery.reconcile(&mut rescuer).await?;
        assert_eq!(second_recovery.state(),client::IdentityOperationState::Applied);
        second_recovery.discard()?;
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
