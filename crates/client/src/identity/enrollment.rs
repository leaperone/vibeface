use super::*;
const FILE: &str = "enrollment.json";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    device_id: String,
    entry: Entry,
    link: Zeroizing<String>,
    name: String,
    join_id: String,
    nonce: Zeroizing<String>,
    admin: Option<(String, String, String)>,
    request: Vec<u8>,
    sent: bool,
    joined: Option<String>,
    terminal: bool,
}
impl Journal {
    fn endpoint(&self) -> Result<Endpoint> {
        let ticket = Ticket::from_link(&self.link)?;
        let addresses = ticket
            .entry
            .addrs
            .iter()
            .filter(|addr| matches!(&addr.host, vibeface_secure::link::Host::Ip(_)))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if self.entry.origin.is_empty() {
            ensure!(
                !addresses.is_empty(),
                "invitation needs a numeric direct address or a Gateway origin"
            );
            Ok(Endpoint::Lan {
                node_id: self.entry.node_id.clone(),
                addresses,
            })
        } else if addresses.is_empty() {
            Ok(Endpoint::Gateway(self.entry.clone()))
        } else {
            Ok(Endpoint::Preferred {
                gateway: self.entry.clone(),
                addresses,
            })
        }
    }
    fn password(key: &ClientKey) -> Zeroizing<String> {
        Zeroizing::new(key.identity.secret_hex())
    }
    fn load(key: &ClientKey) -> Result<Self> {
        let encrypted =
            privatefs::read_private_bounded(&key.dir.join(FILE), MAX_FILE, "pending enrollment")?;
        let plain = Zeroizing::new(recovery_open(&encrypted, &Self::password(key))?);
        let journal: Self = serde_json::from_slice(&plain)?;
        ensure!(
            journal.device_id == key.device_id(),
            "enrollment belongs to another client"
        );
        let ticket = Ticket::from_link(&journal.link)?;
        ensure!(
            ticket.entry_id() == journal.entry.node_id
                && invite_id(&ticket.invite_hash())? == journal.join_id,
            "enrollment ticket identity mismatch"
        );
        Ok(journal)
    }
    fn bytes(&self, key: &ClientKey) -> Result<Vec<u8>> {
        let plain = Zeroizing::new(serde_json::to_vec(self)?);
        Ok(recovery_seal(&plain, &Self::password(key))?)
    }
    fn save(&self, key: &ClientKey) -> Result<()> {
        privatefs::write_atomic(&key.dir.join(FILE), &self.bytes(key)?)
    }

    fn withdrawal_receipt(&self, key: &ClientKey, reply: &Value) -> Result<Value> {
        ensure!(
            reply["join_id"] == self.join_id
                && reply["invite_id"] == self.join_id
                && reply["device_id"] == key.device_id()
                && reply["commit"] == join_commit(key.device_id(), &self.nonce)?,
            "withdrawal reply belongs to another enrollment; original request retained"
        );
        let status = text(reply, "status")?;
        ensure!(
            matches!(status, "withdrawn" | "expired" | "rejected" | "joined"),
            "withdrawal is not confirmed; original request retained"
        );
        // A joined response reports the approval race. It does not install membership or
        // expose the request's nonces; normal enrollment still verifies the signed state.
        Ok(json!({"join_id":self.join_id,"status":status}))
    }
}

/// Joining preserves the original nonce and confirming manager across process restarts.
pub struct Enrollment<'a> {
    session: Session<'a>,
    pub join_id: String,
    journal: Journal,
}
pub enum Progress {
    Waiting,
    Code(String),
    Joined,
}
impl<'a> Enrollment<'a> {
    pub fn pending(key: &ClientKey) -> bool {
        key.dir.join(FILE).exists()
    }
    /// Withdraws only the saved original request, proving this client's own identity.
    /// A lost reply retains the journal so the same withdrawal can be read back by retrying.
    /// If approval won the race, reports `joined` without accepting unverified membership.
    pub async fn withdraw_pending(key: &ClientKey) -> Result<Value> {
        ensure!(
            Self::pending(key),
            "no saved enrollment; there is no original join ID to withdraw"
        );
        let mut journal = Journal::load(key)?;
        ensure!(
            journal.sent,
            "the saved enrollment was never submitted; there is no remote join to withdraw"
        );
        let mut session = key.connect_endpoint(&journal.endpoint()?).await?;
        let reply = session
            .call("join_withdraw", json!({"join_id":journal.join_id}))
            .await;
        session.connection.close();
        let receipt = journal.withdrawal_receipt(key, &reply?)?;
        if receipt["status"] != "joined" {
            journal.terminal = true;
            journal.save(key)?;
        }
        Ok(receipt)
    }
    pub async fn start(key: &'a ClientKey, origin: &str, link: &str, name: &str) -> Result<Self> {
        if Self::pending(key) {
            let pending = Journal::load(key)?;
            ensure!(
                pending.entry.origin == origin
                    && pending.link.as_str() == link
                    && pending.name == name,
                "another enrollment is pending; resume its original invitation"
            );
        } else {
            ensure!(
                key.network_id().is_none(),
                "client already belongs to a personal network"
            );
            vibeface_secure::membership::validate_name(name)?;
            let ticket = Ticket::from_link(link)?;
            if !origin.is_empty() {
                crate::connection::client_url(origin)?;
            } else {
                ensure!(
                    ticket
                        .entry
                        .addrs
                        .iter()
                        .any(|addr| matches!(&addr.host, vibeface_secure::link::Host::Ip(_))),
                    "invitation has no direct address; choose a Gateway origin"
                );
            }
            let nonce = Zeroizing::new(hex::encode(rand::random::<[u8; 32]>()));
            let join_id = invite_id(&ticket.invite_hash())?;
            let request = serde_json::to_vec(
                &json!({"id":format!("join-{join_id}"),"method":"join","params":{"secret":hex::encode(ticket.secret),"public_key":key.device_id(),"kind":"client","name":name,"commit":join_commit(key.device_id(),&nonce)?}}),
            )?;
            let journal = Journal {
                device_id: key.device_id().into(),
                entry: Entry {
                    origin: origin.into(),
                    node_id: ticket.entry_id(),
                },
                link: Zeroizing::new(link.into()),
                name: name.into(),
                join_id,
                nonce,
                admin: None,
                request,
                sent: false,
                joined: None,
                terminal: false,
            };
            durable::publish_new(&key.dir.join(FILE), &journal.bytes(key)?)?;
        }
        Self::resume(key).await
    }
    pub async fn resume(key: &'a ClientKey) -> Result<Self> {
        let mut journal = Journal::load(key)?;
        let mut session = key.connect_endpoint(&journal.endpoint()?).await?;
        if !journal.sent {
            let authenticated = session.call("authenticate", json!({})).await?;
            ensure!(
                authenticated["revoked"] != true,
                "client was revoked; this enrollment cannot proceed"
            );
            ensure!(
                authenticated["principal"].is_null(),
                "client is already a member"
            );
            journal.sent = true;
            journal.save(key)?;
            let request = session
                .connection
                .call_bytes(journal.request.clone())
                .await?;
            ensure!(
                request["join_id"] == journal.join_id && request["invite_id"] == journal.join_id,
                "Node returned another join request"
            );
        }
        Ok(Self {
            join_id: journal.join_id.clone(),
            session,
            journal,
        })
    }
    pub async fn poll(&mut self) -> Result<Progress> {
        let mut status = match self
            .session
            .call("join_status", json!({"join_id":self.join_id}))
            .await
        {
            Ok(status) => status,
            Err(error) => {
                let ticket = Ticket::from_link(&self.journal.link)?;
                if self
                    .session
                    .call("join_check", json!({"secret":hex::encode(ticket.secret)}))
                    .await
                    .is_ok_and(|reply| reply["invite"] == "expired")
                {
                    self.journal.terminal = true;
                    self.journal.save(self.session.key)?;
                }
                return Err(error);
            }
        };
        ensure!(
            status["join_id"] == self.join_id
                && status["invite_id"] == self.join_id
                && status["device_id"] == self.session.key.device_id()
                && status["commit"]
                    == join_commit(self.session.key.device_id(), &self.journal.nonce)?,
            "join status belongs to another request"
        );
        match status["status"].as_str() {
            Some("pending") => Ok(Progress::Waiting),
            Some("confirming") => {
                let admin = (
                    text(&status, "network_id")?.to_owned(),
                    text(&status, "admin_id")?.to_owned(),
                    text(&status, "admin_nonce")?.to_owned(),
                );
                let pinned = self.journal.admin.get_or_insert_with(|| admin.clone());
                ensure!(
                    *pinned == admin,
                    "the approving device changed; the original enrollment is retained"
                );
                self.journal.save(self.session.key)?;
                if status["joiner_nonce"].is_null() {
                    status = self
                        .session
                        .call(
                            "join_status",
                            json!({"join_id":self.join_id,"reveal":self.journal.nonce.as_str()}),
                        )
                        .await?;
                }
                ensure!(
                    status["invite_id"] == self.join_id
                        && status["admin_id"] == admin.1
                        && status["admin_nonce"] == admin.2
                        && status["joiner_nonce"] == self.journal.nonce.as_str(),
                    "revealed enrollment differs from the pinned commitment"
                );
                Ok(Progress::Code(short_code(
                    &admin.0,
                    text(&status, "invite_id")?,
                    self.session.key.device_id(),
                    &admin.1,
                    &admin.2,
                    &self.journal.nonce,
                )?))
            }
            Some("joined") => {
                let state: NetworkState = serde_json::from_value(status["state"].take())?;
                let network = text(&status, "network_id")?.to_owned();
                ensure!(
                    state.genesis.network_id == network
                        && self
                            .journal
                            .admin
                            .as_ref()
                            .is_some_and(|(pinned, ..)| *pinned == network),
                    "joined network differs from the confirmed enrollment"
                );
                state.project()?.client(self.session.key.device_id())?;
                self.journal.joined = Some(network);
                self.journal.save(self.session.key)?;
                Ok(Progress::Joined)
            }
            Some("expired" | "rejected" | "withdrawn") => {
                self.journal.terminal = true;
                self.journal.save(self.session.key)?;
                bail!("enrollment ended; the original request is retained for review")
            }
            _ => bail!("enrollment has no recognized status; original request retained"),
        }
    }
    pub fn endpoint(&self) -> Endpoint {
        self.session.entry.clone()
    }
    pub fn node_link(&self) -> Result<String> {
        Ok(Ticket::from_link(&self.journal.link)?.entry.to_string())
    }
    pub fn finish(self) -> Result<(String, Entry)> {
        let network = self
            .journal
            .joined
            .context("enrollment has not been confirmed joined")?;
        Ok((network, self.journal.entry))
    }
    pub fn discard_finished(key: &ClientKey) -> Result<()> {
        let journal = Journal::load(key)?;
        ensure!(
            journal.terminal,
            "an uncertain or active enrollment cannot be discarded"
        );
        std::fs::remove_file(key.dir.join(FILE))?;
        privatefs::sync_dir(&key.dir)
    }
}
pub(super) fn clear_joined(key: &ClientKey, network: &str) -> Result<()> {
    if !Enrollment::pending(key) {
        return Ok(());
    }
    let journal = Journal::load(key)?;
    if journal.joined.as_deref() == Some(network) {
        std::fs::remove_file(key.dir.join(FILE))?;
        privatefs::sync_dir(&key.dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(key: &ClientKey, entry: Entry) -> Result<Journal> {
        let ticket = Ticket::new(NodeLink::new(&entry.node_id, vec![], None)?, [7; 32])?;
        Ok(Journal {
            device_id: key.device_id().into(),
            entry,
            link: Zeroizing::new(ticket.encode()?),
            name: "fixture client".into(),
            join_id: invite_id(&ticket.invite_hash())?,
            nonce: Zeroizing::new("12".repeat(32)),
            admin: None,
            request: vec![],
            sent: true,
            joined: None,
            terminal: false,
        })
    }

    fn reply(key: &ClientKey, journal: &Journal, status: &str) -> Result<Value> {
        Ok(json!({
            "join_id":journal.join_id,
            "invite_id":journal.join_id,
            "device_id":key.device_id(),
            "commit":join_commit(key.device_id(), &journal.nonce)?,
            "status":status,
            "admin_nonce":"fixture manager nonce",
            "joiner_nonce":journal.nonce.as_str(),
        }))
    }

    #[test]
    fn withdrawal_receipts_bind_the_original_request_and_omit_nonces() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("withdraw-receipt-{}", uuid::Uuid::new_v4()));
        let key = ClientKey::create(&dir, "fixture client passphrase")?;
        let journal = journal(
            &key,
            Entry {
                origin: "http://127.0.0.1:9".into(),
                node_id: Identity::generate()?.device_id(),
            },
        )?;
        for status in ["withdrawn", "expired", "rejected", "joined"] {
            assert_eq!(
                journal.withdrawal_receipt(&key, &reply(&key, &journal, status)?)?,
                json!({"join_id":journal.join_id,"status":status})
            );
        }
        for field in ["join_id", "invite_id", "device_id", "commit"] {
            let mut wrong = reply(&key, &journal, "withdrawn")?;
            wrong[field] = "another request".into();
            assert!(journal.withdrawal_receipt(&key, &wrong).is_err());
        }
        for status in ["pending", "confirming", "unknown"] {
            assert!(
                journal
                    .withdrawal_receipt(&key, &reply(&key, &journal, status)?)
                    .is_err()
            );
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn withdrawal_requires_a_saved_submitted_request_without_creating_one() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("withdraw-unsent-{}", uuid::Uuid::new_v4()));
        let key = ClientKey::create(&dir, "fixture client passphrase")?;
        assert!(
            Enrollment::withdraw_pending(&key)
                .await
                .unwrap_err()
                .to_string()
                .contains("no saved enrollment")
        );
        assert!(!Enrollment::pending(&key));
        let mut journal = journal(
            &key,
            Entry {
                origin: "http://127.0.0.1:9".into(),
                node_id: Identity::generate()?.device_id(),
            },
        )?;
        journal.sent = false;
        journal.save(&key)?;
        let original = std::fs::read(dir.join(FILE))?;
        assert!(
            Enrollment::withdraw_pending(&key)
                .await
                .unwrap_err()
                .to_string()
                .contains("never submitted")
        );
        assert_eq!(std::fs::read(dir.join(FILE))?, original);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn lost_withdrawal_reply_retains_the_original_id_and_reports_an_approval_race()
    -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use vibeface_secure::Handshake;

        for status in ["withdrawn", "joined"] {
            let dir = std::env::temp_dir().join(format!("withdraw-retry-{}", uuid::Uuid::new_v4()));
            let key = ClientKey::create(&dir, "fixture client passphrase")?;
            let node = Identity::generate()?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let journal = journal(
                &key,
                Entry {
                    origin: format!("http://{}", listener.local_addr()?),
                    node_id: node.device_id(),
                },
            )?;
            journal.save(&key)?;
            let original = std::fs::read(dir.join(FILE))?;
            let original_id = journal.join_id.clone();
            let expected_device = key.device_id().to_owned();
            let expected_params = json!({"join_id":original_id});
            let result = reply(&key, &journal, status)?;
            let serving = tokio::spawn(async move {
                for attempt in 0..2 {
                    let (stream, _) = listener.accept().await?;
                    let mut socket = tokio_tungstenite::accept_async(stream).await?;
                    assert!(matches!(
                        socket.next().await.context("missing preface")??,
                        Message::Text(_)
                    ));
                    let mut handshake = Handshake::signed(false, &node, None, PROLOGUE)?;
                    handshake.read(
                        &socket
                            .next()
                            .await
                            .context("missing handshake")??
                            .into_data(),
                    )?;
                    socket
                        .send(Message::Binary(handshake.write(&[])?.into()))
                        .await?;
                    handshake.read(
                        &socket
                            .next()
                            .await
                            .context("missing client proof")??
                            .into_data(),
                    )?;
                    assert_eq!(
                        handshake.remote_id().as_deref(),
                        Some(expected_device.as_str())
                    );
                    let mut channel = handshake.finish()?;
                    loop {
                        let frame = socket.next().await.context("missing withdrawal")??;
                        let Some(plain) = channel.open_record(&frame.into_data())? else {
                            continue;
                        };
                        let request: Value = serde_json::from_slice(&plain)?;
                        assert_eq!(request["method"], "join_withdraw", "never resubmit join");
                        assert_eq!(request["params"], expected_params);
                        if attempt != 0 {
                            let response = json!({"id":request["id"],"result":result});
                            for record in channel.seal_message(&serde_json::to_vec(&response)?)? {
                                socket.send(Message::Binary(record.into())).await?;
                            }
                        }
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
            assert!(Enrollment::withdraw_pending(&key).await.is_err());
            assert_eq!(
                std::fs::read(dir.join(FILE))?,
                original,
                "unknown preserves the journal byte for byte"
            );
            assert_eq!(
                Enrollment::withdraw_pending(&key).await?,
                json!({"join_id":original_id,"status":status})
            );
            serving.await??;
            let saved = Journal::load(&key)?;
            assert_eq!(saved.join_id, original_id);
            assert_eq!(saved.terminal, status == "withdrawn");
            assert!(
                saved.joined.is_none() && key.network_id().is_none(),
                "a race never installs unverified membership"
            );
            if status == "joined" {
                assert_eq!(std::fs::read(dir.join(FILE))?, original);
            }
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
}
