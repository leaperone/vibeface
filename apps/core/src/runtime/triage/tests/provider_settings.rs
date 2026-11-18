use super::super::test_support::{answer, main_input_fact, unanswered, with_node};
use super::super::*;
use crate::config::Config;
use crate::runtime::test_support::{bot, pi_fixture_log};

#[test]
fn pi_fixture_records_are_complete_ordered_and_ignore_unpublished_files() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vibeface-pi-records-{}", uuid::Uuid::new_v4()));
    config::private_dir(&dir)?;
    let result = (|| -> Result<()> {
        assert!(pi_fixture_log(&dir)?.is_empty());
        let records = dir.join("pi-log-records");
        config::private_dir(&records)?;
        std::fs::write(records.join(".unfinished.tmp"), b"{\"prompt\":")?;
        for (name, prompt) in [("0002.json", "second"), ("0001.json", "first")] {
            let temporary = records.join(format!(".{name}.tmp"));
            std::fs::write(&temporary, serde_json::to_vec(&json!({"prompt":prompt}))?)?;
            std::fs::rename(temporary, records.join(name))?;
        }
        assert_eq!(
            pi_fixture_log(&dir)?,
            vec![json!({"prompt":"first"}), json!({"prompt":"second"})]
        );
        let invalid = records.join("0003.json");
        std::fs::write(&invalid, b"{\"prompt\":\"incomplete")?;
        let error = pi_fixture_log(&dir).expect_err("a published partial record must fail");
        let message = format!("{error:#}");
        assert!(message.contains("0003.json") && message.contains("bytes"));
        assert!(
            !message.contains("incomplete"),
            "diagnostics do not include record contents"
        );
        std::fs::remove_file(invalid)?;
        std::fs::remove_dir_all(&records)?;
        std::fs::write(&records, b"a file is not a record directory")?;
        assert!(
            pi_fixture_log(&dir).is_err(),
            "directory I/O errors cannot become an empty log"
        );
        Ok(())
    })();
    match (result, std::fs::remove_dir_all(&dir)) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Pi record fixture cleanup also failed: {cleanup}")))
        }
        (Ok(()), Err(error)) => Err(error.into()),
        (result, Ok(())) => result,
    }
}

#[tokio::test]
async fn provider_paths_a_bot_proposes_are_written_only_after_the_user_allows() -> Result<()> {
    with_node(async |node, admin, dir| {
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        for id in ["m-1", "m-2", "m-3", "m-4", "m-5", "m-6", "partial", "later"] {
            main_input_fact(&node, &admin, "c", id).await?;
        }
        let codex = dir.join("fixture-codex");
        std::fs::create_dir_all(codex.join("sessions"))?;
        std::fs::create_dir_all(codex.join("archived"))?;
        let saved = || Config::load(&dir).map(|config| config.triage.codex_sessions);

        let asked = node
            .propose_provider_config(
                &network,
                "c",
                "message:m-1",
                json!({"codex_sessions": codex.join("sessions")}),
            )
            .await?;
        assert_eq!(asked.value["status"], "asked");
        assert_eq!(asked.receipt.approval, crate::tools::Approval::Pending);
        let question = asked.value["question_id"].as_str().unwrap().to_owned();
        let card = unanswered(&node).await?;
        assert_eq!(card.len(), 1);
        assert!(
            !card[0].to_string().contains(&*dir.to_string_lossy()),
            "the card names no full local path"
        );
        assert!(
            node.apply_provider_config(&network, "c", &question)
                .await
                .is_err(),
            "nothing is written before an answer"
        );
        assert_eq!(saved()?, None);

        answer(&node, &question, "不允许").await?;
        let again = node
            .propose_provider_config(
                &network,
                "c",
                "message:m-1",
                json!({"codex_sessions":codex.join("sessions")}),
            )
            .await?;
        assert_eq!(again.value["question_id"], question.as_str());
        assert_eq!(again.receipt.approval, crate::tools::Approval::Denied);
        let denied = node.apply_provider_config(&network, "c", &question).await?;
        assert_eq!(denied.value["status"], "declined");
        assert_eq!(
            denied.receipt,
            crate::tools::Execution::NotStarted.with(crate::tools::Approval::Denied)
        );
        assert_eq!(saved()?, None, "a declined proposal writes nothing");
        assert_eq!(
            node.triage.config().codex_sessions,
            Some(dir.join("providers/codex/sessions"))
        );

        let asked = node
            .propose_provider_config(
                &network,
                "c",
                "message:m-2",
                json!({"codex_sessions": codex.join("archived")}),
            )
            .await?;
        assert_eq!(asked.receipt.approval, crate::tools::Approval::Pending);
        let question = asked.value["question_id"].as_str().unwrap().to_owned();
        assert!(
            node.apply_provider_config(&network, "other", &question)
                .await
                .is_err(),
            "another chat cannot apply it"
        );
        answer(&node, &question, "允许").await?;
        let again = node
            .propose_provider_config(
                &network,
                "c",
                "message:m-2",
                json!({"codex_sessions":codex.join("archived")}),
            )
            .await?;
        assert_eq!(again.value["question_id"], question.as_str());
        assert_eq!(again.receipt.approval, crate::tools::Approval::Allowed);
        let applied = node.apply_provider_config(&network, "c", &question).await?;
        assert_eq!(
            applied.receipt,
            crate::tools::Execution::Applied.with(crate::tools::Approval::Allowed)
        );
        assert_eq!(applied.value["status"], "applied");
        assert_eq!(saved()?, Some(codex.join("archived")));
        assert_eq!(
            node.triage.config().codex_sessions,
            Some(codex.join("archived"))
        );
        assert_eq!(
            node.apply_provider_config(&network, "c", &question).await?,
            applied,
            "a retry reads the first outcome back"
        );
        assert_eq!(
            node.propose_provider_config(
                &network,
                "c",
                "message:m-3",
                json!({"codex_sessions": codex.join("archived")})
            )
            .await?
            .value["status"],
            "unchanged"
        );
        assert!(
            node.propose_provider_config(
                &network,
                "c",
                "message:m-4",
                json!({"codex_sessions": "relative"})
            )
            .await
            .is_err(),
            "only absolute paths that exist are proposed"
        );

        let projects = dir.join("projects");
        std::fs::create_dir_all(&projects)?;
        let projects = projects.canonicalize()?;
        let asked = node
            .propose_provider_config(&network, "c", "message:m-5", json!({"roots": [projects]}))
            .await?;
        assert_eq!(asked.receipt.approval, crate::tools::Approval::Pending);
        let question = asked.value["question_id"].as_str().unwrap().to_owned();
        assert!(!node.settings_snapshot().roots.contains(&projects));
        answer(&node, &question, "允许").await?;
        assert_eq!(
            node.apply_provider_config(&network, "c", &question)
                .await?
                .value["roots_added"],
            1
        );
        assert!(node.settings_snapshot().roots.contains(&projects));
        assert!(
            Config::load(&dir)?.roots.contains(&projects),
            "the folder is saved"
        );
        assert_eq!(
            node.propose_provider_config(
                &network,
                "c",
                "message:m-6",
                json!({"roots": [projects]})
            )
            .await?
            .value["status"],
            "unchanged"
        );
        let gone = dir.join("temporary-project");
        std::fs::create_dir(&gone)?;
        let partial = node
            .propose_provider_config(
                &network,
                "c",
                "message:partial",
                json!({"codex_sessions":codex.join("sessions"),"roots":[gone]}),
            )
            .await?;
        let partial_id = partial.value["question_id"]
            .as_str()
            .context("partial proposal")?;
        answer(&node, partial_id, "允许").await?;
        std::fs::remove_dir(&gone)?;
        let first = node
            .apply_provider_config(&network, "c", partial_id)
            .await
            .unwrap_err();
        let failure = |error: &anyhow::Error| {
            crate::tools::ToolFailure::new(
                "apply_provider_config",
                &json!({"question_id":partial_id}),
                crate::tools::FailureStage::Execution,
                error,
            )
            .response
        };
        let first = failure(&first);
        assert_eq!(first["call_record"]["approval"], "allowed");
        assert_eq!(first["call_record"]["execution"], "unknown");
        assert_eq!(first["call_record"]["retry"], "none");
        assert_eq!(saved()?, Some(codex.join("sessions")));
        let later = node
            .propose_provider_config(
                &network,
                "c",
                "message:later",
                json!({"codex_sessions":codex.join("archived")}),
            )
            .await?;
        let later_id = later.value["question_id"]
            .as_str()
            .context("later proposal")?;
        answer(&node, later_id, "允许").await?;
        node.apply_provider_config(&network, "c", later_id).await?;
        node.provider_proposals.lock().await.clear();
        let repeated = node
            .apply_provider_config(&network, "c", partial_id)
            .await
            .unwrap_err();
        assert_eq!(failure(&repeated), first);
        assert_eq!(saved()?, Some(codex.join("archived")));
        Ok::<_, anyhow::Error>(())
    })
    .await
}
