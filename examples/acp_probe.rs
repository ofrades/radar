//! Spike: drive an ACP agent from radar's process model.
//!
//! Run against the fixture agent:
//!
//! ```text
//! cargo run --example acp_probe -- tests/fixtures/acp_fake_agent.py "hello"
//! ```
//!
//! Proves the pieces radar's daemon needs: spawn an ACP agent, initialize,
//! create a session, stream `session/update` notifications, and round-trip a
//! `session/request_permission` decision — all on a plain blocking thread via
//! `futures_lite`, no async runtime in the daemon.

use std::str::FromStr;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, ListSessionsRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let script = args.next().unwrap_or_else(|| {
        format!(
            "{}/tests/fixtures/acp_fake_agent.py",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let prompt = args
        .next()
        .unwrap_or_else(|| "hello from radar".to_string());
    let command = format!("python3 {script}");
    let agent = AcpAgent::from_str(&command)?;
    let cwd = std::env::current_dir()?;

    let result = futures_lite::future::block_on(async move {
        Client
            .builder()
            .name("radar-acp-probe")
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    match notification.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                println!("[chunk] {}", text.text);
                            }
                        }
                        other => println!("[update] {other:?}"),
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    println!(
                        "[permission] {}",
                        request.tool_call.fields.title.as_deref().unwrap_or("?")
                    );
                    // The probe always approves the first option.
                    let option = request
                        .options
                        .first()
                        .map(|option| option.option_id.clone());
                    let outcome = match option {
                        Some(id) => {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                        }
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                println!("[init] {:?}", init.agent_info);

                let new_session = connection
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;
                println!("[session] {}", session_id.0);

                let listing = connection
                    .send_request(ListSessionsRequest::new().cwd(Some(cwd.clone())))
                    .block_task()
                    .await?;
                for info in &listing.sessions {
                    println!(
                        "[list] {} {:?} {:?}",
                        info.session_id.0, info.title, info.cwd
                    );
                }

                let response = connection
                    .send_request(PromptRequest::new(
                        session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    ))
                    .block_task()
                    .await?;
                println!("[done] {:?}", response.stop_reason);
                Ok(())
            })
            .await
    });

    result.map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(())
}
